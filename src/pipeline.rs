use anyhow::{anyhow, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::bench::BenchmarkRunner;
use crate::config::{get_repo_root, Harness, TaskType};
use crate::cost::ModelPricing;
use crate::report::archive::{RunArchiver, RunManifest, RunTokenUsage};
use crate::report::judge::RunJudge;
use crate::report::leaderboard::{BenchmarkRunResult, LeaderboardManager};
use crate::report::portal::PortalExporter;
use crate::sandbox::docker::{RunGuard, SandboxManager, PROXY_UPSTREAM_PORTS};

/// Upstream pool ports for a worker (base pool + worker*100 offset).
fn worker_upstream_ports(worker: u8) -> [u16; 3] {
    [
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[0], worker),
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[1], worker),
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[2], worker),
    ]
}
use crate::sandbox::omp::{AgentExecutionLimits, AgentRunSpec, OmpRunner, OmpSessionStats};
use crate::sandbox::opencode::{OpenCodeRunner, OPENCODE_VERSION};
use crate::sandbox::AgentRunner;
use crate::verifier::binproto::BinprotoVerifier;
use crate::verifier::compliance::ComplianceChecker;
use crate::verifier::http::HttpVerifier;
use crate::verifier::pdf::PdfVerifier;
use crate::verifier::proxy::ProxyVerifier;
use crate::verifier::pubsub::PubsubVerifier;
use crate::verifier::redis::RedisVerifier;
use crate::verifier::wal::WalVerifier;

/// Initial-attempt turn cap when feedback repair is enabled: looping models
/// get verification feedback early instead of burning the full budget here.
const INITIAL_TURN_CAP_WITH_FEEDBACK: u32 = 50;
/// Per-round turn cap for surgical repair rounds (bounded by pool remainder).
const REPAIR_ROUND_TURN_CAP: u32 = 25;

/// Shared-pool allowance for one repair round. `Uncapped` (max_turns == 0)
/// preserves the old unlimited behavior — it must not become a silent cap.
#[derive(Debug, PartialEq, Eq)]
enum RepairAllowance {
    /// Pool remainder is zero (including over budget via saturating math):
    /// break without stopping containers, writing reports, or recording
    /// a round that never ran.
    Exhausted,
    Uncapped,
    Capped(u32),
}

/// Pure pool math for a repair round, so the exhausted path is unit-tested
/// instead of only live-observed: a session whose stats extraction fails
/// reports 0 steps, and the pool must still account honestly.
fn repair_round_allowance(global_max: u32, steps_taken: u32) -> RepairAllowance {
    if global_max == 0 {
        return RepairAllowance::Uncapped;
    }
    let left = global_max.saturating_sub(steps_taken);
    if left == 0 {
        RepairAllowance::Exhausted
    } else {
        RepairAllowance::Capped(left.min(REPAIR_ROUND_TURN_CAP))
    }
}

/// Initial-attempt cap: checkpoint at 50 (or max_turns) when feedback repair
/// is on, full budget otherwise, unlimited when max_turns == 0.
/// Campaign perf-gate ratio: the perf ticket requires >= 1.3x the
/// candidate's own genesis-stage GET throughput, measured with the same
/// bench method (median of the reps) on both sides.
pub const PERF_GATE_RATIO: f64 = 1.3;

/// Pure perf-gate decision: `(ok, human-readable detail)`. A missing
/// baseline (single-shot runs, no genesis measurement) skips the gate
/// rather than failing it; a present baseline with no measurement fails —
/// a candidate that cannot produce a throughput number does not pass a
/// throughput gate.
pub fn perf_gate_verdict(base: Option<f64>, measured: Option<f64>) -> (bool, String) {
    match (base, measured) {
        (Some(b), Some(m)) if b > 0.0 => (
            m >= b * PERF_GATE_RATIO,
            format!(
                "measured {m:.0} req/sec vs genesis {b:.0} req/sec (ratio {:.2}x, gate {:.2}x)",
                m / b,
                PERF_GATE_RATIO
            ),
        ),
        (Some(b), None) if b > 0.0 => (
            false,
            format!(
                "no candidate throughput measured (genesis baseline {b:.0} req/sec needs {:.0} req/sec)",
                b * PERF_GATE_RATIO
            ),
        ),
        _ => (
            true,
            "no genesis baseline recorded; throughput gate skipped".to_string(),
        ),
    }
}

fn initial_turn_cap(max_turns: u32, feedback_rounds: u32, reserve_repair: bool) -> Option<u32> {
    if max_turns == 0 {
        None
    } else if feedback_rounds > 0 {
        let mut cap = std::cmp::min(max_turns, INITIAL_TURN_CAP_WITH_FEEDBACK);
        if reserve_repair {
            // Campaign tickets must leave room for at least one repair
            // attempt: the shared pool IS max_turns, and the live canary
            // showed a 25-turn ticket consuming 25/25 on the initial
            // attempt — the repair round never fired and a compile error
            // went unfixed. Reserve a quarter of the pool, but at least 12
            // turns and never more than half the pool.
            //
            // D6(b) amendment 2026-10-14 (floor 8 -> 12): three cells
            // showed an 8-turn repair cannot recover an interrupted
            // refactor. ling run 4 fixed two compile errors and hit the
            // limit right after (2-4 more turns would have reached a
            // build); ling run 5 spent 7 turns surveying and its single
            // edit landed at the limit; the inkling probe never reached an
            // edit at all. The marginal value of initial turns for models
            // that leave uncompilable code is low — they do not finish at
            // 17-30 either — while repair turns start from a precise
            // failure report on a near-complete file. Shifting up to 4
            // turns from initial to repair is therefore net-positive for
            // the observed failure class (see TODO_CAMPAIGNS grid log).
            let reserve = (max_turns / 4).max(12).min(max_turns / 2).max(1);
            cap = cap.min(max_turns.saturating_sub(reserve)).max(1);
        }
        Some(cap)
    } else {
        Some(max_turns)
    }
}

pub trait PipelineLogger: Send + Sync {
    fn log(&self, msg: &str);
}

struct BufferingPipelineLogger {
    inner: Arc<dyn PipelineLogger>,
    buffer: std::sync::Mutex<Vec<String>>,
    cancel_flag: Option<Arc<AtomicBool>>,
}

impl BufferingPipelineLogger {
    fn new(inner: Arc<dyn PipelineLogger>, cancel_flag: Option<Arc<AtomicBool>>) -> Self {
        Self {
            inner,
            buffer: std::sync::Mutex::new(Vec::new()),
            cancel_flag,
        }
    }

    fn get_logs(&self) -> String {
        self.buffer.lock().unwrap().join("\n")
    }
}

impl PipelineLogger for BufferingPipelineLogger {
    fn log(&self, msg: &str) {
        if let Some(ref cf) = self.cancel_flag {
            if cf.load(Ordering::SeqCst)
                && !msg.contains("[USER ACTION]")
                && !msg.contains("[DONE]")
                && !msg.contains("[STOP]")
                && !msg.contains("[PIPELINE] Run cancelled")
                && !msg.contains("[PIPELINE] Cancellation")
            {
                return;
            }
        }
        self.inner.log(msg);
        let ts = Utc::now().format("%H:%M:%S").to_string();
        self.buffer
            .lock()
            .unwrap()
            .push(format!("[{}] {}", ts, msg));
    }
}

fn hash_seed(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", (hash ^ (hash >> 32)) as u32)
}

/// A trial with zero model tokens and an empty workspace is not a
/// measurement: no model call ever landed and nothing runnable exists, so a
/// 0% here is a harness/provider artifact, never a model verdict. Defense
/// in depth for agent paths that can end `Ok` without the model running
/// (live: opencode free-tier quota retries surfaced as `failed_tests` with
/// 0 tokens and ranked 0%). Eval-only runs have no agent by design; a run
/// with any real output keeps its own status.
fn is_zero_output_non_result(eval_only: bool, total_tokens: u64, workspace_files: usize) -> bool {
    !eval_only && total_tokens == 0 && workspace_files == 0
}

/// CI-stage wall-clock cap for one `./test.sh` run.
const TEST_RUN_TIMEOUT_SECS: u64 = 180;

/// Last `n` bytes of tool output, never splitting a UTF-8 char.
fn output_tail(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.len() <= n {
        return t.to_string();
    }
    let mut cut = t.len() - n;
    while cut < t.len() && !t.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &t[cut..])
}

fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git {:?}: {e}", args))?;
    if !out.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `stage N: ...` commit hash from `git log --format=%H %s` output.
fn parse_stage_commit(log: &str, stage: u32) -> Option<String> {
    let needle = format!("stage {stage}:");
    log.lines()
        .find(|l| l.contains(&needle))
        .and_then(|l| l.split_whitespace().next())
        .map(|h| h.to_string())
}

/// Total line of `go tool cover -func` ("total: (statements) 87.5%").
fn parse_go_coverage_total(out: &str) -> Option<f64> {
    out.lines()
        .rev()
        .find(|l| l.trim_start().starts_with("total:"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|p| p.trim_end_matches('%').parse::<f64>().ok())
}

/// Self-reported coverage line ("coverage: 87.5%").
fn parse_self_coverage(text: &str) -> Option<f64> {
    for line in text.lines() {
        let low = line.to_ascii_lowercase();
        let Some(pos) = low.find("coverage") else {
            continue;
        };
        let rest = &line[pos..];
        let num: String = rest
            .chars()
            .skip_while(|c| !(c.is_ascii_digit() || *c == '.'))
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            if (0.0..=100.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

/// Stage 9 (tests & CI): the ticket ships `./test.sh`; this checks it does
/// what a CI gate is for:
///   1. green + deterministic: two offline runs, exit 0, bounded time;
///   2. coverage >= 80%: Go measured by the harness
///      (`go test -coverprofile` + `go tool cover -func`); other languages
///      read a self-reported `coverage.txt` line ("coverage: NN.N%");
///   3. sensitivity: reverting the stage-1 (expiry) diff in a scratch copy
///      must make `./test.sh` FAIL — a suite that stays green without the
///      feature is decoration, not tests.
fn run_tests_ci_blocking(
    sandbox: SandboxManager,
    workdir: PathBuf,
    logger: Arc<dyn PipelineLogger>,
) -> crate::verifier::StageResult {
    let name = "Stage 9: tests & CI (coverage + sensitivity)".to_string();
    let fail = |e: String| crate::verifier::StageResult {
        stage: 9,
        name: name.clone(),
        passed: false,
        error: Some(e),
    };
    if !workdir.join("test.sh").is_file() {
        return fail("./test.sh is missing at the repo root".to_string());
    }
    let run_suite = |dir: &Path| {
        sandbox.run_workspace_command(
            dir,
            "if [ -x ./test.sh ]; then ./test.sh; else bash ./test.sh; fi",
            TEST_RUN_TIMEOUT_SECS,
        )
    };
    for attempt in 1..=2u32 {
        match run_suite(&workdir) {
            Ok((true, _)) => logger.log(&format!(
                "  [TESTS] ./test.sh green (run {attempt}/2, offline, <= {TEST_RUN_TIMEOUT_SECS}s)"
            )),
            Ok((false, out)) => {
                return fail(format!(
                    "./test.sh exited non-zero (run {attempt}/2): {}",
                    output_tail(&out, 500)
                ))
            }
            Err(e) => return fail(format!("./test.sh could not run (run {attempt}/2): {e}")),
        }
    }
    let coverage = if workdir.join("go.mod").is_file() {
        match sandbox.run_workspace_command(
            &workdir,
            "go test ./... -coverprofile=/tmp/sdb-cov.out >/dev/null 2>&1; go tool cover -func=/tmp/sdb-cov.out | tail -n 1",
            TEST_RUN_TIMEOUT_SECS,
        ) {
            Ok((_, out)) => parse_go_coverage_total(&out),
            Err(e) => return fail(format!("coverage measurement failed: {e}")),
        }
    } else {
        std::fs::read_to_string(workdir.join("coverage.txt"))
            .ok()
            .and_then(|t| parse_self_coverage(&t))
    };
    let Some(cov) = coverage else {
        return fail(
            "no coverage signal: Go projects are measured by the harness via `go test -coverprofile`; other languages must write a `coverage: NN.N%` line into coverage.txt"
                .to_string(),
        );
    };
    logger.log(&format!("  [TESTS] coverage {cov:.1}% (gate 80%)"));
    if cov < 80.0 {
        return fail(format!("coverage {cov:.1}% is below the 80% gate"));
    }
    let Some(parent) = workdir.parent() else {
        return fail("workspace has no parent directory".to_string());
    };
    let scenario = parent.join(format!("s9-scenario-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scenario);
    if let Err(e) = crate::campaign::copy_dir_excluding_git(&workdir, &scenario) {
        let _ = std::fs::remove_dir_all(&scenario);
        return fail(format!("scratch copy failed: {e}"));
    }
    let revert = (|| -> Result<(), String> {
        let log = git_in(&workdir, &["log", "--format=%H %s"])?;
        let s0 = parse_stage_commit(&log, 0)
            .ok_or_else(|| "no `stage 0` commit in the workspace history".to_string())?;
        let s1 = parse_stage_commit(&log, 1)
            .ok_or_else(|| "no `stage 1` commit in the workspace history".to_string())?;
        let changed = git_in(&workdir, &["diff", "--name-only", &s0, &s1])?;
        for f in changed.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let target = scenario.join(f);
            match git_in(&workdir, &["show", &format!("{s0}:{f}")]) {
                Ok(content) => {
                    if let Some(p) = target.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::write(&target, content);
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&target);
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = revert {
        let _ = std::fs::remove_dir_all(&scenario);
        return fail(format!("sensitivity setup failed: {e}"));
    }
    let sens = run_suite(&scenario);
    let _ = std::fs::remove_dir_all(&scenario);
    match sens {
        Ok((false, _)) => logger.log(
            "  [TESTS] sensitivity ok: the suite goes red with the stage-1 (expiry) feature reverted",
        ),
        Ok((true, _)) => {
            return fail(
                "./test.sh stayed green with the stage-1 (expiry) feature reverted — the tests do not catch regressions"
                    .to_string(),
            )
        }
        Err(e) => return fail(format!("sensitivity run could not execute: {e}")),
    }
    crate::verifier::StageResult {
        stage: 9,
        name,
        passed: true,
        error: None,
    }
}

/// Stage 13 runner: three-node HA (ticket 10, operator decision Q1).
/// Owns the whole cluster lifecycle: start ha1 master + ha2/ha3 replicas,
/// check replication + sync-ack, SIGKILL the master, require automatic
/// failover to exactly one survivor within T=10s holding every acked
/// write, re-point the old master at the new leader and require it to
/// rejoin as a replica and converge, then re-run the accumulated RESP
/// suite plus `./test.sh` against the current master.
async fn run_ha_stage(
    sandbox: &SandboxManager,
    workdir: &Path,
    ports: &[u16; 3],
    seed: &str,
    logger: &Arc<dyn PipelineLogger>,
) -> crate::verifier::StageResult {
    use crate::verifier::redis::RedisVerifier;
    use std::time::{Duration, Instant};

    let name = "Stage 13: 3-node HA (replication, failover, rejoin)";
    let fail = |e: String| crate::verifier::StageResult {
        stage: 13,
        name: name.to_string(),
        passed: false,
        error: Some(e),
    };
    let pass = || crate::verifier::StageResult {
        stage: 13,
        name: name.to_string(),
        passed: true,
        error: None,
    };
    const FAILOVER_T: Duration = Duration::from_secs(10);
    const REPL_VISIBLE: Duration = Duration::from_secs(3);

    let _ = sandbox.stop_candidate();
    if let Err(e) = sandbox.start_ha_nodes(workdir, ports, None) {
        sandbox.stop_ha_nodes(3);
        return fail(format!("cluster did not start: {e}"));
    }
    for p in ports {
        if !sandbox.wait_for_port(*p, 25).await {
            return fail(format!("HA node on port {p} did not open within 25s"));
        }
    }
    logger.log("  [HA] cluster up (ha1 master, ha2/ha3 replicas)");

    // Roles as launched.
    match RedisVerifier::role_of(ports[0]).await {
        Ok(r) if r == "master" => {}
        Ok(r) => return fail(format!("ha1 reports role {r:?}, want master")),
        Err(e) => return fail(format!("ROLE(ha1): {e}")),
    }
    for (i, p) in [(2usize, ports[1]), (3usize, ports[2])] {
        match RedisVerifier::role_of(p).await {
            Ok(r) if r == "replica" || r == "slave" => {}
            Ok(r) => return fail(format!("ha{i} reports role {r:?}, want replica")),
            Err(e) => return fail(format!("ROLE(ha{i}): {e}")),
        }
    }

    // Sync-ack + replication: every acked write must be readable on at
    // least one replica within 3s.
    let pairs = match RedisVerifier::write_probe_batch(ports[0], "pre", 6).await {
        Ok(p) => p,
        Err(e) => return fail(format!("aced writes on the master failed: {e}")),
    };
    let t0 = Instant::now();
    let mut visible;
    loop {
        let a = RedisVerifier::count_present(ports[1], &pairs)
            .await
            .unwrap_or(0);
        let b = RedisVerifier::count_present(ports[2], &pairs)
            .await
            .unwrap_or(0);
        visible = a.max(b);
        if visible >= pairs.len() || t0.elapsed() >= REPL_VISIBLE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if visible < pairs.len() {
        return fail(format!(
            "replication: {visible}/{} acked writes visible on a replica within 3s",
            pairs.len()
        ));
    }
    logger.log(&format!(
        "  [HA] {} acked writes replicated within {:.1}s",
        pairs.len(),
        t0.elapsed().as_secs_f64()
    ));

    // Automatic failover: SIGKILL the master; within T exactly one
    // survivor reports ROLE master and holds every acked write.
    if let Err(e) = sandbox.kill_ha_node(1) {
        return fail(format!("SIGKILL ha1: {e}"));
    }
    logger.log("  [HA] ha1 SIGKILLed; waiting for automatic failover (T=10s)...");
    let t0 = Instant::now();
    let mut leader: Option<(u8, u16)> = None;
    while t0.elapsed() < FAILOVER_T {
        for (i, p) in [(2u8, ports[1]), (3u8, ports[2])] {
            if let Ok(r) = RedisVerifier::role_of(p).await {
                if r == "master" {
                    leader = Some((i, p));
                    break;
                }
            }
        }
        if leader.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let Some((leader_idx, leader_port)) = leader else {
        return fail(
            "no survivor reported ROLE master within 10s (automatic failover missing)".to_string(),
        );
    };
    logger.log(&format!(
        "  [HA] ha{leader_idx} promoted in {:.1}s",
        t0.elapsed().as_secs_f64()
    ));
    let other_port = if leader_idx == 2 { ports[2] } else { ports[1] };
    match RedisVerifier::role_of(other_port).await {
        Ok(r) if r != "master" => {}
        Ok(r) => return fail(format!("two masters: the other survivor reports {r:?}")),
        Err(e) => return fail(format!("ROLE(other survivor): {e}")),
    }
    let held = match RedisVerifier::count_present(leader_port, &pairs).await {
        Ok(n) => n,
        Err(e) => return fail(format!("reading acked writes on the survivor: {e}")),
    };
    if held < pairs.len() {
        return fail(format!(
            "failover lost data: {held}/{} acked writes on the new master",
            pairs.len()
        ));
    }
    // New writes reach the other node within 3s.
    let newpairs = match RedisVerifier::write_probe_batch(leader_port, "post", 4).await {
        Ok(p) => p,
        Err(e) => return fail(format!("writes on the new master failed: {e}")),
    };
    let t0 = Instant::now();
    let mut visible;
    loop {
        visible = RedisVerifier::count_present(other_port, &newpairs)
            .await
            .unwrap_or(0);
        if visible >= newpairs.len() || t0.elapsed() >= REPL_VISIBLE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if visible < newpairs.len() {
        return fail(format!(
            "post-failover replication: {visible}/{} new writes visible on the other node within 3s",
            newpairs.len()
        ));
    }

    // Rejoin: the old master comes back pointed at the new leader; it must
    // return as a replica (never co-master) and converge.
    let leader_alias = format!("ha{leader_idx}");
    if let Err(e) = sandbox.respawn_ha_node(
        workdir,
        1,
        ports[0],
        Some((leader_alias.as_str(), leader_port)),
    ) {
        return fail(format!("old master rejoin launch failed: {e}"));
    }
    if !sandbox.wait_for_port(ports[0], 25).await {
        return fail("rejoined node did not open its port".to_string());
    }
    let t0 = Instant::now();
    let mut role_ok = false;
    while t0.elapsed() < FAILOVER_T {
        if let Ok(r) = RedisVerifier::role_of(ports[0]).await {
            if r == "replica" || r == "slave" {
                role_ok = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !role_ok {
        return fail("the restarted node did not return as a replica (co-master risk)".to_string());
    }
    let mut all: Vec<(String, String)> = pairs.clone();
    all.extend(newpairs.clone());
    let t0 = Instant::now();
    let mut converged;
    loop {
        converged = RedisVerifier::count_present(ports[0], &all)
            .await
            .unwrap_or(0);
        if converged >= all.len() || t0.elapsed() >= FAILOVER_T {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if converged < all.len() {
        return fail(format!(
            "rejoined node did not converge: {converged}/{} acked writes",
            all.len()
        ));
    }
    logger.log("  [HA] old master rejoined as a replica and converged");

    // Regression: accumulated RESP suite + ./test.sh against the current
    // master.
    let v = RedisVerifier::new(leader_port, Some(seed));
    let basic = v.run_all().await;
    if basic.pass_rate < 100.0 {
        let failed: Vec<String> = basic
            .stages
            .iter()
            .filter(|s| !s.passed)
            .map(|s| format!("{}: {}", s.stage, s.error.clone().unwrap_or_default()))
            .collect();
        return fail(format!(
            "accumulated RESP suite against the current master: {:.0}% ({})",
            basic.pass_rate,
            failed.join("; ")
        ));
    }
    if workdir.join("test.sh").is_file() {
        for attempt in 1..=2u32 {
            match sandbox.run_workspace_command(
                workdir,
                "if [ -x ./test.sh ]; then ./test.sh; else bash ./test.sh; fi",
                TEST_RUN_TIMEOUT_SECS,
            ) {
                Ok((true, _)) => {}
                Ok((false, out)) => {
                    return fail(format!(
                        "./test.sh exited non-zero against the cluster (run {attempt}/2): {}",
                        output_tail(&out, 400)
                    ))
                }
                Err(e) => return fail(format!("./test.sh could not run (run {attempt}/2): {e}")),
            }
        }
    }
    logger.log("  [HA] accumulated RESP suite + ./test.sh green against the current master");
    pass()
}

#[derive(Clone)]
pub struct BenchmarkConfig {
    pub model: String,
    pub task: TaskType,
    pub effort: String,
    pub harness: Harness,
    pub max_turns: u32,
    pub budget_usd: f64,
    pub timeout_min: u64,
    pub api_key: Option<String>,
    pub workdir: PathBuf,
    pub eval_only: bool,
    pub run_id: Option<String>,
    pub save_results: bool,
    pub review_model: Option<String>,
    /// Pre-batch shared reference (median). When present, the trial skips its
    /// own ref measurement and scores against this denominator. Per-trial ref
    /// containers still start: the agent keeps its black-box oracle.
    pub batch_reference: Option<f64>,
    /// Parallel worker id (0..SDB_WORKERS). Offsets candidate/verifier ports
    /// by worker*100 so concurrent cells never collide. 0 = legacy ports.
    pub worker: u8,
    /// Number of feedback repair rounds within this trial (0 = single-shot).
    /// If initial verification fails, verifier diagnostics are fed back to the
    /// agent in the same workspace to surgically fix bugs.
    pub feedback_rounds: u32,
    /// Campaign ticket: prompt override, workspace continuity, and the
    /// acceptance subset that decides this stage's verdict. None for
    /// single-shot runs (behavior identical to before campaigns existed).
    pub campaign_stage: Option<crate::campaign::CampaignStageConfig>,
    /// Campaign genesis-stage candidate GET throughput (median of the bench
    /// reps, same method used for every stage). The perf stage's ticket
    /// requires >= PERF_GATE_RATIO x this value. None outside campaigns or
    /// when the genesis measurement is missing.
    pub baseline_rps: Option<f64>,
}

#[derive(Debug)]
pub struct PipelineOutput {
    pub manifest: RunManifest,
    pub benchmark_result: BenchmarkRunResult,
    pub agent_failed: bool,
    pub disqualified: bool,
    /// A harness fault degraded this run: the candidate workspace could not be
    /// inspected, or the candidate container would not start. The run still
    /// returns `Ok` because the agent and the verifier both completed, so
    /// without this flag it is indistinguishable from a model that genuinely
    /// produced nothing runnable - it is recorded as a 0% trial and counted in
    /// the pass@k denominator. Callers must treat it as a non-result.
    pub infra_error: bool,
}

/// Publish the WAL crash-recovery probe: 2000 MSGs on `recovery_probe`,
/// counting acks. The caller SIGKILLs the candidate next; `check_recovery`
/// then requires this state back.
///
/// Bounded by an overall 60s deadline so one wedged candidate cannot stall
/// the pipeline past the trial timeout. Acks collected before the deadline
/// are kept, never zeroed: check_recovery demands every acked message back,
/// so partial progress stays verifiable instead of discarded.
async fn publish_wal_probe(port: u16) -> anyhow::Result<usize> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let (rh, mut wh) = stream.into_split();
    let mut rd = tokio::io::BufReader::new(rh);
    let mut acks = 0usize;
    for i in 0..2000usize {
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                "WAL probe hit 60s overall deadline; keeping {} partial acks",
                acks
            );
            break;
        }
        if let Err(e) = wh
            .write_all(format!("PUB recovery_probe msg-{}\n", i).as_bytes())
            .await
        {
            tracing::warn!(
                "WAL probe write failed; keeping {} partial acks: {:#}",
                acks,
                e
            );
            break;
        }
        // Flush per write so a buffered but unsent prefix is never mistaken
        // for candidate side fsync loss after SIGKILL.
        if let Err(e) = wh.flush().await {
            tracing::warn!(
                "WAL probe flush failed; keeping {} partial acks: {:#}",
                acks,
                e
            );
            break;
        }
        let per_read = deadline
            .saturating_duration_since(std::time::Instant::now())
            .min(std::time::Duration::from_secs(5));
        let mut line = String::new();
        match tokio::time::timeout(per_read, rd.read_line(&mut line)).await {
            Ok(Ok(0)) => {
                tracing::warn!("WAL probe hit EOF; keeping {} partial acks", acks);
                break;
            }
            Ok(Ok(_)) if line.starts_with("+OK") => acks += 1,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    "WAL probe read failed; keeping {} partial acks: {:#}",
                    acks,
                    e
                );
                break;
            }
            Err(_) => {
                tracing::warn!("WAL probe read timed out; keeping {} partial acks", acks);
                break;
            }
        }
    }
    Ok(acks)
}

#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    pub pass_rate: f64,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub stage_results: Vec<crate::verifier::StageResult>,
    pub extra_robustness: Vec<crate::verifier::StageResult>,
    pub extra_throughput: Option<f64>,
    pub candidate_logs: String,
    pub disqualified: bool,
    pub disqualification_reason: Option<String>,
    pub infra_error: bool,
}

pub struct BenchmarkPipeline;

impl BenchmarkPipeline {
    pub fn format_feedback_prompt(
        task: TaskType,
        passed_stages: u32,
        total_stages: u32,
        stages: &[crate::verifier::StageResult],
        candidate_logs: &str,
    ) -> String {
        let mut out = String::new();
        out.push_str("# Automated Verification Failure Report\n\n");
        out.push_str(&format!(
            "Your previous implementation of the **{}** service passed {} of {} stages ({:.1}%).\n\n",
            task,
            passed_stages,
            total_stages,
            if total_stages > 0 {
                (passed_stages as f64 / total_stages as f64) * 100.0
            } else {
                0.0
            }
        ));
        out.push_str("## Verifier Test Results:\n");
        for s in stages {
            let status = if s.passed { "PASS" } else { "FAIL" };
            out.push_str(&format!("- [{status}] Stage {}: {}\n", s.stage, s.name));
            if let Some(ref err) = s.error {
                out.push_str(&format!("  * Diagnostic details: {}\n", err.trim()));
            }
        }
        let trimmed_logs = candidate_logs.trim();
        if !trimmed_logs.is_empty() {
            out.push_str("\n## Service Container Runtime Output:\n```\n");
            let lines: Vec<&str> = trimmed_logs.lines().collect();
            let start = lines.len().saturating_sub(60);
            for l in &lines[start..] {
                out.push_str(l);
                out.push('\n');
            }
            out.push_str("```\n");
        }
        out.push_str("\n## Directives for Surgical Bug Fixing:\n");
        out.push_str("1. DO NOT wipe your workspace or start from scratch. Your existing implementation files are in `/workspace`.\n");
        out.push_str("2. Carefully analyze the verifier test errors and container logs above.\n");
        out.push_str("3. Use `edit`, `write`, or `bash` to surgically fix the failing edge cases, protocol handling, or logic errors.\n");
        out.push_str(
            "4. Verify your fix locally by testing compilation and execution with `bash`.\n",
        );
        out.push_str("5. Ensure `./start.sh` or `Dockerfile` remains fully executable and properly starts the server.\n");
        out.push_str("6. When your surgical fix is complete, signal that you are ready for re-verification by creating `/workspace/READY` (e.g. `touch READY`), or conclude your session.\n");
        out
    }

    async fn run_agent_session(
        config: &BenchmarkConfig,
        run_id: &str,
        prompt: &str,
        limits: AgentExecutionLimits,
        sandbox: &SandboxManager,
        logger: &Arc<dyn PipelineLogger>,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<OmpSessionStats> {
        let logger_sub = logger.clone();
        let model_c = config.model.clone();
        let prompt_c = prompt.to_string();
        let work_path_buf = config.workdir.clone();
        let api_key_c = config.api_key.clone();
        let effort_c = config.effort.clone();
        let run_id_c = run_id.to_string();
        let net_name = sandbox.network_name();

        let cancel_flag_c = cancel_flag.clone();
        let cancel_flag_watcher = cancel_flag.clone();
        let sandbox_clone = SandboxManager::with_id(&run_id_c);
        let harness = config.harness;
        let task = config.task;
        let worker = config.worker;

        let mut blocking_handle = tokio::task::spawn_blocking(move || {
            let spec = AgentRunSpec {
                model: &model_c,
                prompt: &prompt_c,
                workdir: &work_path_buf,
                api_key: api_key_c.as_deref(),
                limits,
                effort: Some(&effort_c),
                network: Some(net_name.as_str()),
                task: Some(task),
                run_id: Some(&run_id_c),
                cancel_flag: cancel_flag_c,
                worker,
            };
            match harness {
                Harness::Omp => {
                    <OmpRunner as AgentRunner>::run_agent_with_logger(spec, move |line| {
                        logger_sub.log(&line);
                    })
                }
                Harness::Opencode => {
                    <OpenCodeRunner as AgentRunner>::run_agent_with_logger(spec, move |line| {
                        logger_sub.log(&line);
                    })
                }
            }
        });

        tokio::select! {
            res = &mut blocking_handle => match res {
                Ok(inner) => inner,
                Err(e) => Err(anyhow!("Task execution error: {}", e)),
            },
            _ = async {
                if let Some(ref cf) = cancel_flag_watcher {
                    loop {
                        if cf.load(Ordering::SeqCst) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                blocking_handle.abort();
                sandbox_clone.cleanup_instance();
                Err(anyhow!("Benchmark run was cancelled by user"))
            }
        }
    }

    // Nine params (task + workdir + sandbox + port + seed + agent flag +
    // worker + logger + accepted stages): the verifier fans out per task and
    // needs them all; bundling into a struct is churn without a second
    // caller. `accepted` is the campaign ticket's acceptance subset (None =
    // full suite, identical to pre-campaign behavior).
    #[allow(clippy::too_many_arguments)]
    fn run_verification<'a>(
        task: TaskType,
        workdir: &'a Path,
        sandbox: &'a SandboxManager,
        port: u16,
        verifier_seed: &'a str,
        agent_failed: bool,
        worker: u8,
        logger: &'a Arc<dyn PipelineLogger>,
        accepted: Option<&'a [u32]>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = VerificationOutcome> + Send + 'a>> {
        Box::pin(async move {
            logger
                .log("[VERIFY] Checking anti-cheat compliance (no prebuilt frameworks/daemons)...");
            let mut disqualified = false;
            let mut disqualification_reason = None;
            if let Err(violation) = ComplianceChecker::check_no_frameworks(workdir) {
                logger.log(&format!("[ANTI-CHEAT VIOLATION] {}", violation));
                disqualified = true;
                disqualification_reason = Some(violation);
            } else {
                logger.log("[VERIFY] Anti-cheat compliance passed.");
            }

            let (has_runnable, candidate_check_error) =
                match sandbox.ensure_runnable_candidate(workdir) {
                    Ok(v) => (v, None),
                    Err(e) => (false, Some(e.to_string())),
                };

            let mut extra_throughput: Option<f64> = None;
            let mut extra_robustness: Vec<crate::verifier::StageResult> = Vec::new();
            let mut infra_error = false;

            // A campaign ticket is judged on its acceptance subset; the
            // early-return branches (disqualification, no runnable code)
            // must count that subset, not the full task suite.
            let accepted_total = accepted
                .map(|a| a.len() as u32)
                .unwrap_or_else(|| task.total_stages());

            let (pass_rate, passed_stages, total_stages, stage_results) = if disqualified {
                let reason = disqualification_reason
                    .clone()
                    .unwrap_or_else(|| "Anti-cheat compliance violation".to_string());
                let stages = vec![crate::verifier::StageResult {
                    stage: 0,
                    name: "Anti-Cheat Compliance".to_string(),
                    passed: false,
                    error: Some(reason),
                }];
                (0.0, 0, accepted_total, stages)
            } else if !has_runnable {
                infra_error = candidate_check_error.is_some();
                if let Some(e) = &candidate_check_error {
                    logger.log(&format!(
                    "[INFRA] Could not inspect the candidate workspace ({e}). This is a harness fault, not a model result."
                ));
                } else if agent_failed {
                    logger.log(
                        "[SETUP] Candidate did not produce runnable code. Skipping verification.",
                    );
                } else {
                    logger.log("[SETUP] No runnable start.sh or Dockerfile found in ./workspace! Skipping verification.");
                }
                (0.0, 0, accepted_total, Vec::new())
            } else {
                // Extra published ports, gated on the ticket's acceptance:
                // 8090 for the dashboard stage (10), 6395 for the admin
                // binary-protocol stage (16). Publishing an unneeded port
                // would add a collision failure mode.
                let mut extra_ports: Vec<u16> = Vec::new();
                if matches!(task, TaskType::Redis) {
                    if accepted.map(|a| a.contains(&10)).unwrap_or(false) {
                        extra_ports.push(crate::web::server::worker_port(8090, worker));
                    }
                    if accepted.map(|a| a.contains(&16)).unwrap_or(false) {
                        extra_ports.push(crate::web::server::worker_port(6395, worker));
                    }
                }
                let extras = crate::sandbox::docker::CandidateRunExtras {
                    ports: extra_ports,
                    ..Default::default()
                };
                if let Err(e) = sandbox.start_candidate_in_docker_with(workdir, port, &extras) {
                    if crate::sandbox::docker::SandboxManager::is_model_packaging_error(
                        &e.to_string(),
                    ) {
                        logger.log(&format!(
                        "[MODEL-FAULT] Candidate container failed to start ({e}); scoring as model failure."
                    ));
                    } else {
                        logger.log(&format!(
                        "[INFRA] Candidate container failed to start ({e}); verifiers would fail on transport, not on the candidate."
                    ));
                        infra_error = true;
                    }
                }

                if matches!(task, TaskType::Proxy | TaskType::Redis | TaskType::Http) {
                    if let Err(e) = sandbox.connect_candidate_to_network() {
                        logger.log(&format!(
                        "[SETUP] Warning: candidate network attach failed: {}. Upstream dials may fail.",
                        e
                    ));
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }

                let ready = if infra_error {
                    logger.log(&format!(
                    "[SETUP] Candidate never started; skipping port {} wait (stages will fail fast on transport).",
                    port
                ));
                    false
                } else {
                    logger.log(&format!(
                        "[SETUP] Waiting for server port {} to become ready (timeout: 25s)...",
                        port
                    ));
                    sandbox.wait_for_port(port, 25).await
                };

                if ready {
                    logger.log(&format!(
                        "[SETUP] Server on port {} is online! Proceeding to verifier.",
                        port
                    ));
                } else {
                    logger.log(&format!("[WARN] Port {} not responding within 25s timeout. Proceeding with verification.", port));
                }

                let log_robust =
                    |logger: &Arc<dyn PipelineLogger>, rs: &[crate::verifier::StageResult]| {
                        for s in rs {
                            logger.log(&format!(
                                "  [ROBUST] Stage {}: {} -> {} {} (advisory, unscored)",
                                s.stage,
                                s.name,
                                if s.passed { "PASS" } else { "FAIL" },
                                s.error.as_deref().unwrap_or("")
                            ));
                        }
                    };

                match task {
                    TaskType::Redis => {
                        // Campaign acceptance subset: None = full suite (old
                        // behavior). A ticket runs every suite for evidence
                        // but is scored only on its accepted ids, and the
                        // destructive AOF stage runs only when accepted.
                        let allowed = |n: u32| accepted.map(|a| a.contains(&n)).unwrap_or(true);
                        let verifier = RedisVerifier::new(port, Some(verifier_seed));
                        let summary = verifier.run_all().await;
                        let mut stages = summary.stages.clone();
                        for s in &summary.stages {
                            let status_str = if s.passed { "PASS" } else { "FAIL" };
                            let err_str = s.error.as_deref().unwrap_or("");
                            let scope = if allowed(s.stage) {
                                ""
                            } else {
                                " (not in ticket acceptance)"
                            };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                s.stage,
                                crate::config::stage_label(s.stage, &s.name),
                                status_str,
                                err_str
                            ));
                            if !scope.is_empty() {
                                logger.log(&format!("    ^ accepted scope: excluded{scope}"));
                            }
                        }
                        if !allowed(6) {
                            logger.log("[VERIFY] AOF crash-recovery stage is outside this ticket's acceptance; skipping the destructive kill.");
                        } else if summary
                            .stages
                            .iter()
                            .filter(|s| allowed(s.stage))
                            .all(|s| s.passed)
                        {
                            let fail6 = |e: String| crate::verifier::StageResult {
                                stage: 6,
                                name: "Stage 6: AOF crash recovery".to_string(),
                                passed: false,
                                error: Some(e),
                            };
                            match verifier.write_probe_key().await {
                                Ok(()) => {
                                    logger
                                        .log("[BENCH] AOF probe written; SIGKILLing candidate...");
                                    let st = match sandbox.kill_candidate() {
                                        Ok(()) => {
                                            tokio::time::sleep(std::time::Duration::from_secs(2))
                                                .await;
                                            match sandbox.restart_candidate() {
                                                Ok(()) => {
                                                    if sandbox.wait_for_port(port, 25).await {
                                                        verifier.check_persistence().await
                                                    } else {
                                                        fail6("candidate did not reopen port after restart".to_string())
                                                    }
                                                }
                                                Err(e) => fail6(format!("restart failed: {}", e)),
                                            }
                                        }
                                        Err(e) => fail6(format!("SIGKILL failed: {}", e)),
                                    };
                                    let status_str = if st.passed { "PASS" } else { "FAIL" };
                                    logger.log(&format!(
                                        "  Stage {}: {} -> {} {}",
                                        st.stage,
                                        crate::config::stage_label(st.stage, &st.name),
                                        status_str,
                                        st.error.as_deref().unwrap_or("")
                                    ));
                                    stages.push(st);
                                }
                                Err(e) => {
                                    let st = fail6(format!("AOF probe write failed: {}", e));
                                    logger.log(&format!(
                                        "  Stage {}: {} -> FAIL {}",
                                        st.stage,
                                        crate::config::stage_label(st.stage, &st.name),
                                        st.error.as_deref().unwrap_or("")
                                    ));
                                    stages.push(st);
                                }
                            }
                        } else {
                            let st = crate::verifier::StageResult {
                                stage: 6,
                                name: "Stage 6: AOF crash recovery".to_string(),
                                passed: false,
                                error: Some("skipped due to failure in earlier stages".to_string()),
                            };
                            logger.log("  Stage 6: AOF crash recovery -> FAIL skipped due to failure in earlier stages");
                            stages.push(st);
                        }
                        // Stage 7 (INFO metrics) is explicit-acceptance
                        // only: single-shot redis runs keep their six-stage
                        // totals, campaign tickets including id 7 opt in.
                        if accepted.map(|a| a.contains(&7)).unwrap_or(false) {
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if prior_ok {
                                verifier.test_stage_info().await
                            } else {
                                crate::verifier::StageResult {
                                    stage: 7,
                                    name: "Stage 7: INFO metrics & counters".to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                        }
                        // Stage 8 (ENOSPC incident) runs in a fault
                        // container: the candidate is restarted with
                        // SDB_DATA_DIR on an 8MB tmpfs so "disk full" is
                        // real and contained. Explicit acceptance only.
                        // The normal container is restored afterwards so the
                        // advisory robustness/differential probes run
                        // against a clean candidate.
                        if accepted.map(|a| a.contains(&8)).unwrap_or(false) {
                            let name8 = "Stage 8: ENOSPC incident (log volume full)";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 8,
                                    name: name8.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                let started = match sandbox.stop_candidate() {
                                    Ok(_) => sandbox.start_candidate_enospc(workdir, port),
                                    Err(e) => Err(e),
                                };
                                match started {
                                    Ok(()) => {
                                        if sandbox.wait_for_port(port, 25).await {
                                            verifier.test_stage_enospc().await
                                        } else {
                                            crate::verifier::StageResult {
                                                stage: 8,
                                                name: name8.to_string(),
                                                passed: false,
                                                error: Some(
                                                    "fault container did not open its port"
                                                        .to_string(),
                                                ),
                                            }
                                        }
                                    }
                                    Err(e) => crate::verifier::StageResult {
                                        stage: 8,
                                        name: name8.to_string(),
                                        passed: false,
                                        error: Some(format!("fault container start failed: {e}")),
                                    },
                                }
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                            let _ = sandbox.stop_candidate();
                            let _ = sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                            let _ = sandbox.wait_for_port(port, 25).await;
                        }
                        // Stage 9 (tests & CI) is explicit-acceptance only.
                        // Runs host-side checks (test.sh twice, coverage,
                        // revert-sensitivity) in isolated sandbox containers.
                        if accepted.map(|a| a.contains(&9)).unwrap_or(false) {
                            let name9 = "Stage 9: tests & CI (coverage + sensitivity)";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 9,
                                    name: name9.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                let sandbox9 = SandboxManager::with_id(&format!("s9-w{worker}"));
                                let workdir9 = workdir.to_path_buf();
                                let logger9 = logger.clone();
                                match tokio::task::spawn_blocking(move || {
                                    run_tests_ci_blocking(sandbox9, workdir9, logger9)
                                })
                                .await
                                {
                                    Ok(st) => st,
                                    Err(e) => crate::verifier::StageResult {
                                        stage: 9,
                                        name: name9.to_string(),
                                        passed: false,
                                        error: Some(format!("tests stage task failed: {e}")),
                                    },
                                }
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                        }
                        // Stage 10 (dashboard, HTTP + SSE) is explicit-
                        // acceptance only; the candidate container published
                        // the worker's 8090 port above.
                        if accepted.map(|a| a.contains(&10)).unwrap_or(false) {
                            let name10 = "Stage 10: dashboard (HTTP + SSE)";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 10,
                                    name: name10.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                verifier
                                    .test_stage_dashboard(crate::web::server::worker_port(
                                        8090, worker,
                                    ))
                                    .await
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                        }
                        // Stage 11 (security) runs the candidate under
                        // SDB_PASSWORD and a 64MB memory cap: restart with
                        // the secret in its env, probe, then restore the
                        // normal container for the advisory passes. The
                        // secret is never logged by the harness; the stage
                        // also checks the candidate's own logs for it.
                        if accepted.map(|a| a.contains(&11)).unwrap_or(false) {
                            let name11 = "Stage 11: security (auth, caps, fuzz, slow-loris)";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 11,
                                    name: name11.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                let mut h = std::collections::hash_map::DefaultHasher::new();
                                use std::hash::{Hash, Hasher};
                                verifier_seed.hash(&mut h);
                                "security-stage-v1".hash(&mut h);
                                let mut rng = h.finish() | 1;
                                let mut next = || {
                                    rng ^= rng << 13;
                                    rng ^= rng >> 7;
                                    rng ^= rng << 17;
                                    rng
                                };
                                let secret = format!("{:016x}{:016x}", next(), next());
                                let sec_extras = crate::sandbox::docker::CandidateRunExtras {
                                    env: vec![("SDB_PASSWORD".to_string(), secret.clone())],
                                    ports: extras.ports.clone(),
                                    memory: Some("64m".to_string()),
                                    ..Default::default()
                                };
                                let started = sandbox.stop_candidate().and_then(|_| {
                                    sandbox.start_candidate_in_docker_with(
                                        workdir,
                                        port,
                                        &sec_extras,
                                    )
                                });
                                match started {
                                    Ok(()) => {
                                        if sandbox.wait_for_port(port, 25).await {
                                            let mut st = verifier
                                                .test_stage_security(
                                                    crate::web::server::worker_port(8090, worker),
                                                    &secret,
                                                )
                                                .await;
                                            if st.passed {
                                                let logs = sandbox.get_candidate_logs_tail(2000);
                                                if logs.contains(&secret) {
                                                    st = crate::verifier::StageResult {
                                                        stage: 11,
                                                        name: name11.to_string(),
                                                        passed: false,
                                                        error: Some(
                                                            "the password appeared in the container logs"
                                                                .to_string(),
                                                        ),
                                                    };
                                                }
                                            }
                                            st
                                        } else {
                                            crate::verifier::StageResult {
                                                stage: 11,
                                                name: name11.to_string(),
                                                passed: false,
                                                error: Some(
                                                    "secured candidate did not open its port"
                                                        .to_string(),
                                                ),
                                            }
                                        }
                                    }
                                    Err(e) => crate::verifier::StageResult {
                                        stage: 11,
                                        name: name11.to_string(),
                                        passed: false,
                                        error: Some(format!("secured candidate start failed: {e}")),
                                    },
                                }
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                            // Restore the unsecured candidate for the
                            // advisory robustness/differential passes.
                            let _ = sandbox.stop_candidate();
                            let _ = sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                            let _ = sandbox.wait_for_port(port, 25).await;
                        }
                        // Stage 12 (perf) is explicit-acceptance only: the
                        // protocol checks run here; the throughput ratio
                        // gate is applied after the bench (same method on
                        // both sides) further down.
                        if accepted.map(|a| a.contains(&12)).unwrap_or(false) {
                            let name12 = "Stage 12: pipelining (in order) + latency";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 12,
                                    name: name12.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                verifier.test_stage_perf().await
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                        }
                        // Stage 13 (HA finale) is explicit-acceptance only:
                        // the candidate is restarted as a three-node cluster
                        // for the probes, then the single-node candidate is
                        // restored for the advisory passes.
                        if accepted.map(|a| a.contains(&13)).unwrap_or(false) {
                            let name13 = "Stage 13: 3-node HA (replication, failover, rejoin)";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 13,
                                    name: name13.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                let ha_ports = [
                                    crate::web::server::worker_port(7001, worker),
                                    crate::web::server::worker_port(7002, worker),
                                    crate::web::server::worker_port(7003, worker),
                                ];
                                run_ha_stage(sandbox, workdir, &ha_ports, verifier_seed, logger)
                                    .await
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                            sandbox.stop_ha_nodes(3);
                            let _ = sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                            let _ = sandbox.wait_for_port(port, 25).await;
                        }
                        // Stages 14-16 (disk residency, compaction, admin
                        // binary protocol) are explicit-acceptance only.
                        // 14/15 restart the candidate onto a host-bind data
                        // dir under a 192MB cap and restore it afterwards;
                        // 16 runs against the restored candidate (6395 was
                        // published by the extras gating above).
                        let staged_fail =
                            |stage: u32, name: &str, err: String| crate::verifier::StageResult {
                                stage,
                                name: name.to_string(),
                                passed: false,
                                error: Some(err),
                            };
                        let prep_datadir = |workdir: &Path| -> Result<std::path::PathBuf> {
                            let datadir = workdir
                                .parent()
                                .ok_or_else(|| anyhow!("workspace has no parent dir"))?
                                .join("datadir");
                            let _ = std::fs::remove_dir_all(&datadir);
                            std::fs::create_dir_all(&datadir)?;
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                let _ = std::fs::set_permissions(
                                    &datadir,
                                    std::fs::Permissions::from_mode(0o777),
                                );
                            }
                            Ok(datadir)
                        };
                        let disk_extras =
                            |datadir: &Path| crate::sandbox::docker::CandidateRunExtras {
                                memory: Some("192m".to_string()),
                                binds: vec![(
                                    datadir.to_string_lossy().to_string(),
                                    "/data".to_string(),
                                )],
                                env: vec![("SDB_DATA_DIR".to_string(), "/data".to_string())],
                                ports: extras.ports.clone(),
                                ..Default::default()
                            };
                        if accepted.map(|a| a.contains(&14)).unwrap_or(false) {
                            let name14 = "Stage 14: disk-resident store under a memory cap";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                staged_fail(
                                    14,
                                    name14,
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                match prep_datadir(workdir) {
                                    Err(e) => staged_fail(14, name14, format!("datadir: {e}")),
                                    Ok(datadir) => {
                                        let de = disk_extras(&datadir);
                                        let started = sandbox.stop_candidate().and_then(|_| {
                                            sandbox
                                                .start_candidate_in_docker_with(workdir, port, &de)
                                        });
                                        match started {
                                            Err(e) => staged_fail(
                                                14,
                                                name14,
                                                format!(
                                                    "disk-resident candidate start failed: {e}"
                                                ),
                                            ),
                                            Ok(()) => {
                                                if !sandbox.wait_for_port(port, 30).await {
                                                    staged_fail(
                                                        14,
                                                        name14,
                                                        "disk-resident candidate did not open its port"
                                                            .to_string(),
                                                    )
                                                } else {
                                                    let fill =
                                                        verifier.test_stage_diskresid().await;
                                                    if !fill.passed {
                                                        fill
                                                    } else {
                                                        logger.log("  [DISKRESID] ~480MB written under a 192MB cap; SIGKILL + restart...");
                                                        let crash = sandbox
                                                            .kill_candidate()
                                                            .and_then(|_| {
                                                                std::thread::sleep(
                                                                    std::time::Duration::from_secs(
                                                                        2,
                                                                    ),
                                                                );
                                                                sandbox.restart_candidate()
                                                            });
                                                        match crash {
                                                            Err(e) => staged_fail(
                                                                14,
                                                                name14,
                                                                format!(
                                                                    "SIGKILL/restart failed: {e}"
                                                                ),
                                                            ),
                                                            Ok(()) => {
                                                                if !sandbox
                                                                    .wait_for_port(port, 25)
                                                                    .await
                                                                {
                                                                    staged_fail(
                                                                        14,
                                                                        name14,
                                                                        "candidate did not reopen its port after restart"
                                                                            .to_string(),
                                                                    )
                                                                } else {
                                                                    verifier
                                                                        .check_diskresid_restart()
                                                                        .await
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                            let _ = sandbox.stop_candidate();
                            let _ = sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                            let _ = sandbox.wait_for_port(port, 25).await;
                        }
                        if accepted.map(|a| a.contains(&15)).unwrap_or(false) {
                            let name15 = "Stage 15: compaction reclaims disk";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                staged_fail(
                                    15,
                                    name15,
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                match prep_datadir(workdir) {
                                    Err(e) => staged_fail(15, name15, format!("datadir: {e}")),
                                    Ok(datadir) => {
                                        let de = disk_extras(&datadir);
                                        let started = sandbox.stop_candidate().and_then(|_| {
                                            sandbox
                                                .start_candidate_in_docker_with(workdir, port, &de)
                                        });
                                        match started {
                                            Err(e) => staged_fail(
                                                15,
                                                name15,
                                                format!("candidate start failed: {e}"),
                                            ),
                                            Ok(()) => {
                                                if !sandbox.wait_for_port(port, 30).await {
                                                    staged_fail(
                                                        15,
                                                        name15,
                                                        "candidate did not open its port"
                                                            .to_string(),
                                                    )
                                                } else {
                                                    let compact = verifier
                                                        .test_stage_compaction(&datadir)
                                                        .await;
                                                    if !compact.passed {
                                                        compact
                                                    } else {
                                                        logger.log("  [COMPACT] footprint reclaimed; SIGKILL + restart to check survivors...");
                                                        let crash = sandbox
                                                            .kill_candidate()
                                                            .and_then(|_| {
                                                                std::thread::sleep(
                                                                    std::time::Duration::from_secs(
                                                                        2,
                                                                    ),
                                                                );
                                                                sandbox.restart_candidate()
                                                            });
                                                        match crash {
                                                            Err(e) => staged_fail(
                                                                15,
                                                                name15,
                                                                format!(
                                                                    "SIGKILL/restart failed: {e}"
                                                                ),
                                                            ),
                                                            Ok(()) => {
                                                                if !sandbox
                                                                    .wait_for_port(port, 25)
                                                                    .await
                                                                {
                                                                    staged_fail(
                                                                        15,
                                                                        name15,
                                                                        "candidate did not reopen its port after restart"
                                                                            .to_string(),
                                                                    )
                                                                } else {
                                                                    verifier
                                                                        .check_compaction_restart()
                                                                        .await
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                            let _ = sandbox.stop_candidate();
                            let _ = sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                            let _ = sandbox.wait_for_port(port, 25).await;
                        }
                        if accepted.map(|a| a.contains(&16)).unwrap_or(false) {
                            let name16 = "Stage 16: admin binary protocol (SDB1 v2)";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                staged_fail(
                                    16,
                                    name16,
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                crate::verifier::binproto::admin_stage(
                                    crate::web::server::worker_port(6395, worker),
                                    port,
                                    Some(verifier_seed),
                                )
                                .await
                            };
                            let status_str = if st.passed { "PASS" } else { "FAIL" };
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                st.stage,
                                crate::config::stage_label(st.stage, &st.name),
                                status_str,
                                st.error.as_deref().unwrap_or("")
                            ));
                            stages.push(st);
                        }
                        let verdict: Vec<&crate::verifier::StageResult> =
                            stages.iter().filter(|s| allowed(s.stage)).collect();
                        let ps = verdict.iter().filter(|s| s.passed).count() as u32;
                        let ts = verdict.len() as u32;
                        let pr = if ts > 0 {
                            ps as f64 / ts as f64 * 100.0
                        } else {
                            0.0
                        };
                        for s in verifier.run_robustness().await {
                            log_robust(logger, std::slice::from_ref(&s));
                            extra_robustness.push(s);
                        }
                        let ref_port = crate::web::server::worker_port(6380, worker);
                        for s in verifier.run_differential(ref_port).await {
                            log_robust(logger, std::slice::from_ref(&s));
                            extra_robustness.push(s);
                        }
                        (pr, ps, ts, stages)
                    }
                    TaskType::Http => {
                        let verifier = HttpVerifier::new(port, Some(verifier_seed));
                        let summary = verifier.run_all().await;
                        for s in &summary.stages {
                            let status_str = if s.passed { "PASS" } else { "FAIL" };
                            let err_str = s.error.as_deref().unwrap_or("");
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                s.stage,
                                crate::config::stage_label(s.stage, &s.name),
                                status_str,
                                err_str
                            ));
                        }
                        for s in verifier.run_robustness().await {
                            log_robust(logger, std::slice::from_ref(&s));
                            extra_robustness.push(s);
                        }
                        (
                            summary.pass_rate,
                            summary.passed_count,
                            summary.total_stages,
                            summary.stages,
                        )
                    }
                    TaskType::Wal => {
                        let log_stage =
                            |logger: &Arc<dyn PipelineLogger>, s: &crate::verifier::StageResult| {
                                let status_str = if s.passed { "PASS" } else { "FAIL" };
                                let err_str = s.error.as_deref().unwrap_or("");
                                logger.log(&format!(
                                    "  Stage {}: {} -> {} {}",
                                    s.stage,
                                    crate::config::stage_label(s.stage, &s.name),
                                    status_str,
                                    err_str
                                ));
                            };
                        let fail = |n: u32, name: &str, err: String| crate::verifier::StageResult {
                            stage: n,
                            name: name.to_string(),
                            passed: false,
                            error: Some(err),
                        };
                        let verifier = WalVerifier::new(port, Some(verifier_seed));
                        let basic = verifier.run_basic().await;
                        let mut stages = basic.stages.clone();
                        for s in &basic.stages {
                            log_stage(logger, s);
                        }
                        let mut probe_acks = 0usize;
                        match publish_wal_probe(port).await {
                            Ok(n) => {
                                probe_acks = n;
                                logger.log(&format!("  Probe: {}/2000 acks on recovery_probe", n));
                            }
                            Err(e) => logger.log(&format!(
                                "  Probe publish failed (recovery unlikely): {}",
                                e
                            )),
                        }
                        let rec = if probe_acks >= 1500 {
                            match sandbox.kill_candidate() {
                                Ok(()) => {
                                    logger.log("[BENCH] Candidate SIGKILLed; restarting...");
                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                    match sandbox.restart_candidate() {
                                        Ok(()) => {
                                            if sandbox.wait_for_port(port, 25).await {
                                                logger.log(
                                                    "[BENCH] Candidate back; checking recovery...",
                                                );
                                                verifier.check_recovery(probe_acks).await
                                            } else {
                                                fail(
                                                    4,
                                                    "Stage 4: Crash recovery",
                                                    "candidate did not reopen port after restart"
                                                        .to_string(),
                                                )
                                            }
                                        }
                                        Err(e) => fail(
                                            4,
                                            "Stage 4: Crash recovery",
                                            format!("restart failed: {}", e),
                                        ),
                                    }
                                }
                                Err(e) => fail(
                                    4,
                                    "Stage 4: Crash recovery",
                                    format!("SIGKILL failed: {}", e),
                                ),
                            }
                        } else {
                            fail(
                                4,
                                "Stage 4: Crash recovery",
                                "probe had no full acks; skipping kill".to_string(),
                            )
                        };
                        log_stage(logger, &rec);
                        stages.push(rec);
                        let tp = verifier.measure_throughput().await;
                        extra_throughput = tp;
                        let tp_stage = crate::verifier::StageResult {
                            stage: 5,
                            name: "Stage 5: PUB burst throughput".to_string(),
                            passed: tp.is_some(),
                            error: match tp {
                                Some(v) => {
                                    logger.log(&format!("  Candidate burst: {:.0} msgs/sec", v));
                                    None
                                }
                                None => Some("no throughput measurement".to_string()),
                            },
                        };
                        log_stage(logger, &tp_stage);
                        stages.push(tp_stage);
                        let ps = stages.iter().filter(|s| s.passed).count() as u32;
                        let ts = stages.len() as u32;
                        let pr = ps as f64 / ts as f64 * 100.0;
                        (pr, ps, ts, stages)
                    }
                    TaskType::Pdf => {
                        let verifier = PdfVerifier::new(port, Some(verifier_seed));
                        let summary = verifier.run_all().await;
                        for s in &summary.stages {
                            let status_str = if s.passed { "PASS" } else { "FAIL" };
                            let err_str = s.error.as_deref().unwrap_or("");
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                s.stage,
                                crate::config::stage_label(s.stage, &s.name),
                                status_str,
                                err_str
                            ));
                        }
                        (
                            summary.pass_rate,
                            summary.passed_count,
                            summary.total_stages,
                            summary.stages,
                        )
                    }
                    TaskType::Proxy => {
                        let log_stage =
                            |logger: &Arc<dyn PipelineLogger>, s: &crate::verifier::StageResult| {
                                let status_str = if s.passed { "PASS" } else { "FAIL" };
                                let err_str = s.error.as_deref().unwrap_or("");
                                logger.log(&format!(
                                    "  Stage {}: {} -> {} {}",
                                    s.stage,
                                    crate::config::stage_label(s.stage, &s.name),
                                    status_str,
                                    err_str
                                ));
                            };
                        let ups: Vec<(String, u16)> = (1u8..=3u8)
                            .map(|i| {
                                (
                                    sandbox.upstream_name(i),
                                    crate::web::server::worker_port(
                                        PROXY_UPSTREAM_PORTS[(i - 1) as usize],
                                        worker,
                                    ),
                                )
                            })
                            .collect();
                        let pool_ok = crate::config::upstreams_reachable(
                            &ups.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
                        );
                        if !pool_ok {
                            logger.log(
                                "[SETUP] Upstream pool unreachable at verify; recreating fresh...",
                            );
                            match sandbox
                            .start_upstream_pool(&worker_upstream_ports(worker))
                        {
                            Ok(()) => logger.log("[SETUP] Upstream pool recreated."),
                            Err(e) => logger.log(&format!(
                                "[SETUP] Warning: upstream pool recreate failed: {}. Proxy stages will fail.",
                                e
                            )),
                        }
                        }
                        let verifier = ProxyVerifier::new(port, Some(verifier_seed), ups);
                        // Single-shot proxy runs stages 1-5; the sdb-edge
                        // campaign ticket scope comes from the acceptance.
                        // Stages 6-9 are campaign-only (explicit acceptance).
                        let allowed =
                            |stage: u32| accepted.map(|a| a.contains(&stage)).unwrap_or(true);
                        let campaign_only =
                            |stage: u32| accepted.map(|a| a.contains(&stage)).unwrap_or(false);
                        let mut stages: Vec<crate::verifier::StageResult> = Vec::new();
                        if allowed(1) || allowed(2) || allowed(3) {
                            let routing = verifier.run_routing().await;
                            for s in routing.stages.iter().filter(|s| allowed(s.stage)) {
                                log_stage(logger, s);
                                stages.push(s.clone());
                            }
                        }
                        if allowed(4) {
                            let fo = verifier.run_failover().await;
                            log_stage(logger, &fo);
                            stages.push(fo);
                        }
                        if allowed(5) {
                            let tp = verifier.measure_throughput().await;
                            extra_throughput = tp;
                            let tp_stage = crate::verifier::StageResult {
                                stage: 5,
                                name: "Stage 5: Concurrent GET throughput".to_string(),
                                passed: tp.is_some(),
                                error: match tp {
                                    Some(v) => {
                                        logger.log(&format!("  Candidate load: {:.0} req/sec", v));
                                        None
                                    }
                                    None => Some("no throughput measurement".to_string()),
                                },
                            };
                            log_stage(logger, &tp_stage);
                            stages.push(tp_stage);
                        }
                        // Stages 6/7 run against the plain candidate; 8 and
                        // 9 restart it with the cache env / the 128MB cap and
                        // restore it afterwards.
                        if campaign_only(6) || campaign_only(7) {
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            if campaign_only(6) {
                                let st = if !prior_ok {
                                    crate::verifier::StageResult {
                                        stage: 6,
                                        name: "Stage 6: Fast failover under a hung upstream"
                                            .to_string(),
                                        passed: false,
                                        error: Some(
                                            "skipped due to failure in earlier stages".to_string(),
                                        ),
                                    }
                                } else {
                                    verifier.test_stage6_quick_failover().await
                                };
                                log_stage(logger, &st);
                                stages.push(st);
                            }
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            if campaign_only(7) {
                                let st = if !prior_ok {
                                    crate::verifier::StageResult {
                                        stage: 7,
                                        name: "Stage 7: Active health checks + /status".to_string(),
                                        passed: false,
                                        error: Some(
                                            "skipped due to failure in earlier stages".to_string(),
                                        ),
                                    }
                                } else {
                                    verifier.test_stage7_health_status().await
                                };
                                log_stage(logger, &st);
                                stages.push(st);
                            }
                        }
                        if campaign_only(8) {
                            let name8 = "Stage 8: Response caching (TTL + X-Cache)";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 8,
                                    name: name8.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                let cache_extras = crate::sandbox::docker::CandidateRunExtras {
                                    env: vec![(
                                        "SDB_PROXY_CACHE_TTL".to_string(),
                                        crate::verifier::proxy::CACHE_TTL_SECS.to_string(),
                                    )],
                                    ports: extras.ports.clone(),
                                    ..Default::default()
                                };
                                let restarted = sandbox.stop_candidate().and_then(|_| {
                                    sandbox.start_candidate_in_docker_with(
                                        workdir,
                                        port,
                                        &cache_extras,
                                    )
                                });
                                match restarted {
                                    Err(e) => crate::verifier::StageResult {
                                        stage: 8,
                                        name: name8.to_string(),
                                        passed: false,
                                        error: Some(format!(
                                            "cache-enabled candidate start failed: {e}"
                                        )),
                                    },
                                    Ok(()) => {
                                        if !sandbox.wait_for_port(port, 25).await {
                                            crate::verifier::StageResult {
                                                stage: 8,
                                                name: name8.to_string(),
                                                passed: false,
                                                error: Some(
                                                    "candidate did not reopen its port with the cache env"
                                                        .to_string(),
                                                ),
                                            }
                                        } else {
                                            verifier.test_stage8_caching().await
                                        }
                                    }
                                }
                            };
                            log_stage(logger, &st);
                            stages.push(st);
                            let _ = sandbox.stop_candidate();
                            let _ = sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                            let _ = sandbox.wait_for_port(port, 25).await;
                        }
                        if campaign_only(9) {
                            let name9 = "Stage 9: Limits and hostile input";
                            let prior_ok =
                                stages.iter().filter(|s| allowed(s.stage)).all(|s| s.passed);
                            let st = if !prior_ok {
                                crate::verifier::StageResult {
                                    stage: 9,
                                    name: name9.to_string(),
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                }
                            } else {
                                let limits_extras = crate::sandbox::docker::CandidateRunExtras {
                                    memory: Some("128m".to_string()),
                                    ports: extras.ports.clone(),
                                    ..Default::default()
                                };
                                let restarted = sandbox.stop_candidate().and_then(|_| {
                                    sandbox.start_candidate_in_docker_with(
                                        workdir,
                                        port,
                                        &limits_extras,
                                    )
                                });
                                match restarted {
                                    Err(e) => crate::verifier::StageResult {
                                        stage: 9,
                                        name: name9.to_string(),
                                        passed: false,
                                        error: Some(format!(
                                            "memory-capped candidate start failed: {e}"
                                        )),
                                    },
                                    Ok(()) => {
                                        if !sandbox.wait_for_port(port, 25).await {
                                            crate::verifier::StageResult {
                                                stage: 9,
                                                name: name9.to_string(),
                                                passed: false,
                                                error: Some(
                                                    "candidate did not reopen its port under the memory cap"
                                                        .to_string(),
                                                ),
                                            }
                                        } else {
                                            verifier.test_stage9_limits().await
                                        }
                                    }
                                }
                            };
                            log_stage(logger, &st);
                            stages.push(st);
                            let _ = sandbox.stop_candidate();
                            let _ = sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                            let _ = sandbox.wait_for_port(port, 25).await;
                        }
                        let ps = stages.iter().filter(|s| s.passed).count() as u32;
                        let ts = stages.len() as u32;
                        let pr = ps as f64 / ts as f64 * 100.0;
                        (pr, ps, ts, stages)
                    }
                    TaskType::Pubsub => {
                        let verifier = PubsubVerifier::new(port, Some(verifier_seed));
                        let summary = verifier.run_all().await;
                        for s in &summary.stages {
                            let status_str = if s.passed { "PASS" } else { "FAIL" };
                            let err_str = s.error.as_deref().unwrap_or("");
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                s.stage,
                                crate::config::stage_label(s.stage, &s.name),
                                status_str,
                                err_str
                            ));
                        }
                        if let Some(p99) = summary.soak_p99_ms {
                            logger.log(&format!("  Soak p99 broadcast latency: {:.1}ms", p99));
                        }
                        extra_throughput = summary.soak_msgs_per_sec;
                        (
                            summary.pass_rate,
                            summary.passed_count,
                            summary.total_stages,
                            summary.stages,
                        )
                    }
                    TaskType::Binproto => {
                        let verifier = BinprotoVerifier::new(port, Some(verifier_seed));
                        let summary = verifier.run_all().await;
                        for s in &summary.stages {
                            let status_str = if s.passed { "PASS" } else { "FAIL" };
                            let err_str = s.error.as_deref().unwrap_or("");
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                s.stage,
                                crate::config::stage_label(s.stage, &s.name),
                                status_str,
                                err_str
                            ));
                        }
                        if let Some(tp) = verifier.measure_throughput().await {
                            logger.log(&format!("  Candidate Throughput: {tp:.0} ops/sec"));
                            extra_throughput = Some(tp);
                        }
                        (
                            summary.pass_rate,
                            summary.passed_count,
                            summary.total_stages,
                            summary.stages,
                        )
                    }
                }
            };

            let candidate_logs = if pass_rate < 100.0 {
                let diag = sandbox.get_candidate_logs();
                logger.log("[DIAGNOSTICS] Candidate container runtime output:");
                for line in diag.lines() {
                    logger.log(&format!("  | {}", line));
                }
                diag
            } else {
                String::new()
            };

            VerificationOutcome {
                pass_rate,
                passed_stages,
                total_stages,
                stage_results,
                extra_robustness,
                extra_throughput,
                candidate_logs,
                disqualified,
                disqualification_reason,
                infra_error,
            }
        })
    }

    pub async fn execute(
        mut config: BenchmarkConfig,
        logger: Arc<dyn PipelineLogger>,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<PipelineOutput> {
        let buffering_logger = Arc::new(BufferingPipelineLogger::new(logger, cancel_flag.clone()));
        let logger: Arc<dyn PipelineLogger> = buffering_logger.clone();

        let start_time = Instant::now();
        let started_at = Utc::now().to_rfc3339();

        let run_id = config.run_id.clone().unwrap_or_else(|| {
            format!(
                "{}_{}_{}_w{}",
                config.task,
                config.model.replace(['/', ':'], "_"),
                Utc::now().format("%Y%m%d_%H%M%S%.3f"),
                config.worker
            )
        });
        let verifier_seed = hash_seed(&run_id);

        logger.log(&format!(
            "[PIPELINE] Starting benchmark run '{}' (Model: {}, Task: {}, Effort: {}, Budget: ${:.2})",
            run_id, config.model, config.task, config.effort, config.budget_usd
        ));

        // Per-run capped key: the container must never see the account key.
        // Mint `budget + headroom` capped, 24h-expiring; revoke at every exit
        // via the guard below (return/Err/panic/cancel). No mgmt key (or
        // mint failure) = loud fail-open to the account key, never a failed
        // batch. The plaintext is never logged (RunKey has no Debug).
        // Async drop guard: revoke fires on EVERY exit (happy path,
        // early return, cancel, panic-unwind) exactly once. Drop cannot
        // await, so it spawns the revoke onto the runtime; if no runtime
        // is alive (test teardown) the hash is simply dropped and the
        // 24h expiry bounds the leak.
        struct RevokeGuard {
            hash: Option<String>,
        }
        impl Drop for RevokeGuard {
            fn drop(&mut self) {
                if let Some(hash) = self.hash.take() {
                    if tokio::runtime::Handle::try_current().is_ok() {
                        tokio::spawn(async move {
                            let _ = crate::cost::keys::revoke_run_key(&hash).await;
                        });
                    }
                }
            }
        }
        let mut revoke_guard = RevokeGuard { hash: None };
        if crate::cost::keys::mgmt_key().is_some() {
            // Headroom: live-spend metering lags; the cap must survive the
            // batch without tripping mid-run. 2x budget, min $2.
            let cap = (config.budget_usd * 2.0).max(2.0);
            match crate::cost::keys::mint_run_key(&run_id, cap).await {
                Ok(rk) => {
                    logger.log(&format!(
                        "[KEY] minted capped run key (limit ${:.2}, 24h expiry; hash {}…) for this batch",
                        cap,
                        &rk.hash[..rk.hash.len().min(8)]
                    ));
                    revoke_guard.hash = Some(rk.hash);
                    config.api_key = Some(rk.key);
                }
                Err(e) => {
                    logger.log(&format!(
                        "[KEY] WARNING: mint failed ({e:#}); continuing with account key (fail-open, blast radius = account)"
                    ));
                }
            }
        } else {
            logger.log(
                "[KEY] no OPENROUTER_MGMT_KEY: using account key in container (set a mgmt key + per-key credit limit for blast-radius control)",
            );
        }
        // 1. Prepare Workspace
        // Campaign continuity: stage > 0 continues in the workspace the
        // model already built. The wipe is what makes a single-shot run
        // pristine; for a ticket the existing repo IS the input.
        let continuity = config
            .campaign_stage
            .as_ref()
            .map(|c| c.continuity)
            .unwrap_or(false);
        if !config.eval_only && config.workdir.exists() && !continuity {
            let _ = fs::remove_dir_all(&config.workdir);
        }
        // Eval-only and campaign runs may arrive with an existing workspace;
        // create when missing so verification and the git-evidence path
        // always have a directory (creating an empty one never hides a
        // candidate: the runnable check still fails on it).
        fs::create_dir_all(&config.workdir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&config.workdir, fs::Permissions::from_mode(0o777));
        }

        let sandbox = SandboxManager::with_id(&run_id);
        // RAII owner: trial exit (return/Err/panic/cancel) reaps by label.
        // Manual cleanup() calls below remain as belt; both are idempotent.
        let _trial_guard = RunGuard::new(SandboxManager::with_id(&run_id));

        // 1b. Create the run's private Docker network (reference aliases + agent attach)
        match sandbox.ensure_network() {
            Ok(()) => logger.log(&format!(
                "[SETUP] Private run network '{}' ready (reference aliases: ref-redis / ref-http)",
                sandbox.network_name()
            )),
            Err(e) => {
                // Fail closed: no private network => agent attach is a
                // guaranteed ghost run (exit 125 "network not found") that
                // persists a 0-trial junk parent and poisons the merge.
                // Surface as infra-error, never as a scored trial.
                logger.log(&format!(
                    "[SETUP] FATAL: could not create run network: {}. Aborting trial (infra-error, unscored).",
                    e
                ));
                sandbox.cleanup();
                return Err(anyhow!("infra-error: run network unavailable: {}", e));
            }
        }

        if let Some(ref cf) = cancel_flag {
            if cf.load(Ordering::SeqCst) {
                sandbox.cleanup();
                return Err(anyhow!("Benchmark run was cancelled by user"));
            }
        }

        // 2. Query initial spend checkpoint
        let initial_spend = if let Some(ref key) = config.api_key {
            ModelPricing::query_openrouter_key_usage(key).await
        } else {
            None
        };
        if let Some(init) = initial_spend {
            // Label honesty: after the swap above, config.api_key is the
            // capped run key when minted (spend ~$0.00 fresh) or the account
            // key on fail-open. Never imply whose balance this is.
            let minted = revoke_guard.hash.is_some();
            logger.log(&format!(
                "[KEY] initial {} spend: ${:.4} USD",
                if minted { "run-key" } else { "account-key" },
                init
            ));
        }

        // Auto-update OMP sandbox image if needed
        if !config.eval_only {
            if let Err(e) = crate::sandbox::omp_updater::auto_update_if_needed().await {
                logger.log(&format!("[OMP Auto-Update Warning] {}", e));
            }
        }

        // 3. Start reference containers (attached to the run network under stable aliases)
        logger.log("[SETUP] Starting official reference server in Docker...");
        match config.task {
            TaskType::Redis => {
                if let Err(e) = sandbox
                    .start_reference_redis(crate::web::server::worker_port(6380, config.worker))
                {
                    logger.log(&format!(
                        "[SETUP] Warning: reference Redis unavailable: {}",
                        e
                    ));
                }
            }
            TaskType::Http => {
                if let Err(e) = sandbox
                    .start_reference_http(crate::web::server::worker_port(8081, config.worker))
                {
                    logger.log(&format!(
                        "[SETUP] Warning: reference HTTP unavailable: {}",
                        e
                    ));
                }
            }
            TaskType::Pdf => {
                logger.log(
                    "[SETUP] PDF task uses verifier-generated fixtures; no reference container.",
                );
            }
            TaskType::Proxy => {
                match sandbox.start_upstream_pool(&worker_upstream_ports(config.worker)) {
                    Ok(()) => logger.log("[SETUP] Upstream nginx pool on :8101/:8102/:8103 ready."),
                    Err(e) => logger.log(&format!(
                        "[SETUP] Warning: upstream pool unavailable: {}. Proxy stages will fail.",
                        e
                    )),
                }
            }
            TaskType::Wal | TaskType::Pubsub | TaskType::Binproto => {
                logger.log("[SETUP] No reference container for this task (verifier-timed).");
            }
        }

        // 4. Read prompt (campaign tickets override the task prompt; port
        // templating + the READY contract apply to both).
        let prompt_content = if let Some(cs) = &config.campaign_stage {
            logger.log(&format!(
                "[CAMPAIGN] Ticket {}: reading campaign prompt {}",
                cs.stage,
                cs.prompt_path.display()
            ));
            fs::read_to_string(&cs.prompt_path).map_err(|_| {
                anyhow!(
                    "Could not read campaign stage prompt: {}",
                    cs.prompt_path.display()
                )
            })?
        } else {
            let prompt_file = get_repo_root()
                .join("tasks")
                .join(config.task.to_string())
                .join("prompt.md");
            fs::read_to_string(&prompt_file)
                .map_err(|_| anyhow!("Could not read prompt file: {}", prompt_file.display()))?
        };
        // Port templating (parallel workers): prompt files carry the canonical
        // port; worker>0 cells substitute their offset port so agents build on
        // the ports their containers actually publish. Worker 0 is verbatim.
        // Targeted rule: skip lines mentioning in-network aliases
        // (ref-redis/ref-http/upstreamN — fixed ports inside the run
        // network, identical for every worker). Rewrite only candidate-local
        // references (localhost, 0.0.0.0, -p, bare port mentions) with word
        // boundaries so coincidental digit runs never match.
        let prompt_content = if config.worker == 0 {
            prompt_content
        } else {
            crate::config::template_ports_for_worker(
                &prompt_content,
                config.task.default_port(),
                config.worker,
            )
        };
        let prompt_content = format!(
            "{}\n\n### READY FOR VERIFICATION SIGNAL\nWhen you believe your implementation is complete and properly running on the requested port, signal that you are ready for evaluation by creating `/workspace/READY` (for example, run `touch READY` or `touch /workspace/READY`), or simply conclude your session. The benchmark framework will immediately detect this and run the automated verification suite!\n",
            prompt_content
        );

        // Clean any stale signal file from previous runs
        let _ = fs::remove_file(config.workdir.join("READY"));

        // 5. Run agent harness (omp | opencode) unless eval_only.
        let htag = match config.harness {
            Harness::Omp => "OMP",
            Harness::Opencode => "OPENCODE",
        };
        let mut agent_failed = false;
        let (mut omp_stats, mut prompt_tokens, mut cached_tokens, mut completion_tokens) =
            if !config.eval_only {
                logger.log(&format!(
                "[{htag}] Spawning {htag} agent inside isolated Docker sandbox with model '{}' (Effort: {})...",
                config.model, config.effort
            ));

                let initial_turn_cap = initial_turn_cap(
                    config.max_turns,
                    config.feedback_rounds,
                    config.campaign_stage.is_some(),
                );

                let limits = AgentExecutionLimits {
                    max_turns: initial_turn_cap,
                    max_budget_usd: if config.budget_usd > 0.0 {
                        Some(config.budget_usd)
                    } else {
                        None
                    },
                    timeout_seconds: if config.timeout_min > 0 {
                        Some(config.timeout_min * 60)
                    } else {
                        None
                    },
                };

                match Self::run_agent_session(
                    &config,
                    &run_id,
                    &prompt_content,
                    limits,
                    &sandbox,
                    &logger,
                    cancel_flag.clone(),
                )
                .await
                {
                    Ok(stats) => {
                        logger.log(&format!(
                            "[{htag}] Finished! Tokens: prompt={}, cached={}, completion={}",
                            stats.prompt_tokens, stats.cached_tokens, stats.completion_tokens
                        ));
                        let p = stats.prompt_tokens;
                        let c = stats.cached_tokens;
                        let comp = stats.completion_tokens;
                        (stats, p, c, comp)
                    }
                    Err(e) => {
                        if let Some(ref cf) = cancel_flag {
                            if cf.load(Ordering::SeqCst) {
                                sandbox.cleanup();
                                return Err(anyhow!("Benchmark run was cancelled by user"));
                            }
                        }
                        logger.log(&format!("[{htag} ERROR] Agent execution failed: {}", e));
                        agent_failed = true;
                        let partial_stats = e
                            .downcast_ref::<crate::sandbox::omp::OmpExecutionError>()
                            .map(|oe| oe.partial_stats.clone())
                            .unwrap_or_default();
                        let p = partial_stats.prompt_tokens;
                        let c = partial_stats.cached_tokens;
                        let comp = partial_stats.completion_tokens;
                        (partial_stats, p, c, comp)
                    }
                }
            } else {
                logger.log(&format!("[{htag}] Skipped (--eval-only)"));
                (OmpSessionStats::default(), 0, 0, 0)
            };

        if let Some(ref cf) = cancel_flag {
            if cf.load(Ordering::SeqCst) {
                logger.log(
                    "[PIPELINE] Execution cancelled by user. Terminating sandbox containers...",
                );
                sandbox.cleanup();
                return Err(anyhow!("Benchmark run was cancelled by user"));
            }
        }

        // 6 & 7. Verification & Interactive Repair
        let _ = fs::remove_file(config.workdir.join("READY"));
        let port = crate::web::server::worker_port(config.task.default_port(), config.worker);
        let mut v_outcome = Self::run_verification(
            config.task,
            &config.workdir,
            &sandbox,
            port,
            &verifier_seed,
            agent_failed,
            config.worker,
            &logger,
            config
                .campaign_stage
                .as_ref()
                .map(|c| c.accepted_stages.as_slice()),
        )
        .await;

        let mut was_repaired = false;
        let mut feedback_rounds_used = 0u32;
        let mut best_pass_rate = v_outcome.pass_rate;

        if v_outcome.pass_rate < 100.0
            && config.feedback_rounds > 0
            && !v_outcome.disqualified
            && !v_outcome.infra_error
            && !config.eval_only
        {
            // A fault with no runnable candidate produced no stage results:
            // there is nothing to surgically repair, and the prompt's
            // "fix your existing implementation" is meaningless (live: a
            // quota-blocked model burned a repair round against an empty
            // workspace). Skip straight to the verdict.
            if v_outcome.stage_results.is_empty() {
                logger.log(
                    "[FEEDBACK] No stage results to feed back (no runnable candidate); skipping repair rounds.",
                );
            } else {
                logger.log(&format!(
                "[FEEDBACK] Initial attempt passed {} of {} stages ({:.1}%). Starting interactive repair loop (max {} rounds)...",
                v_outcome.passed_stages,
                v_outcome.total_stages,
                v_outcome.pass_rate,
                config.feedback_rounds
            ));

                for round in 1..=config.feedback_rounds {
                    if let Some(ref cf) = cancel_flag {
                        if cf.load(Ordering::SeqCst) {
                            sandbox.cleanup();
                            return Err(anyhow!("Benchmark run was cancelled by user"));
                        }
                    }

                    // Pool check FIRST: an exhausted pool must break before
                    // stopping the candidate, writing FEEDBACK_REPORT.md, or
                    // recording a round that never runs. (Previously all three
                    // happened first, so the manifest recorded a phantom round.)
                    let global_max = config.max_turns;
                    let turns_remaining = match repair_round_allowance(
                        global_max,
                        omp_stats.steps_taken,
                    ) {
                        RepairAllowance::Exhausted => {
                            logger.log(&format!(
                            "[LIMIT] Global turn budget ({} turns) reached across attempts. Concluding run.",
                            global_max
                        ));
                            break;
                        }
                        RepairAllowance::Uncapped => None,
                        RepairAllowance::Capped(n) => Some(n),
                    };
                    feedback_rounds_used = round;

                    // 1. Stop candidate container to release ports and candidate name
                    let _ = sandbox.stop_candidate();

                    // 2. Format feedback prompt and save FEEDBACK_REPORT.md in workspace
                    let feedback_prompt = Self::format_feedback_prompt(
                        config.task,
                        v_outcome.passed_stages,
                        v_outcome.total_stages,
                        &v_outcome.stage_results,
                        &v_outcome.candidate_logs,
                    );
                    let _ = fs::write(config.workdir.join("FEEDBACK_REPORT.md"), &feedback_prompt);

                    logger.log(&format!(
                        "[FEEDBACK] ━━━ Repair Round {}/{} (Model: {}) ━━━",
                        round, config.feedback_rounds, config.model
                    ));

                    // Clean any lingering signal file before launching repair session
                    let _ = fs::remove_file(config.workdir.join("READY"));

                    // 3. Launch agent for surgical fix with feedback prompt
                    let round_limits = AgentExecutionLimits {
                        max_turns: turns_remaining,
                        max_budget_usd: if config.budget_usd > 0.0 {
                            Some(config.budget_usd)
                        } else {
                            None
                        },
                        timeout_seconds: if config.timeout_min > 0 {
                            Some(config.timeout_min * 60)
                        } else {
                            None
                        },
                    };

                    let round_run_id = format!("{}_r{}", run_id, round);
                    let round_res = Self::run_agent_session(
                        &config,
                        &round_run_id,
                        &feedback_prompt,
                        round_limits,
                        &sandbox,
                        &logger,
                        cancel_flag.clone(),
                    )
                    .await;

                    match round_res {
                        Ok(r_stats) => {
                            prompt_tokens += r_stats.prompt_tokens;
                            cached_tokens += r_stats.cached_tokens;
                            completion_tokens += r_stats.completion_tokens;
                            omp_stats.steps_taken += r_stats.steps_taken;
                            omp_stats.total_tokens += r_stats.total_tokens;
                            logger.log(&format!(
                            "[FEEDBACK] Round {} agent completed! Steps: {}, Round tokens: prompt={}, completion={}",
                            round, r_stats.steps_taken, r_stats.prompt_tokens, r_stats.completion_tokens
                        ));
                        }
                        Err(e) => {
                            logger.log(&format!(
                                "[FEEDBACK ERROR] Round {} agent failed: {}",
                                round, e
                            ));
                            // Count partial work against the pool even on failure,
                            // mirroring the main agent path: a failed round still
                            // burned turns and tokens.
                            let partial = e
                                .downcast_ref::<crate::sandbox::omp::OmpExecutionError>()
                                .map(|oe| &oe.partial_stats)
                                .or_else(|| {
                                    e.downcast_ref::<crate::sandbox::omp::OmpBillingError>()
                                        .map(|be| &be.partial_stats)
                                });
                            if let Some(ps) = partial {
                                prompt_tokens += ps.prompt_tokens;
                                cached_tokens += ps.cached_tokens;
                                completion_tokens += ps.completion_tokens;
                                omp_stats.steps_taken += ps.steps_taken;
                                omp_stats.total_tokens += ps.total_tokens;
                            }
                        }
                    }

                    if let Some(ref cf) = cancel_flag {
                        if cf.load(Ordering::SeqCst) {
                            sandbox.cleanup();
                            return Err(anyhow!("Benchmark run was cancelled by user"));
                        }
                    }

                    // 4. Re-verify the updated candidate
                    let _ = fs::remove_file(config.workdir.join("READY"));
                    let next_outcome = Self::run_verification(
                        config.task,
                        &config.workdir,
                        &sandbox,
                        port,
                        &verifier_seed,
                        false,
                        config.worker,
                        &logger,
                        config
                            .campaign_stage
                            .as_ref()
                            .map(|c| c.accepted_stages.as_slice()),
                    )
                    .await;

                    logger.log(&format!(
                        "[FEEDBACK] Round {} test result: {} of {} stages passed ({:.1}%)",
                        round,
                        next_outcome.passed_stages,
                        next_outcome.total_stages,
                        next_outcome.pass_rate
                    ));

                    if next_outcome.pass_rate > best_pass_rate {
                        best_pass_rate = next_outcome.pass_rate;
                    }

                    if next_outcome.pass_rate >= 100.0 {
                        logger.log(&format!(
                        "[FEEDBACK] SUCCESS! Candidate surgically repaired to 100.0% stage pass in round {}!",
                        round
                    ));
                        was_repaired = true;
                        v_outcome = next_outcome;
                        break;
                    } else {
                        v_outcome = next_outcome;
                    }
                }
            }
        }

        let mut pass_rate = v_outcome.pass_rate;
        let mut passed_stages = v_outcome.passed_stages;
        let total_stages = v_outcome.total_stages;
        let mut stage_results = v_outcome.stage_results;
        let extra_robustness = v_outcome.extra_robustness;
        let extra_throughput = v_outcome.extra_throughput;
        let disqualified = v_outcome.disqualified;
        let _disqualification_reason = v_outcome.disqualification_reason;
        let infra_error = v_outcome.infra_error;

        // 8. Load testing if 100% pass
        let mut throughput = None;
        let mut reference_throughput = None;
        let mut baseline_source: Option<String> = None;
        if pass_rate == 100.0 {
            logger.log(
                "[BENCH] 100% tests passed! Running live throughput load test on candidate...",
            );
            match config.task {
                TaskType::Redis => {
                    // Scored window is GET-only; the write path is a separate
                    // unscored durability probe (see REDIS_BENCH_N). Candidate
                    // measurement failure is LOUD: silent Ok-arm-only logging
                    // let five reps x timeout kills produce tp=None while the
                    // row looked merely unanchored (2026-10-14 finding).
                    match BenchmarkRunner::run_redis_benchmark_async(
                        &sandbox.network_name(),
                        "candidate",
                        port,
                    )
                    .await
                    {
                        Ok(tp) => {
                            logger.log(&format!(
                                "  Candidate Throughput (median of {}, GET): {:.0} req/sec",
                                crate::bench::BENCH_REPS,
                                tp
                            ));
                            throughput = Some(tp);
                        }
                        Err(e) => logger.log(&format!(
                            "  [BENCH] candidate Redis load test FAILED: {e:#}. \
                             No speed ratio this trial."
                        )),
                    }
                    match BenchmarkRunner::probe_redis_write_async(
                        &sandbox.network_name(),
                        "candidate",
                        port,
                    )
                    .await
                    {
                        Ok(w) => logger.log(&format!(
                            "  Durability probe (unscored, fsync-bound): SET {:.0} req/sec",
                            w
                        )),
                        Err(e) => logger.log(&format!(
                            "  [BENCH] write probe unavailable: {e:#} (durability is verified by the AOF stage, not this number)"
                        )),
                    }
                    if config.batch_reference.is_none() {
                        logger.log("[BENCH] Calibrating live official Redis baseline (redis:alpine on port 6380)...");
                        match BenchmarkRunner::run_redis_benchmark_async(
                            &sandbox.network_name(),
                            "ref-redis",
                            6379,
                        )
                        .await
                        {
                            Ok(ref_tp) => {
                                logger.log(&format!(
                                    "  Official Reference Redis: {:.0} req/sec",
                                    ref_tp
                                ));
                                reference_throughput = Some(ref_tp);
                            }
                            // Never silent. A reference that fails to
                            // measure leaves throughput_req_sec populated
                            // with nothing to divide by, and the row looks
                            // merely unanchored - indistinguishable from a
                            // task that has no reference by design.
                            Err(e) => logger.log(&format!(
                                "  [BENCH] live Redis reference FAILED: {e:#}. \
                                 Throughput will be reported but NOT ranked."
                            )),
                        }
                    }
                }
                TaskType::Http => {
                    if let Ok(tp) = BenchmarkRunner::run_wrk_benchmark_async(
                        &sandbox.network_name(),
                        "candidate",
                        port,
                    )
                    .await
                    {
                        logger.log(&format!(
                            "  Candidate Throughput (median of {}): {:.0} req/sec",
                            crate::bench::BENCH_REPS,
                            tp
                        ));
                        throughput = Some(tp);
                    }
                    if config.batch_reference.is_none() {
                        logger.log("[BENCH] Calibrating live official Nginx baseline (nginx:alpine on port 8081)...");
                        match BenchmarkRunner::run_wrk_benchmark_async(
                            &sandbox.network_name(),
                            "ref-http",
                            80,
                        )
                        .await
                        {
                            Ok(ref_tp) => {
                                logger.log(&format!(
                                    "  Official Reference Nginx: {:.0} req/sec",
                                    ref_tp
                                ));
                                reference_throughput = Some(ref_tp);
                            }
                            // See the Redis arm: a swallowed failure here
                            // is indistinguishable from having no reference.
                            Err(e) => logger.log(&format!(
                                "  [BENCH] live Nginx reference FAILED: {e:#}. \
                                 Throughput will be reported but NOT ranked."
                            )),
                        }
                    }
                }
                TaskType::Wal | TaskType::Pubsub | TaskType::Binproto => {
                    if let Some(tp) = extra_throughput {
                        let unit = match config.task {
                            TaskType::Wal => "msgs/sec",
                            TaskType::Pubsub => "deliveries/sec",
                            _ => "ops/sec",
                        };
                        logger.log(&format!(
                            "  Candidate Throughput (verifier soak, median-grade): {:.0} {}",
                            tp, unit
                        ));
                        throughput = Some(tp);
                    }
                    // Verifier-timed, no external reference: reported but
                    // unscored until a reference harness exists.
                    logger.log(
                        "[BENCH] No reference implementation for this task; timing reported, score unscored.",
                    );
                }
                TaskType::Proxy => {
                    if let Ok(tp) = BenchmarkRunner::run_wrk_benchmark_async(
                        &sandbox.network_name(),
                        "candidate",
                        port,
                    )
                    .await
                    {
                        logger.log(&format!(
                            "  Candidate Throughput (median of {}): {:.0} req/sec",
                            crate::bench::BENCH_REPS,
                            tp
                        ));
                        throughput = Some(tp);
                    }
                    if config.batch_reference.is_none() {
                        // Same nginx denominator as http: a proxy cannot
                        // beat direct. Live per-trial when no batch ref.
                        logger.log("[BENCH] Calibrating live official Nginx baseline (nginx:alpine on port 8081)...");
                        match BenchmarkRunner::run_wrk_benchmark_async(
                            &sandbox.network_name(),
                            "ref-http",
                            80,
                        )
                        .await
                        {
                            Ok(ref_tp) => {
                                logger.log(&format!(
                                    "  Official Reference Nginx: {:.0} req/sec",
                                    ref_tp
                                ));
                                reference_throughput = Some(ref_tp);
                            }
                            // See the Redis arm: a swallowed failure here
                            // is indistinguishable from having no reference.
                            Err(e) => logger.log(&format!(
                                "  [BENCH] live Nginx reference FAILED: {e:#}. \
                                 Throughput will be reported but NOT ranked."
                            )),
                        }
                    }
                }
                TaskType::Pdf => {
                    let verifier = PdfVerifier::new(port, Some(&verifier_seed));
                    if let Some(tp) = verifier.measure_throughput().await {
                        logger.log(&format!("  Candidate Throughput: {:.0} pages/sec", tp));
                        throughput = Some(tp);
                    }
                    // No external PDF reference exists; candidate timing is
                    // reported but unscored (throughput_score stays None).
                    logger.log(
                        "[BENCH] No reference implementation for PDF; timing reported, score unscored.",
                    );
                }
            }

            // Batch denominator wins when present: every trial of the batch
            // scores against the same pre-batch median; the per-trial live
            // measurement above is skipped in that case.
            if let Some(batch) = config.batch_reference {
                logger.log(&format!(
                    "  Batch reference (shared pre-batch median): {:.0} req/sec",
                    batch
                ));
                reference_throughput = Some(batch);
            }

            // Two tasks have no valid throughput denominator, and ranking
            // them on one asserts a comparison that cannot exist:
            //
            //   proxy - divided by DIRECT nginx while it has to forward to
            //     a three-upstream pool. It is capped below 1.0 by
            //     construction, so every proxy row reads as a deficit
            //     against a ceiling it was never racing.
            //   pdf   - the reference is our own ~275-line Python
            //     extractor. That is a parity oracle, not a ceiling: it is
            //     expected to be SLOWER than candidates, so the ratio
            //     rewards distance from our own slow implementation.
            //
            // The candidate's raw rate is still reported - it is a real
            // measurement - but it is not ranked, and the board shows no
            // percentage for these tasks rather than a wrong one.
            if matches!(config.task, TaskType::Proxy | TaskType::Pdf)
                && reference_throughput.is_some()
            {
                logger.log(
                    "[BENCH] No valid throughput denominator for this task \
                     (reference is structurally unreachable); rate reported, not ranked.",
                );
                reference_throughput = None;
            }
            // `baseline_source` describes where the denominator came from,
            // so it must follow reference_throughput: a batch reference
            // that was just discarded is not a "batch" baseline.
            baseline_source = match (config.batch_reference.is_some(), reference_throughput) {
                (true, Some(_)) => Some("batch".to_string()),
                (false, Some(_)) => Some("live".to_string()),
                _ => None,
            };

            if let (Some(cand_tp), Some(ref_tp)) = (throughput, reference_throughput) {
                if ref_tp > 0.0 {
                    let ratio = (cand_tp / ref_tp) * 100.0;
                    logger.log(&format!(
                        "  ⚡ Relative Performance: {:.1}% of official reference",
                        ratio
                    ));
                }
            }
        }

        // 8b. Campaign perf gate (acceptance includes stage 12): the
        // candidate must beat its own genesis-stage GET throughput,
        // measured with the same bench method, by >= PERF_GATE_RATIO. Only
        // applied when the protocol checks passed (a failed stage is
        // already a failure) and a genesis baseline exists; the pass_rate
        // recompute below uses the acceptance subset total.
        if config
            .campaign_stage
            .as_ref()
            .map(|c| c.accepted_stages.contains(&12))
            .unwrap_or(false)
            && stage_results.iter().any(|s| s.stage == 12 && s.passed)
        {
            let accepted_ids: Option<&[u32]> = config
                .campaign_stage
                .as_ref()
                .map(|c| c.accepted_stages.as_slice());
            let (ok, detail) = perf_gate_verdict(config.baseline_rps, throughput);
            if ok {
                logger.log(&format!("  [PERF] throughput gate ok: {detail}"));
            } else {
                logger.log(&format!("  [PERF] throughput gate FAILED: {detail}"));
                if let Some(s) = stage_results.iter_mut().find(|s| s.stage == 12) {
                    s.passed = false;
                    s.error = Some(detail);
                }
                passed_stages = stage_results
                    .iter()
                    .filter(|s| {
                        s.passed && accepted_ids.map(|a| a.contains(&s.stage)).unwrap_or(true)
                    })
                    .count() as u32;
                pass_rate = if total_stages > 0 {
                    passed_stages as f64 / total_stages as f64 * 100.0
                } else {
                    0.0
                };
            }
        }

        // 9. Clean up containers
        sandbox.cleanup();

        if let Some(ref cf) = cancel_flag {
            if cf.load(Ordering::SeqCst) {
                logger.log("[PIPELINE] Run cancelled before archiving.");
                return Err(anyhow!("Benchmark run cancelled by user"));
            }
        }

        // 10. Cost & Performance Accounting
        let completed_at = Utc::now().to_rfc3339();
        let duration_seconds = start_time.elapsed().as_secs_f64();
        let total_run_tokens = omp_stats
            .total_tokens
            .max(prompt_tokens + cached_tokens + completion_tokens);

        let pricing = ModelPricing::for_model_async(&config.model).await;
        let breakdown =
            pricing.compute_cost_with_cache(prompt_tokens, cached_tokens, completion_tokens);

        let mut live_spend_delta = None;
        if let (Some(key), Some(init)) = (&config.api_key, initial_spend) {
            sleep(Duration::from_millis(1500)).await;
            if let Some(fin) = ModelPricing::query_openrouter_key_usage(key).await {
                if fin >= init {
                    live_spend_delta = Some(fin - init);
                }
            }
        }

        // Cost accounting: the live key-usage delta is ground truth only when
        // it moves; a zero delta is weekly-counter lag, not zero spend (see
        // resolve_api_cost). Both measurements stay in the record so a
        // disagreement is auditable after the fact.
        let (cost_usd, cost_source) =
            ModelPricing::resolve_api_cost(live_spend_delta, breakdown.total_cost_usd);
        if cost_source == "formula_zero_delta" {
            logger.log(&format!(
                "[COST] Live key usage read $0.00 but tokens price at ${:.4} — recording the formula cost (counter lag).",
                breakdown.total_cost_usd
            ));
        }
        let effective_cost_usd = ModelPricing::calculate_effective_cost(
            cost_usd,
            duration_seconds,
            total_run_tokens,
            cached_tokens,
        );
        let efficiency_score =
            ModelPricing::calculate_efficiency_score(pass_rate, effective_cost_usd);
        let throughput_score = ModelPricing::calculate_throughput_score_for_task(
            pass_rate,
            effective_cost_usd,
            throughput,
            reference_throughput,
            Some(&config.task.to_string()),
        );
        if throughput.is_some() && reference_throughput.is_none() {
            logger.log(
                "[BENCH] Timing recorded without a live reference: throughput_score unscored (--).",
            );
        }

        let lang = LeaderboardManager::detect_language(&config.workdir);
        let scanned_files = RunArchiver::scan_workspace_files(&config.workdir);

        // Canary echo scan (advisory, never a verdict): a workspace that
        // reproduces a benchmark canary GUID is echoing benchmark prompt
        // material — contamination evidence worth surfacing, not a cheat.
        let canary_hits = crate::report::canary::scan_workspace_for_canary(&config.workdir);
        if !canary_hits.is_empty() {
            logger.log(&format!(
                "[CANARY] {} echo(s) of benchmark markers in the candidate workspace: {}",
                canary_hits.len(),
                canary_hits
                    .iter()
                    .map(|(f, g)| format!("{f} -> {g}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }

        // Single verdict chain shared by the manifest and the result
        // row: trial results carry status so merges and boards never
        // re-derive (or drop) it.
        let trial_status: String = if infra_error && !disqualified {
            // `infra_error` first: a harness fault that also scored 0% must
            // not be archived as `failed_tests`, which is a model verdict.
            // Ordering matters - a disqualified run is a model-side result and
            // keeps its own status even if setup also faulted.
            "infra_error".to_string()
        } else if disqualified {
            "disqualified_cheat".to_string()
        } else if pass_rate == 100.0 {
            // Solved is solved: how the agent died (turn limit reached
            // after the files landed, 429 post-completion, non-zero exit
            // with a working candidate) doesn't unsolve a 100% verified
            // candidate. agent_failed stays on PipelineOutput for
            // telemetry; the status reflects the verdict.
            "completed".to_string()
        } else if agent_failed
            || is_zero_output_non_result(config.eval_only, total_run_tokens, scanned_files.len())
        {
            // Defense in depth: a session that ended Ok with zero model
            // tokens and an empty workspace never ran the model — that is a
            // harness/provider fault, not a 0% verdict. (Live: opencode
            // free-tier quota retries once surfaced exactly this shape as
            // `failed_tests` and ranked 0%.) Eval-only runs and any run with
            // real output stay on their own paths.
            "agent_failed".to_string()
        } else {
            "failed_tests".to_string()
        };
        let manifest = RunManifest {
            run_id: run_id.clone(),
            model: config.model.clone(),
            task: config.task.to_string(),
            status: trial_status.clone(),
            language: lang.clone(),
            effort: Some(config.effort.clone()),
            effort_effective: omp_stats.effort_effective.clone(),
            started_at,
            completed_at: completed_at.clone(),
            duration_seconds,
            turns: Some(omp_stats.steps_taken),
            pass_rate,
            passed_stages,
            total_stages,
            stages: stage_results,
            robustness: extra_robustness,
            throughput_req_sec: throughput,
            reference_throughput_req_sec: reference_throughput,
            baseline_source: baseline_source.clone(),
            ref_drift: None,
            // Anchored rows only: an unanchored row must stay "legacy" even
            // after its task gains a reference image; otherwise regime_of
            // implies comparable ratios where throughput_score is None.
            ref_image: if reference_throughput.is_some() {
                crate::sandbox::docker::ref_image_for_task(&config.task.to_string())
                    .map(|s| s.to_string())
            } else {
                None
            },
            // Recorded so a reader can tell an enforced run from an open one
            // without trusting the archive tooling. A run with egress_enforced
            // false (or absent, from before this field) made no isolation claim.
            egress_enforced: Some(crate::sandbox::docker::egress_filter_enabled()),
            tokens: RunTokenUsage {
                prompt_tokens,
                cached_tokens,
                completion_tokens,
                total_tokens: total_run_tokens,
            },
            cost_usd,
            cost_source: Some(cost_source.to_string()),
            cost_formula_usd: Some(breakdown.total_cost_usd),
            effective_cost_usd: Some(effective_cost_usd),
            savings_percent: breakdown.savings_percent,
            efficiency_score,
            throughput_score,
            files: scanned_files,
            canary_echoed: (!canary_hits.is_empty()).then_some(canary_hits),
            // No live cutoff recording: annotate-cutoff flags accepted
            // cutoffs post-hoc, uniformly for old and new rows.
            cutoff_turn1: None,
            env: None,
            git_commit: None,
            rules_version: Some(crate::report::rules_fingerprint()),
            // Sampling: OMP passes provider defaults through (no explicit
            // temp/top_p/seed knobs), so None = reproducible-by-default.
            // Prompt hash pins the graded input for this run.
            temperature: None,
            top_p: None,
            seed: None,
            prompt_sha: {
                let p = crate::config::get_repo_root()
                    .join("tasks")
                    .join(config.task.to_string())
                    .join("prompt.md");
                std::fs::read(&p).ok().map(|b| {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    b.hash(&mut h);
                    format!("{:016x}", h.finish())
                })
            },
            omp_version: crate::sandbox::omp_updater::get_installed_sandbox_version(),
            harness: Some(config.harness.to_string()),
            harness_version: Some(match config.harness {
                Harness::Omp => {
                    crate::sandbox::omp_updater::get_installed_sandbox_version().unwrap_or_default()
                }
                Harness::Opencode => OPENCODE_VERSION.to_string(),
            }),
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            trial_pass_rates: Vec::new(),
            protocol: None,
            solved: Some(pass_rate >= 100.0),
            solved_at: if pass_rate >= 100.0 {
                if was_repaired {
                    Some(feedback_rounds_used + 1)
                } else {
                    Some(1)
                }
            } else {
                None
            },
            best_stage_score: Some(best_pass_rate),
            repaired: if was_repaired {
                Some(true)
            } else if pass_rate >= 100.0 {
                Some(false)
            } else {
                None
            },
            repair_attempted: if config.feedback_rounds > 0 {
                Some(feedback_rounds_used > 0)
            } else {
                None
            },
            feedback_rounds: if config.feedback_rounds > 0 {
                Some(feedback_rounds_used)
            } else {
                None
            },
            is_published: None,
            method_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };

        let benchmark_result = BenchmarkRunResult {
            id: run_id,
            model: config.model.clone(),
            task: config.task.to_string(),
            language: lang,
            effort: Some(config.effort.clone()),
            effort_effective: omp_stats.effort_effective.clone(),
            harness: Some(config.harness.to_string()),
            status: Some(trial_status.clone()),
            pass_rate,
            passed_stages,
            total_stages,
            throughput_req_sec: throughput,
            reference_throughput_req_sec: reference_throughput,
            baseline_source: baseline_source.clone(),
            ref_drift: None,
            // Same anchored-only gate as the manifest above: regime_of must
            // never show a digest on a row with no anchored denominator.
            ref_image: if reference_throughput.is_some() {
                crate::sandbox::docker::ref_image_for_task(&config.task.to_string())
                    .map(|s| s.to_string())
            } else {
                None
            },
            prompt_tokens,
            cached_tokens,
            completion_tokens,
            total_tokens: total_run_tokens,
            total_cost_usd: cost_usd,
            cost_source: Some(cost_source.to_string()),
            cost_formula_usd: Some(breakdown.total_cost_usd),
            effective_cost_usd: Some(effective_cost_usd),
            duration_seconds: Some(duration_seconds),
            turns: Some(omp_stats.steps_taken),
            savings_percent: breakdown.savings_percent,
            efficiency_score,
            throughput_score,
            timestamp: completed_at,
            method_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            subruns: Vec::new(),
            total_trials: Some(1),
            rules_version: Some(crate::report::rules_fingerprint()),
            graded_under: Some(crate::report::rules_fingerprint()),
            pass_at_1: Some(if pass_rate >= 100.0 { 100.0 } else { 0.0 }),
            pass_at_k: Some(if pass_rate >= 100.0 { 100.0 } else { 0.0 }),
            avg_pass_rate: Some(pass_rate),
            pass_at_1_lo: Some({
                let c = usize::from(pass_rate >= 100.0);
                crate::report::leaderboard::wilson_interval_95(1, c).0
            }),
            pass_at_1_hi: Some({
                let c = usize::from(pass_rate >= 100.0);
                crate::report::leaderboard::wilson_interval_95(1, c).1
            }),
            stability: None,
            frontier_fraction: None,
            protocol: None,
            solved: Some(pass_rate >= 100.0),
            solved_at: if pass_rate >= 100.0 {
                if was_repaired {
                    Some(feedback_rounds_used + 1)
                } else {
                    Some(1)
                }
            } else {
                None
            },
            best_stage_score: Some(best_pass_rate),
            repaired: if was_repaired {
                Some(true)
            } else if pass_rate >= 100.0 {
                Some(false)
            } else {
                None
            },
            repair_attempted: if config.feedback_rounds > 0 {
                Some(feedback_rounds_used > 0)
            } else {
                None
            },
            feedback_rounds: if config.feedback_rounds > 0 {
                Some(feedback_rounds_used)
            } else {
                None
            },
        };

        logger.log(&format!(
            "[TELEMETRY] turn={} max_turns={} spent={:.6} tokens={}",
            omp_stats.steps_taken, config.max_turns, cost_usd, total_run_tokens
        ));

        // 11. Optional Archiving and Leaderboard update
        if config.save_results {
            let runs_dir = RunArchiver::resolve_runs_dir();
            let console_log = buffering_logger.get_logs();
            let _ = RunArchiver::archive_run(&runs_dir, &manifest, &config.workdir, &console_log);
            // Runs index mirror (best-effort, never fatal): the new trial
            // manifest is queryable via /api/runs without an FS scan.
            // archive_run stays FS-pure; this callsite owns the DB write.
            // Publish hygiene: test/CI probe runs never reach the shared store
            // (mirrors the save_result `test_` guard, which covers parents + JSON).
            if !manifest.run_id.starts_with("test_") {
                if let Ok(store) =
                    crate::report::Store::open(&crate::report::Store::resolve_db_path())
                {
                    let _ = store.upsert_run_manifest(&manifest);
                }
            }
            let results_dir = get_repo_root().join("results");
            let _ = LeaderboardManager::save_result(
                results_dir.to_str().unwrap_or("./results"),
                &benchmark_result,
            );
            // 12. LLM judge code review (best-effort; never fails the run)
            {
                let judge_model = config
                    .review_model
                    .clone()
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| "openrouter/meta/muse-spark-1.3-contributor".to_string());
                let task_name = config.task.to_string();
                let task_prompt = std::fs::read_to_string(
                    get_repo_root()
                        .join("tasks")
                        .join(&task_name)
                        .join("prompt.md"),
                )
                .unwrap_or_default();
                match RunJudge::judge_run(
                    config.api_key.as_deref(),
                    &judge_model,
                    &task_name,
                    &task_prompt,
                    &config.workdir,
                    &manifest,
                )
                .await
                {
                    Ok(review) => {
                        let review_path = runs_dir.join(&manifest.run_id).join("review.json");
                        if let Ok(text) = serde_json::to_string_pretty(&review) {
                            let _ = std::fs::write(&review_path, text);
                        }
                        let rev_msg = format!(
                            "[REVIEW] Judge {} scored {} strengths, {} weaknesses",
                            judge_model,
                            review.good.len(),
                            review.bad.len()
                        );
                        buffering_logger.log(&rev_msg);
                        let log_path = runs_dir.join(&manifest.run_id).join("console.log");
                        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_path)
                        {
                            use std::io::Write;
                            let _ = writeln!(f, "[{}] {}", Utc::now().format("%H:%M:%S"), rev_msg);
                        }
                    }
                    Err(e) => {
                        let skip_msg = format!("[REVIEW] Skipped: {}", e);
                        buffering_logger.log(&skip_msg);
                        let log_path = runs_dir.join(&manifest.run_id).join("console.log");
                        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_path)
                        {
                            use std::io::Write;
                            let _ = writeln!(f, "[{}] {}", Utc::now().format("%H:%M:%S"), skip_msg);
                        }
                    }
                }
            }
            // 13. Refresh the static portal mirror (includes any new review)
            let portal_out = get_repo_root().join("portal");
            let _ = PortalExporter::export(
                runs_dir.to_str().unwrap_or("./runs"),
                results_dir.to_str().unwrap_or("./results"),
                portal_out.to_str().unwrap_or("./portal"),
                "",
                200,
            );
        }
        // Revoke the capped run key on the happy path. Early returns above
        // (cancel/infra-error) revoke via the same call — see the helper:
        // best-effort, logged, never fails the batch. Panic-unwind skips
        // revoke (documented residual: 24h expiry bounds it).
        if let Some(hash) = revoke_guard.hash.take() {
            match crate::cost::keys::revoke_run_key(&hash).await {
                Ok(()) => logger.log(&format!(
                    "[KEY] revoked run key (hash {}…)",
                    &hash[..hash.len().min(8)]
                )),
                Err(e) => logger.log(&format!(
                    "[KEY] WARNING: revoke failed ({e:#}); key expires in 24h"
                )),
            }
        }
        Ok(PipelineOutput {
            manifest,
            benchmark_result,
            agent_failed,
            disqualified,
            infra_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyLogger;
    impl PipelineLogger for DummyLogger {
        fn log(&self, _msg: &str) {}
    }

    /// Logger capturing every line, for asserting on pipeline decisions.
    #[derive(Default)]
    struct RecordingLogger(std::sync::Mutex<Vec<String>>);

    impl RecordingLogger {
        fn contains(&self, needle: &str) -> bool {
            self.0.lock().unwrap().iter().any(|l| l.contains(needle))
        }
    }

    impl PipelineLogger for RecordingLogger {
        fn log(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
    }

    /// Temp fixture root unique to this process and test tag.
    fn covtest_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("covtest-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Minimal RESP candidate: raw sockets + a thread per connection, so
    /// PING/ECHO/SET/GET/DEL/EXISTS pass stages 1-2 (and the advisory
    /// robustness probes), while the deliberately missing TTL/INCR/PUBSUB
    /// commands keep stages 3-5 red. That keeps pass_rate < 100%, so the
    /// pipeline skips the bench while still building the full manifest.
    const MOCK_RESP_SERVER_PY: &str = r#"import socket
import sys
import threading

STORE = {}
LOCK = threading.Lock()


def line(f):
    raw = f.readline()
    if not raw:
        raise EOFError("eof")
    return raw.rstrip(b"\r\n")


def read_args(f):
    first = f.read(1)
    if not first:
        raise EOFError("eof")
    if first != b"*":
        raise ValueError("unsupported frame")
    count = int(line(f))
    args = []
    for _ in range(count):
        kind = f.read(1)
        if kind != b"$":
            raise ValueError("unsupported type")
        n = int(line(f))
        data = f.read(n)
        f.read(2)
        args.append(data)
    return args


def dispatch(args):
    cmd = args[0].upper()
    if cmd == b"PING":
        return b"+PONG\r\n"
    if cmd == b"ECHO" and len(args) == 2:
        v = args[1]
        return b"$%d\r\n%s\r\n" % (len(v), v)
    if cmd == b"SET" and len(args) >= 3:
        with LOCK:
            STORE[args[1]] = args[2]
        return b"+OK\r\n"
    if cmd == b"GET" and len(args) == 2:
        with LOCK:
            v = STORE.get(args[1])
        if v is None:
            return b"$-1\r\n"
        return b"$%d\r\n%s\r\n" % (len(v), v)
    if cmd == b"DEL" and len(args) == 2:
        with LOCK:
            hit = STORE.pop(args[1], None) is not None
        return b":%d\r\n" % (1 if hit else 0)
    if cmd == b"EXISTS" and len(args) == 2:
        with LOCK:
            hit = args[1] in STORE
        return b":%d\r\n" % (1 if hit else 0)
    return b"-ERR unknown command\r\n"


def handle(conn):
    try:
        f = conn.makefile("rb")
        while True:
            args = read_args(f)
            if not args:
                continue
            conn.sendall(dispatch(args))
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def main():
    port = int(sys.argv[1])
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(128)
    print("covtest-mock listening on %d" % port, flush=True)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


main()
"#;

    /// Stage-2 live fixture: RESP on `argv[1]` + SDB1 v2 admin on 6395,
    /// one shared store whose values live in an append log under
    /// SDB_DATA_DIR (offset index in memory), idle-gated compaction to
    /// <= 3x live bytes, SIGKILL-safe (flush per write, partial tail
    /// dropped on replay). Implements exactly what stages 14-16 grade.
    const STAGE2_MOCK_SERVER_PY: &str = r#"import os
import socket
import struct
import sys
import threading
import time
import zlib

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 6379
# Real campaigns run on worker 0 where the published admin port is 6395;
# eval-only runs on other workers pass the offset host port the harness
# publishes 1:1.
ADMIN_PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 6395
DATA = os.environ.get("SDB_DATA_DIR", "/tmp/sdbmock")
os.makedirs(DATA, exist_ok=True)
LOGF = os.path.join(DATA, "store.log")
TMPF = os.path.join(DATA, "store.tmp")

U32 = struct.Struct(">I")


class Store(object):
    """Offset index in memory, values on disk in an append log."""

    def __init__(self):
        self.lock = threading.Lock()
        self.index = {}
        self.live = 0
        self.last_write = time.time()
        self.f = open(LOGF, "a+b", buffering=0)
        self.end = 0
        self.conns = 0
        self.cmds = 0
        self.hits = 0
        self.misses = 0
        self.started = time.time()
        self.replay()

    def _set_rec(self, key, value):
        return b"S" + U32.pack(len(key)) + key + U32.pack(len(value)) + value

    def _del_rec(self, key):
        return b"D" + U32.pack(len(key)) + key

    def replay(self):
        self.f.close()
        try:
            f = open(LOGF, "rb")
        except IOError:
            self.end = 0
            self.f = open(LOGF, "a+b", buffering=0)
            return
        good = 0
        while True:
            tag = f.read(1)
            if not tag:
                break
            if tag == b"S":
                hdr = f.read(4)
                if len(hdr) < 4:
                    break
                klen = U32.unpack(hdr)[0]
                key = f.read(klen)
                if len(key) < klen:
                    break
                hdr = f.read(4)
                if len(hdr) < 4:
                    break
                vlen = U32.unpack(hdr)[0]
                voff = f.tell()
                f.seek(vlen, 1)
                if f.tell() - voff < vlen:
                    break
                old = self.index.get(key)
                if old:
                    self.live -= old[1]
                self.index[key] = (voff, vlen)
                self.live += vlen
                good = f.tell()
            elif tag == b"D":
                hdr = f.read(4)
                if len(hdr) < 4:
                    break
                klen = U32.unpack(hdr)[0]
                key = f.read(klen)
                if len(key) < klen:
                    break
                old = self.index.pop(key, None)
                if old:
                    self.live -= old[1]
                good = f.tell()
            else:
                break
        f.close()
        # Drop a partial tail left by a SIGKILL mid-append.
        os.truncate(LOGF, good)
        self.end = good
        self.f = open(LOGF, "a+b", buffering=0)

    def set(self, key, value):
        with self.lock:
            self.cmds += 1
            old = self.index.get(key)
            if old:
                self.live -= old[1]
            off = self.end
            rec = self._set_rec(key, value)
            self.f.write(rec)
            os.fsync(self.f.fileno())
            self.end = off + len(rec)
            self.index[key] = (off + 1 + 4 + len(key) + 4, len(value))
            self.live += len(value)
            self.last_write = time.time()

    def get(self, key):
        with self.lock:
            self.cmds += 1
            ent = self.index.get(key)
            if not ent:
                self.misses += 1
                return None
            self.hits += 1
            return os.pread(self.f.fileno(), ent[1], ent[0])

    def delete(self, keys):
        n = 0
        with self.lock:
            for k in keys:
                self.cmds += 1
                old = self.index.pop(k, None)
                rec = self._del_rec(k)
                self.f.write(rec)
                self.end += len(rec)
                if old:
                    self.live -= old[1]
                    n += 1
            os.fsync(self.f.fileno())
            self.last_write = time.time()
        return n

    def exists(self, key):
        with self.lock:
            self.cmds += 1
            return key in self.index

    def stats(self):
        with self.lock:
            return (
                "total_connections_received:%d\n"
                "total_commands_processed:%d\n"
                "keyspace_hits:%d\n"
                "keyspace_misses:%d\n"
                "expired_keys:0\nrejected_connections:0\n"
                "connected_clients:1\nuptime_in_seconds:%d\n"
                % (
                    self.conns,
                    self.cmds,
                    self.hits,
                    self.misses,
                    int(time.time() - self.started),
                )
            )

    def compact(self):
        """Rewrite the log with live records only (idle + garbage gated)."""
        with self.lock:
            if time.time() - self.last_write < 0.8:
                return
            try:
                size = os.path.getsize(LOGF)
            except OSError:
                return
            if size <= max(3 * self.live, 4 * 1024 * 1024):
                return
            fd = self.f.fileno()
            new = {}
            tmp = open(TMPF, "wb", buffering=0)
            for key, (voff, vlen) in self.index.items():
                val = os.pread(fd, vlen, voff)
                pos = tmp.tell()
                tmp.write(self._set_rec(key, val))
                new[key] = (pos + 1 + 4 + len(key) + 4, vlen)
            tmp.close()
            self.f.close()
            os.replace(TMPF, LOGF)
            self.end = os.path.getsize(LOGF)
            self.f = open(LOGF, "a+b", buffering=0)
            self.index = new


def compactor(store):
    while True:
        time.sleep(0.5)
        try:
            store.compact()
        except Exception:
            pass


def read_line(f):
    raw = f.readline()
    if not raw:
        raise EOFError("eof")
    return raw.rstrip(b"\r\n")


def read_resp(f):
    first = f.read(1)
    if not first:
        return None
    if first != b"*":
        raise ValueError("bad frame")
    n = int(read_line(f))
    args = []
    for _ in range(n):
        kind = f.read(1)
        if kind != b"$":
            raise ValueError("bad bulk")
        ln = int(read_line(f))
        data = f.read(ln)
        if len(data) < ln:
            raise EOFError("eof mid bulk")
        f.read(2)
        args.append(data)
    return args


def resp_conn(conn, store):
    store.conns += 1
    f = conn.makefile("rb")
    out = conn.makefile("wb")
    try:
        while True:
            args = read_resp(f)
            if not args:
                break
            cmd = args[0].upper()
            if cmd == b"PING":
                out.write(b"+PONG\r\n")
            elif cmd == b"SET" and len(args) == 3:
                store.set(args[1], args[2])
                out.write(b"+OK\r\n")
            elif cmd == b"GET" and len(args) == 2:
                v = store.get(args[1])
                if v is None:
                    out.write(b"$-1\r\n")
                else:
                    out.write(b"$%d\r\n" % len(v) + v + b"\r\n")
            elif cmd == b"DEL" and len(args) >= 2:
                out.write(b":%d\r\n" % store.delete(args[1:]))
            elif cmd == b"EXISTS" and len(args) == 2:
                out.write(b":%d\r\n" % (1 if store.exists(args[1]) else 0))
            else:
                out.write(b"-ERR unknown\r\n")
            out.flush()
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def sdb1_conn(conn, store):
    store.conns += 1

    def reply(seq, op, status, payload=b""):
        return (
            b"SDB1"
            + struct.pack(
                ">HBBII", seq, op, status, len(payload), zlib.crc32(payload) & 0xFFFFFFFF
            )
            + payload
        )

    buf = b""
    try:
        while True:
            while len(buf) < 16:
                chunk = conn.recv(65536)
                if not chunk:
                    return
                buf += chunk
            if buf[:4] != b"SDB1":
                return
            seq, op, _status, ln, crc = struct.unpack(">HBBII", buf[4:16])
            if ln > 1024 * 1024:
                return
            while len(buf) < 16 + ln:
                chunk = conn.recv(65536)
                if not chunk:
                    return
                buf += chunk
            payload = buf[16 : 16 + ln]
            buf = buf[16 + ln :]
            if (zlib.crc32(payload) & 0xFFFFFFFF) != crc:
                conn.sendall(reply(seq, op, 1))
                continue
            if op == 1:
                conn.sendall(reply(seq, op, 0))
            elif op == 2:
                conn.sendall(reply(seq, op, 0, payload))
            elif op == 3:
                i = payload.find(b"\x00")
                if i < 0:
                    conn.sendall(reply(seq, op, 2))
                else:
                    store.set(payload[:i], payload[i + 1 :])
                    conn.sendall(reply(seq, op, 0))
            elif op == 4:
                v = store.get(payload)
                if v is None:
                    conn.sendall(reply(seq, op, 2))
                else:
                    conn.sendall(reply(seq, op, 0, v))
            elif op == 6:
                conn.sendall(reply(seq, 6, 0, store.stats().encode()))
            else:
                conn.sendall(reply(seq, op, 2))
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def serve(port, handler, store):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(128)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handler, args=(conn, store), daemon=True).start()


def main():
    store = Store()
    threading.Thread(target=compactor, args=(store,), daemon=True).start()
    threading.Thread(target=serve, args=(ADMIN_PORT, sdb1_conn, store), daemon=True).start()
    serve(PORT, resp_conn, store)


main()
"#;

    /// Write the stage-2 mock candidate into `dir`, RESP on `port`.
    fn write_stage2_mock(dir: &Path, port: u16, admin_port: u16) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("server.py"), STAGE2_MOCK_SERVER_PY).unwrap();
        let start =
            format!("#!/bin/bash\nexec python3 -u /workspace/server.py {port} {admin_port}\n");
        fs::write(dir.join("start.sh"), start).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir.join("start.sh"), fs::Permissions::from_mode(0o755));
        }
    }

    /// Proxy-campaign live fixture: a minimal reverse proxy that forwards
    /// `GET /` to `upstream1:80` (the run-network nginx pool) with valid
    /// HTTP/1.1 framing. Clears stage 1 and exercises the sdb-edge path.
    const PROXY_MOCK_SERVER_PY: &str = r#"import socket
import sys
import threading

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8091
UPSTREAM = ("upstream1", 80)


def fetch_upstream():
    up = socket.create_connection(UPSTREAM, timeout=5)
    try:
        up.sendall(
            b"GET / HTTP/1.1\r\nHost: upstream1\r\nX-Forwarded-For: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        data = b""
        while True:
            chunk = up.recv(65536)
            if not chunk:
                break
            data += chunk
    finally:
        up.close()
    head, _, body = data.partition(b"\r\n\r\n")
    status_line = head.split(b"\r\n")[0] if head else b"HTTP/1.1 502 Bad Gateway"
    if not status_line.startswith(b"HTTP/1.1 200"):
        return b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 4\r\nConnection: close\r\n\r\nboom"
    out = bytearray()
    out += status_line + b"\r\n"
    out += b"Content-Length: %d\r\n" % len(body)
    out += b"Connection: close\r\n\r\n"
    out += body
    return bytes(out)


def handle(conn):
    try:
        conn.settimeout(5)
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = conn.recv(4096)
            if not chunk:
                return
            buf += chunk
        request_line = buf.split(b"\r\n")[0]
        if request_line.startswith(b"GET "):
            conn.sendall(fetch_upstream())
        else:
            conn.sendall(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def serve():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", PORT))
    srv.listen(128)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


serve()
"#;

    /// Write the proxy mock into `dir`, listening on `port`.
    fn write_proxy_mock(dir: &Path, port: u16) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("proxy.py"), PROXY_MOCK_SERVER_PY).unwrap();
        let start = format!("#!/bin/bash\nexec python3 -u /workspace/proxy.py {port}\n");
        fs::write(dir.join("start.sh"), start).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir.join("start.sh"), fs::Permissions::from_mode(0o755));
        }
    }

    /// Write the mock candidate into `dir`, listening on `port`.
    /// `boot_sleep_secs` delays the listener (used to keep a run inside
    /// verification while a test flips the cancel flag).
    fn write_mock_candidate(dir: &Path, port: u16, boot_sleep_secs: u64) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("server.py"), MOCK_RESP_SERVER_PY).unwrap();
        let start = format!(
            "#!/bin/bash\nsleep {boot_sleep_secs}\nexec python3 -u /workspace/server.py {port}\n"
        );
        fs::write(dir.join("start.sh"), start).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir.join("start.sh"), fs::Permissions::from_mode(0o755));
        }
    }

    /// Eval-only Redis config on a high worker offset: candidate/ref ports
    /// live at base + worker*100, far away from the worker-0 ports
    /// (6379/6380/8090) used by live campaign cells.
    fn covtest_config(workdir: PathBuf, worker: u8, tag: &str) -> BenchmarkConfig {
        BenchmarkConfig {
            model: "test_model".to_string(),
            task: TaskType::Redis,
            effort: "low".to_string(),
            harness: Harness::Omp,
            max_turns: 1,
            budget_usd: 0.10,
            timeout_min: 1,
            api_key: None,
            workdir,
            eval_only: true,
            run_id: Some(format!("test_covtest_{tag}_{}", std::process::id())),
            save_results: false,
            review_model: None,
            batch_reference: None,
            worker,
            feedback_rounds: 0,
            campaign_stage: None,
            baseline_rps: None,
        }
    }

    /// The pipeline's own cleanup deliberately keeps networks younger than
    /// its staleness window (a parallel worker may still be attaching). Tests
    /// know their run is over, so reap their own named network explicitly.
    fn cleanup_covtest_network(tag: &str) {
        let net = format!("subdollar-net-test_covtest_{tag}_{}", std::process::id());
        let _ = std::process::Command::new("docker")
            .args(["network", "rm", &net])
            .output();
    }

    /// Stage 9 end-to-end against real fixtures (needs Docker + the
    /// `subdollar-sandbox` image, like the other container-backed sandbox
    /// tests). Builds a temp git repo with genuine stage-0/stage-1 commits,
    /// a green offline `test.sh` that checks the stage-1 marker, and a
    /// coverage self-report, then drives `run_tests_ci_blocking`:
    ///   - the good fixture must PASS (suite green twice, then red once the
    ///     stage-1 revert lands in the scratch copy);
    ///   - a toothless suite (always green) must FAIL with the
    ///     "stayed green" verdict.
    #[test]
    fn test_tests_ci_sensitivity_end_to_end() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("s9-ci-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let good = root.join("good");
        std::fs::create_dir_all(&good).unwrap();

        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&good)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(good.join("main.py"), "MARKER = \"off\"\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 0: MVP [pass]"]);
        std::fs::write(good.join("main.py"), "MARKER = \"on\"\nEXPIRY = True\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 1: Expiry [pass]"]);
        std::fs::write(good.join("coverage.txt"), "coverage: 85.0%\n").unwrap();
        std::fs::write(
            good.join("test.sh"),
            "#!/bin/sh\nset -e\ngrep -q '^EXPIRY = True' main.py\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                good.join("test.sh"),
                std::fs::Permissions::from_mode(0o755),
            );
        }

        let st = run_tests_ci_blocking(
            SandboxManager::with_id(&format!("s9-good-{}", std::process::id())),
            good.clone(),
            Arc::new(DummyLogger),
        );
        assert!(st.passed, "good fixture must pass: {:?}", st.error);

        // Toothless: suite always green -> the stage-1 revert cannot turn it
        // red and the gate must reject it.
        std::fs::write(good.join("test.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        let st = run_tests_ci_blocking(
            SandboxManager::with_id(&format!("s9-bad-{}", std::process::id())),
            good.clone(),
            Arc::new(DummyLogger),
        );
        assert_eq!(st.stage, 9);
        assert!(!st.passed, "toothless suite must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("stayed green"),
            "expected 'stayed green' verdict: {:?}",
            st.error
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_perf_gate_verdict() {
        // 1.3x boundary: exactly 130 passes, 129 fails with the ratio shown.
        let (ok, detail) = perf_gate_verdict(Some(100.0), Some(130.0));
        assert!(ok, "{detail}");
        let (ok, detail) = perf_gate_verdict(Some(100.0), Some(129.0));
        assert!(!ok);
        assert!(detail.contains("1.29x"), "{detail}");
        // Baseline present, measurement missing: fail (no number, no gate).
        let (ok, detail) = perf_gate_verdict(Some(100.0), None);
        assert!(!ok);
        assert!(detail.contains("130"), "{detail}");
        // No baseline (single-shot / genesis measurement missing): skipped.
        assert!(perf_gate_verdict(None, Some(500.0)).0);
        assert!(perf_gate_verdict(None, None).0);
        // Degenerate baseline: skipped, never a division by zero.
        assert!(perf_gate_verdict(Some(0.0), Some(1.0)).0);
    }

    #[test]
    fn test_initial_turn_cap_checkpoints_only_with_feedback() {
        // No cap configured: unlimited either way.
        assert_eq!(initial_turn_cap(0, 0, false), None);
        assert_eq!(initial_turn_cap(0, 2, false), None);
        assert_eq!(initial_turn_cap(0, 2, true), None);
        // Feedback on, single-shot: checkpoint at 50 (or the smaller max).
        assert_eq!(initial_turn_cap(100, 2, false), Some(50));
        assert_eq!(initial_turn_cap(30, 1, false), Some(30));
        // Feedback off: full budget up front.
        assert_eq!(initial_turn_cap(100, 0, false), Some(100));
        // Campaign reserve (D6 option b, amended floor 12): a quarter of the
        // pool is kept for the repair round, at least 12 turns and at most
        // half the pool; big pools are unchanged (the 50-cap binds first).
        assert_eq!(initial_turn_cap(100, 2, true), Some(50));
        assert_eq!(initial_turn_cap(40, 2, true), Some(28));
        assert_eq!(initial_turn_cap(30, 1, true), Some(18));
        assert_eq!(initial_turn_cap(25, 1, true), Some(13));
        assert_eq!(initial_turn_cap(10, 1, true), Some(5));
        assert_eq!(initial_turn_cap(4, 1, true), Some(2));
        assert_eq!(initial_turn_cap(1, 1, true), Some(1));
        // No repair rounds means no reserve.
        assert_eq!(initial_turn_cap(25, 0, true), Some(25));
    }

    #[test]
    fn test_repair_round_allowance_exhaustion_math() {
        // Uncapped stays uncapped (must never become a silent 25-cap).
        assert_eq!(repair_round_allowance(0, 0), RepairAllowance::Uncapped);
        assert_eq!(repair_round_allowance(0, 999), RepairAllowance::Uncapped);
        // Remainder bounded by the round cap.
        assert_eq!(repair_round_allowance(100, 0), RepairAllowance::Capped(25));
        assert_eq!(repair_round_allowance(100, 90), RepairAllowance::Capped(10));
        // Exact exhaustion and over-budget (saturating math can report
        // used > max when stats extraction failed mid-round: still
        // exhausted, never negative).
        assert_eq!(repair_round_allowance(100, 100), RepairAllowance::Exhausted);
        assert_eq!(repair_round_allowance(100, 140), RepairAllowance::Exhausted);
    }

    /// Zero model output + empty workspace is a provider/harness fault, not
    /// a 0% verdict. The guard must stay narrow: eval-only runs and any run
    /// with tokens or files are untouched (live regression: opencode quota
    /// retries ranked as failed_tests with 0 tokens).
    #[test]
    fn test_zero_output_guard_stays_narrow() {
        assert!(is_zero_output_non_result(false, 0, 0));
        // Real output keeps its own status.
        assert!(!is_zero_output_non_result(false, 1, 0));
        assert!(!is_zero_output_non_result(false, 0, 1));
        assert!(!is_zero_output_non_result(false, 1500, 2));
        // Eval-only has no agent by design.
        assert!(!is_zero_output_non_result(true, 0, 0));
    }

    #[test]
    fn test_stage_commit_parsing() {
        let log = "aaaa111 stage 0: MVP: RESP server [pass]\nbbbb222 stage 1: Expiry [fail]\ncccc333 not a stage line\n";
        assert_eq!(parse_stage_commit(log, 0).as_deref(), Some("aaaa111"));
        assert_eq!(parse_stage_commit(log, 1).as_deref(), Some("bbbb222"));
        assert_eq!(parse_stage_commit(log, 2), None);
    }

    #[test]
    fn test_coverage_parsers() {
        // go tool cover -func tail
        assert_eq!(
            parse_go_coverage_total("total:\t(statements)\t87.5%"),
            Some(87.5)
        );
        assert_eq!(
            parse_go_coverage_total("sdb-kv/main.go:12:\tfoo\t90.0%\ntotal:\t(statements)\t79.9%"),
            Some(79.9)
        );
        assert_eq!(parse_go_coverage_total("no total here"), None);
        // self-reported
        assert_eq!(parse_self_coverage("coverage: 81.2%\n"), Some(81.2));
        assert_eq!(parse_self_coverage("# Coverage\nlines: 80.0%"), None);
        assert_eq!(parse_self_coverage("Coverage summary: 100%"), Some(100.0));
        assert_eq!(parse_self_coverage("nothing"), None);
    }

    #[tokio::test]
    async fn test_pipeline_cancelled_early() {
        let cancel = Arc::new(AtomicBool::new(true));
        let config = BenchmarkConfig {
            model: "test_model".to_string(),
            task: TaskType::Redis,
            effort: "low".to_string(),
            max_turns: 1,
            budget_usd: 0.10,
            timeout_min: 1,
            api_key: None,
            workdir: std::env::temp_dir().join(format!("test_pipe_cancel_{}", std::process::id())),
            eval_only: true,
            run_id: Some("test_cancel_run".to_string()),
            save_results: false,
            review_model: None,
            batch_reference: None,
            worker: 0,
            harness: Harness::Omp,
            feedback_rounds: 0,
            campaign_stage: None,
            baseline_rps: None,
        };

        let res = BenchmarkPipeline::execute(config, Arc::new(DummyLogger), Some(cancel)).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn test_pipeline_buffers_and_writes_console_log() {
        let temp_workdir =
            std::env::temp_dir().join(format!("test_pipe_buf_ws_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_workdir);

        let run_id = format!("test_pipe_buf_run_{}", std::process::id());
        let config = BenchmarkConfig {
            model: "test_model".to_string(),
            task: TaskType::Redis,
            effort: "low".to_string(),
            max_turns: 1,
            budget_usd: 0.10,
            timeout_min: 1,
            api_key: None,
            workdir: temp_workdir.clone(),
            eval_only: true,
            run_id: Some(run_id.clone()),
            save_results: true,
            review_model: None,
            batch_reference: None,
            // Worker 99: this test only asserts the archive path; worker 0
            // ports (6379/6380/8090) belong to live campaign cells on this
            // host, so keep test traffic on the high offset.
            worker: 99,
            harness: Harness::Omp,
            feedback_rounds: 0,
            campaign_stage: None,
            baseline_rps: None,
        };

        struct CaptureLogger(std::sync::Mutex<Vec<String>>);
        impl PipelineLogger for CaptureLogger {
            fn log(&self, msg: &str) {
                self.0.lock().unwrap().push(msg.to_string());
            }
        }

        let logger = Arc::new(CaptureLogger(std::sync::Mutex::new(Vec::new())));
        let res = BenchmarkPipeline::execute(config, logger.clone(), None).await;
        assert!(res.is_ok());
        assert!(!logger.0.lock().unwrap().is_empty());

        let runs_dir = RunArchiver::resolve_runs_dir();
        let console_log_path = runs_dir.join(&run_id).join("console.log");
        assert!(console_log_path.exists());
        let content = fs::read_to_string(&console_log_path).unwrap();
        assert!(!content.is_empty());
        assert!(content.contains("[PIPELINE] Starting benchmark run"));
        let first_line = content.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with('[')
                && first_line.contains("] [PIPELINE] Starting benchmark run")
        );

        // Clean up, including the probe's DB rows (parents + runs tables).
        let _ = fs::remove_dir_all(&temp_workdir);
        let _ = fs::remove_dir_all(runs_dir.join(&run_id));
        let results_dir = get_repo_root().join("results");
        let _ = fs::remove_file(results_dir.join(format!("{}.json", run_id)));
        if let Ok(store) = crate::report::Store::open(&crate::report::Store::resolve_db_path()) {
            let _ = store.delete_parent(&run_id);
            let _ = store.delete_run_manifest(&run_id);
        }
    }

    #[tokio::test]
    async fn test_wal_probe_partial_acks_survive_eof() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let (rh, mut wh) = socket.into_split();
                let mut rd = tokio::io::BufReader::new(rh);
                for _ in 0..3 {
                    let mut line = String::new();
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if wh.write_all(b"+OK\n").await.is_err() {
                        return;
                    }
                }
            }
        });
        // The mock closes after 3 acks; the probe must return 3, not an error.
        let n = publish_wal_probe(port)
            .await
            .expect("probe keeps partial acks on EOF");
        assert_eq!(n, 3);
    }

    #[test]
    fn test_format_feedback_prompt() {
        let stages = vec![
            crate::verifier::StageResult {
                stage: 1,
                name: "PING / PONG".to_string(),
                passed: true,
                error: None,
            },
            crate::verifier::StageResult {
                stage: 2,
                name: "SET / GET".to_string(),
                passed: false,
                error: Some("expected +OK, got -ERR".to_string()),
            },
        ];
        let prompt = BenchmarkPipeline::format_feedback_prompt(
            TaskType::Redis,
            1,
            2,
            &stages,
            "server started on port 6379\ncommand error: SET foo bar",
        );
        assert!(prompt.contains("# Automated Verification Failure Report"));
        assert!(prompt.contains("redis"));
        assert!(prompt.contains("passed 1 of 2 stages (50.0%)"));
        assert!(prompt.contains("[PASS] Stage 1: PING / PONG"));
        assert!(prompt.contains("[FAIL] Stage 2: SET / GET"));
        assert!(prompt.contains("Diagnostic details: expected +OK, got -ERR"));
        assert!(prompt.contains("server started on port 6379"));
        assert!(prompt.contains("Directives for Surgical Bug Fixing:"));
        assert!(prompt.contains("DO NOT wipe your workspace"));
    }

    fn stage_by(m: &RunManifest, n: u32) -> &crate::verifier::StageResult {
        m.stages
            .iter()
            .find(|s| s.stage == n)
            .unwrap_or_else(|| panic!("stage {n} missing from manifest"))
    }

    #[test]
    fn test_output_tail_trims_and_respects_char_boundaries() {
        assert_eq!(output_tail("  hello  ", 10), "hello");
        assert_eq!(output_tail("hello world", 5), "…world");
        // A cut landing inside a multi-byte char walks forward to the next
        // boundary ("héllo" = 6 bytes: cutting at 3 keeps "llo").
        assert_eq!(output_tail("héllo", 3), "…llo");
        // Entire tail is one partial char: the marker stands alone, never a
        // broken UTF-8 slice.
        assert_eq!(output_tail("h€", 2), "…");
        assert_eq!(output_tail("", 4), "");
    }

    #[test]
    fn test_hash_seed_is_stable_and_hex_shaped() {
        let a = hash_seed("redis_test_model_20261014");
        assert_eq!(a, hash_seed("redis_test_model_20261014"));
        assert_ne!(a, hash_seed("redis_test_model_20261015"));
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_git_in_success_and_failure() {
        let dir = covtest_dir("git");
        fs::create_dir_all(&dir).unwrap();
        // Not a repo: the failure must name the git args for diagnosis.
        let err = git_in(&dir, &["rev-parse", "HEAD"]).unwrap_err();
        assert!(err.contains("rev-parse"), "{err}");
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(init.status.success());
        let inside = git_in(&dir, &["rev-parse", "--is-inside-work-tree"]).unwrap();
        assert_eq!(inside.trim(), "true");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_buffering_logger_honors_cancel_filter() {
        let inner = Arc::new(RecordingLogger::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let buf = BufferingPipelineLogger::new(inner.clone(), Some(cancel.clone()));
        buf.log("normal before cancel");
        cancel.store(true, Ordering::SeqCst);
        buf.log("suppressed while cancelled");
        buf.log("[USER ACTION] still shown");
        buf.log("[DONE] marker");
        buf.log("[STOP] marker");
        buf.log("[PIPELINE] Run cancelled before archiving.");
        buf.log("[PIPELINE] Cancellation reason");
        assert!(inner.contains("normal before cancel"));
        assert!(!inner.contains("suppressed while cancelled"));
        assert!(inner.contains("[USER ACTION] still shown"));
        assert!(inner.contains("[DONE] marker"));
        assert!(inner.contains("[STOP] marker"));
        assert!(inner.contains("Run cancelled before archiving"));
        assert!(inner.contains("Cancellation reason"));
        // Buffer mirrors the forwarded lines, timestamp-prefixed.
        let logs = buf.get_logs();
        assert!(logs.contains("normal before cancel"));
        assert!(!logs.contains("suppressed while cancelled"));
        assert!(logs
            .lines()
            .any(|l| l.starts_with('[') && l.contains("] normal before cancel")));
    }

    #[test]
    fn test_worker_upstream_ports_follow_worker_offset() {
        assert_eq!(worker_upstream_ports(0), [8101, 8102, 8103]);
        assert_eq!(worker_upstream_ports(3), [8401, 8402, 8403]);
    }

    #[test]
    fn test_format_feedback_prompt_zero_stages_and_tail_limit() {
        // Zero totals: the percentage guard must not divide by zero.
        let p = BenchmarkPipeline::format_feedback_prompt(TaskType::Redis, 0, 0, &[], "");
        assert!(p.contains("passed 0 of 0 stages (0.0%)"));
        assert!(!p.contains("Service Container Runtime Output"));
        // Container logs are tail-capped at 60 lines: older lines drop off.
        let logs: Vec<String> = (0..80).map(|i| format!("line {i:02}")).collect();
        let p =
            BenchmarkPipeline::format_feedback_prompt(TaskType::Redis, 1, 2, &[], &logs.join("\n"));
        assert!(p.contains("line 79"));
        assert!(p.contains("line 20"));
        assert!(!p.contains("line 19"));
    }

    /// Full REAL pipeline drive with no model: eval-only Redis on a mock
    /// RESP candidate that clears stages 1-2 and deliberately fails 3-5.
    /// Asserts the verdict/manifest shape of a partially-passing trial and
    /// that the bench is skipped below 100%.
    #[tokio::test]
    async fn test_pipeline_eval_only_mock_candidate_full_flow() {
        let worker = 90u8;
        let port = crate::web::server::worker_port(TaskType::Redis.default_port(), worker);
        let workdir = covtest_dir("flow");
        write_mock_candidate(&workdir, port, 0);
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(
            covtest_config(workdir.clone(), worker, "flow"),
            logger.clone(),
            None,
        )
        .await
        .expect("eval-only mock pipeline must complete");
        assert!(!out.disqualified && !out.infra_error && !out.agent_failed);

        let m = &out.manifest;
        assert_eq!(m.status, "failed_tests");
        assert_eq!((m.passed_stages, m.total_stages), (2, 6));
        assert!(
            m.pass_rate > 33.0 && m.pass_rate < 34.0,
            "pass_rate {}",
            m.pass_rate
        );
        assert!(
            stage_by(m, 1).passed,
            "handshake: {:?}",
            stage_by(m, 1).error
        );
        assert!(stage_by(m, 2).passed, "kv: {:?}", stage_by(m, 2).error);
        for n in [3, 4, 5] {
            assert!(!stage_by(m, n).passed, "stage {n} must fail");
        }
        assert!(!stage_by(m, 6).passed);
        assert!(stage_by(m, 6)
            .error
            .as_deref()
            .unwrap_or("")
            .contains("skipped"));

        // Below 100%: no load test, no tokens, no cost, unsolved.
        assert!(m.throughput_req_sec.is_none());
        assert!(m.reference_throughput_req_sec.is_none());
        assert!(m.baseline_source.is_none());
        assert_eq!(m.tokens.total_tokens, 0);
        assert_eq!(m.cost_usd, 0.0);
        assert_eq!(m.cost_source.as_deref(), Some("formula"));
        assert_eq!(m.solved, Some(false));
        assert!(m.solved_at.is_none());
        assert_eq!(m.repaired, None);
        assert_eq!(m.feedback_rounds, None);
        assert_eq!(out.benchmark_result.status.as_deref(), Some("failed_tests"));
        assert_eq!(out.benchmark_result.pass_at_1, Some(0.0));

        // Advisory probes ran; the threaded mock survives both robustness
        // checks (malformed garbage + half-open slow sender).
        assert_eq!(m.robustness.len(), 3);
        let adv = |n: u32| m.robustness.iter().find(|s| s.stage == n).unwrap();
        assert!(adv(101).passed, "malformed: {:?}", adv(101).error);
        assert!(adv(102).passed, "slow sender: {:?}", adv(102).error);

        assert!(logger.contains("PING & ECHO Handshake"));
        assert!(logger.contains("skipped due to failure in earlier stages"));
        cleanup_covtest_network("flow");
        let _ = fs::remove_dir_all(&workdir);
    }

    /// Empty workspace: no start.sh/Dockerfile -> verification is skipped,
    /// no container is built, and the verdict stays a (0%, 6-stage) trial.
    #[tokio::test]
    async fn test_pipeline_eval_only_no_runnable_workspace() {
        let workdir = covtest_dir("norun");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(workdir.join("notes.txt"), "no server here").unwrap();
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(
            covtest_config(workdir.clone(), 91, "norun"),
            logger.clone(),
            None,
        )
        .await
        .expect("pipeline completes even without a candidate");
        assert!(!out.infra_error && !out.disqualified && !out.agent_failed);
        assert_eq!(out.manifest.status, "failed_tests");
        assert_eq!(out.manifest.pass_rate, 0.0);
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (0, 6)
        );
        assert!(out.manifest.stages.is_empty());
        assert!(logger.contains("No runnable start.sh or Dockerfile found"));
        cleanup_covtest_network("norun");
        let _ = fs::remove_dir_all(&workdir);
    }

    /// Forbidden base image -> disqualified before any container work, with
    /// the compliance stage carrying the reason and the cheat status.
    #[tokio::test]
    async fn test_pipeline_eval_only_disqualified_workspace() {
        let workdir = covtest_dir("dq");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(workdir.join("Dockerfile"), "FROM redis:alpine\n").unwrap();
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(
            covtest_config(workdir.clone(), 92, "dq"),
            logger.clone(),
            None,
        )
        .await
        .expect("disqualified run still returns Ok");
        assert!(out.disqualified);
        assert!(!out.infra_error);
        assert_eq!(out.manifest.status, "disqualified_cheat");
        assert_eq!(out.manifest.pass_rate, 0.0);
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (0, 6)
        );
        assert_eq!(out.manifest.stages.len(), 1);
        assert_eq!(out.manifest.stages[0].stage, 0);
        assert!(out.manifest.stages[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("forbidden base image"));
        assert!(logger.contains("[ANTI-CHEAT VIOLATION]"));
        cleanup_covtest_network("dq");
        let _ = fs::remove_dir_all(&workdir);
    }

    /// `docker run -p` cannot bind a port the host already holds -> the
    /// candidate container failure is classified as an infra error, not a
    /// 0% model verdict.
    #[tokio::test]
    async fn test_pipeline_eval_only_infra_error_on_port_conflict() {
        let worker = 93u8;
        let port = crate::web::server::worker_port(TaskType::Redis.default_port(), worker);
        let workdir = covtest_dir("infra");
        write_mock_candidate(&workdir, port, 0);

        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_accept = stop.clone();
        let acceptor = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_accept.load(Ordering::SeqCst) {
                    break;
                }
                // Immediately reset: verifier probes fail fast on EOF.
                drop(stream);
            }
        });

        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(
            covtest_config(workdir.clone(), worker, "infra"),
            logger.clone(),
            None,
        )
        .await
        .expect("infra-error run still returns Ok");
        assert!(out.infra_error);
        assert!(!out.disqualified && !out.agent_failed);
        assert_eq!(out.manifest.status, "infra_error");
        assert!(logger.contains("[INFRA] Candidate container failed to start"));
        assert!(
            !logger.contains("[MODEL-FAULT]"),
            "a port clash is not a model packaging fault"
        );

        stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        acceptor.join().unwrap();
        cleanup_covtest_network("infra");
        let _ = fs::remove_dir_all(&workdir);
    }

    /// Runnable image, unusable entrypoint: `docker run` fails with
    /// "no such file or directory", which is the model's packaging fault
    /// (not infra). Driven through `run_verification` directly.
    #[tokio::test]
    async fn test_verification_classifies_model_packaging_fault() {
        let worker = 94u8;
        let port = crate::web::server::worker_port(TaskType::Redis.default_port(), worker);
        let workdir = covtest_dir("packaging");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(
            workdir.join("Dockerfile"),
            "FROM subdollar-sandbox\nENTRYPOINT [\"/nonexistent-binary\"]\n",
        )
        .unwrap();

        let id = format!("covtest-{}-packaging", std::process::id());
        let sandbox = SandboxManager::with_id(&id);
        let rec: Arc<RecordingLogger> = Arc::new(RecordingLogger::default());
        let logger: Arc<dyn PipelineLogger> = rec.clone();
        let outcome = BenchmarkPipeline::run_verification(
            TaskType::Redis,
            &workdir,
            &sandbox,
            port,
            "covtest-seed",
            false,
            worker,
            &logger,
            None,
        )
        .await;
        assert!(
            !outcome.infra_error,
            "packaging faults are model-side, not infra"
        );
        assert_eq!(outcome.pass_rate, 0.0);
        assert_eq!(outcome.total_stages, 6);
        assert!(rec.contains("[MODEL-FAULT] Candidate container failed to start"));

        sandbox.cleanup();
        let _ = std::process::Command::new("docker")
            .args(["rmi", "-f", &format!("subdollar-candidate-{id}")])
            .output();
        let _ = fs::remove_dir_all(&workdir);
    }

    /// A cancel that lands while verification is running must surface as an
    /// Err after the sandbox is cleaned up, never as a scored trial.
    #[tokio::test]
    async fn test_pipeline_cancel_after_verification_returns_err() {
        let worker = 95u8;
        let port = crate::web::server::worker_port(TaskType::Redis.default_port(), worker);
        let workdir = covtest_dir("cancel");
        // Slow boot keeps the run inside verification past the timer.
        write_mock_candidate(&workdir, port, 8);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_timer = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            cancel_timer.store(true, Ordering::SeqCst);
        });
        let logger = Arc::new(RecordingLogger::default());
        let res = BenchmarkPipeline::execute(
            covtest_config(workdir.clone(), worker, "cancel"),
            logger.clone(),
            Some(cancel),
        )
        .await;
        let err = res.expect_err("a cancelled run must not report success");
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(logger.contains("cancel"), "cancellation must be logged");
        cleanup_covtest_network("cancel");
        let _ = fs::remove_dir_all(&workdir);
    }

    /// Campaign ticket with a partial acceptance subset: only stages 1-3
    /// count toward the verdict, the destructive AOF stage is skipped, and
    /// the manifest records the subset totals.
    #[tokio::test]
    async fn test_pipeline_campaign_acceptance_subset_scopes_verdict() {
        let worker = 96u8;
        let port = crate::web::server::worker_port(TaskType::Redis.default_port(), worker);
        let workdir = covtest_dir("campaign");
        write_mock_candidate(&workdir, port, 0);
        let prompt_path = workdir.join("ticket_prompt.md");
        fs::write(&prompt_path, "campaign ticket: continue the build").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "campaign");
        config.feedback_rounds = 2;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-campaign".to_string(),
            stage: 1,
            prompt_path,
            continuity: true,
            accepted_stages: vec![1, 2, 3],
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("campaign eval-only run must complete");
        let m = &out.manifest;
        assert_eq!(m.status, "failed_tests");
        assert_eq!((m.passed_stages, m.total_stages), (2, 3));
        assert!(
            m.pass_rate > 66.0 && m.pass_rate < 67.0,
            "pass_rate {}",
            m.pass_rate
        );
        // feedback_rounds > 0 but eval-only never runs a repair round.
        assert_eq!(m.feedback_rounds, Some(0));
        assert_eq!(m.repair_attempted, Some(false));
        assert_eq!(m.repaired, None);
        assert!(logger.contains("outside this ticket's acceptance"));
        assert!(logger.contains("skipping the destructive kill"));
        cleanup_covtest_network("campaign");
        let _ = fs::remove_dir_all(&workdir);
    }

    /// Stage-2 wave live proof (eval-only, no model): a disk-resident mock
    /// candidate driven through the REAL pipeline for stages 14-16 —
    /// host-bind restart under the 192MB cap, SIGKILL re-reads, the
    /// footprint gate, and the SDB1 v2 admin channel end to end.
    #[tokio::test]
    async fn test_pipeline_eval_only_stage2_disk_and_admin_live() {
        let worker = 97u8;
        let port = crate::web::server::worker_port(TaskType::Redis.default_port(), worker);
        let dir = covtest_dir("s2live");
        let workdir = dir.join("workspace");
        write_stage2_mock(
            &workdir,
            port,
            crate::web::server::worker_port(6395, worker),
        );
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(
            &prompt_path,
            "stage-2 ticket: disk resident, compact, admin",
        )
        .unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "s2live");
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-kv".to_string(),
            stage: 11,
            prompt_path,
            continuity: true,
            accepted_stages: vec![14, 15, 16],
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("stage-2 live flow must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        assert!(
            logger.contains("Stage 14: disk-resident store under a memory cap -> PASS"),
            "stage 14 must pass"
        );
        assert!(
            logger.contains("Stage 15: compaction reclaims disk -> PASS"),
            "stage 15 must pass"
        );
        assert!(
            logger.contains("Stage 16: admin binary protocol (SDB1 v2) -> PASS"),
            "stage 16 must pass"
        );
        cleanup_covtest_network("s2live");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-edge live smoke (eval-only, no model): a minimal forwarding
    /// proxy + the run-network nginx pool, driven through the real pipeline
    /// with acceptance [1]. Validates the proxy campaign path: task
    /// adapter, upstream pool startup, acceptance-scoped verdict and the
    /// stage-log contract.
    #[tokio::test]
    async fn test_pipeline_eval_only_proxy_campaign_stage1_live() {
        let worker = 98u8;
        let port = crate::web::server::worker_port(TaskType::Proxy.default_port(), worker);
        let dir = covtest_dir("edge");
        let workdir = dir.join("workspace");
        write_proxy_mock(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-edge ticket 0: route GET /").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "edge");
        config.task = TaskType::Proxy;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-edge".to_string(),
            stage: 0,
            prompt_path,
            continuity: false,
            accepted_stages: vec![1],
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("proxy campaign flow must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        assert!(
            logger.contains("Stage 1: Basic routing (200 + marker) -> PASS"),
            "stage 1 must pass through the proxy campaign path"
        );
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (1, 1)
        );
        cleanup_covtest_network("edge");
        let _ = fs::remove_dir_all(&dir);
    }

    /// No-runnable branches of `run_verification`, including the
    /// agent-failed (empty workspace) wording. No container is started.
    #[tokio::test]
    async fn test_run_verification_no_runnable_candidate_paths() {
        let workdir = covtest_dir("v-none");
        fs::create_dir_all(&workdir).unwrap();
        let sandbox = SandboxManager::with_id(&format!("covtest-{}-vn", std::process::id()));
        let port = crate::web::server::worker_port(TaskType::Redis.default_port(), 97);

        let rec: Arc<RecordingLogger> = Arc::new(RecordingLogger::default());
        let logger: Arc<dyn PipelineLogger> = rec.clone();
        let outcome = BenchmarkPipeline::run_verification(
            TaskType::Redis,
            &workdir,
            &sandbox,
            port,
            "covtest-seed",
            false,
            97,
            &logger,
            None,
        )
        .await;
        assert_eq!(outcome.pass_rate, 0.0);
        assert!(outcome.stage_results.is_empty());
        assert!(!outcome.infra_error);
        assert!(rec.contains("No runnable start.sh or Dockerfile found"));

        let rec2: Arc<RecordingLogger> = Arc::new(RecordingLogger::default());
        let logger2: Arc<dyn PipelineLogger> = rec2.clone();
        let outcome2 = BenchmarkPipeline::run_verification(
            TaskType::Redis,
            &workdir,
            &sandbox,
            port,
            "covtest-seed",
            true,
            97,
            &logger2,
            None,
        )
        .await;
        assert_eq!(outcome2.pass_rate, 0.0);
        assert!(outcome2.stage_results.is_empty());
        assert!(rec2.contains("did not produce runnable code"));
        let _ = fs::remove_dir_all(&workdir);
    }
}
