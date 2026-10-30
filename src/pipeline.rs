use anyhow::{anyhow, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::config::{get_repo_root, Harness, TaskType};
use crate::cost::ModelPricing;
use crate::report::archive::{RunArchiver, RunManifest, RunTokenUsage};
use crate::report::judge::RunJudge;
use crate::report::leaderboard::{BenchmarkRunResult, LeaderboardManager};
use crate::sandbox::docker::{RunGuard, SandboxManager};

/// Upstream pool ports for a worker (base pool + worker*100 offset).

/// The harness-owned data directory for an sdb-sql candidate. In a
/// campaign it is a sibling of the workspace (the campaign directory), so
/// it survives candidate restarts and stage rollbacks and the harness can
/// snapshot or upgrade-check it (docs/PROPOSAL-sdb-sql.md, "Upgrade
/// check"). A single-shot run gets a process-private temp directory, so
/// unrelated runs sharing a parent never share one store.
fn sql_data_dir(workdir: &Path, campaign: bool) -> PathBuf {
    if campaign {
        return workdir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(workdir)
            .join("sql-data");
    }
    std::env::temp_dir().join(format!("sdb-sql-data-{}", std::process::id()))
}

/// Stop the candidate and start it again with `extras`, then wait until it
/// answers one protocol-level probe. `Err` means the harness could not get
/// it back up (or it stayed silent); for a model candidate that is a
/// failure, so callers grade it accordingly.
///
/// `graceful` sends SIGTERM and gives the server a few seconds to flush
/// (the ticket-4 clean-restart contract); `false` is `rm -f`, i.e. SIGKILL,
/// which is what ticket 5's ladder and a shrink reset want.
fn restart_candidate(
    instance_id: &str,
    workdir: &Path,
    port: u16,
    extras: &crate::sandbox::docker::CandidateRunExtras,
    graceful: bool,
) -> Result<(), String> {
    let sb = crate::sandbox::docker::SandboxManager::with_id(instance_id);
    if graceful {
        sb.stop_candidate_gracefully(5)
            .map_err(|e| format!("stop: {e}"))?;
    } else {
        sb.stop_candidate().map_err(|e| format!("stop: {e}"))?;
    }
    sb.start_candidate_in_docker_with(workdir, port, extras)
        .map_err(|e| format!("start: {e}"))?;
    crate::verifier::sql::driver::wait_until_answering(
        &format!("127.0.0.1:{port}"),
        std::time::Duration::from_secs(20),
    )
}

/// Stage 5's crash ladder against the candidate: acked rows survive a
/// SIGKILL, a mid-flight bulk UPDATE is all-or-nothing, a torn newest data
/// file recovers every committed statement, and a second restart changes
/// nothing (docs/PROPOSAL-sdb-sql.md, "5. Crash safety").
///
/// The ladder writes hundreds of thousands of rows, so it runs against a
/// throwaway copy of the store and restarts back onto the real one at the
/// end: the store the next ticket reads stays small (and recovery fast),
/// and a torn file is never torn in it. The copy is taken from a quiesced
/// store (the candidate is killed first).
#[allow(clippy::too_many_arguments)]
fn crash_ladder(
    sandbox: &crate::sandbox::docker::SandboxManager,
    workdir: &Path,
    port: u16,
    extras: &crate::sandbox::docker::CandidateRunExtras,
    data: &Path,
    verifier_seed: &str,
    lite: bool,
    logger: &Arc<dyn PipelineLogger>,
) -> Result<(), String> {
    use crate::verifier::sql::crash;
    let addr = format!("127.0.0.1:{port}");
    let t = std::time::Duration::from_secs(20);
    let ns = crate::verifier::sql::driver::stage_seed(verifier_seed, 5) ^ 0x5E27_1E10_0000_0005;
    let acked_table = format!("c{ns:x}_acked");
    let mut bulk_table = String::new();
    let mut rows = 0usize;
    let mut bulk_rows = 0usize;
    let mut victims = 0usize;
    let mut owed = false;
    // The bulk column's value right after (b) pinned it; (c) and (d) must
    // not change it.
    let mut value_after_b = String::new();
    // Keys the tear may not cost (`must`) and those it may (`maybe`).
    let mut must: Vec<usize> = Vec::new();
    let mut maybe: Vec<usize> = Vec::new();

    let scratch = data.with_file_name(format!(
        "{}-crash",
        data.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    let _ = sandbox.kill_candidate();
    let _ = std::fs::remove_dir_all(&scratch);
    crash::copy_tree(data, &scratch).map_err(|e| format!("scratch copy: {e}"))?;
    let mut scratch_extras = extras.clone();
    let real_src = data.display().to_string();
    for b in scratch_extras.binds.iter_mut() {
        if b.0 == real_src {
            b.0 = scratch.display().to_string();
        }
    }
    let restart = |e: &crate::sandbox::docker::CandidateRunExtras| -> Result<(), String> {
        restart_candidate(&sandbox.instance_id, workdir, port, e, false).map_err(|e| {
            logger.log(&format!("  [CRASH] restart failed: {e}"));
            e
        })
    };
    restart(&scratch_extras)?;
    logger.log(&format!(
        "  [CRASH] ladder runs on a scratch copy of the store ({})",
        scratch.display()
    ));

    // The ladder body: any early return must still put the candidate back
    // on the real store and drop the scratch copy, because the rest of this
    // verification (and the repair round) works on the real store.
    let body = (|| -> Result<(), String> {
        // (a) acked rows survive SIGKILL. One more INSERT is sent and never
        // read; if its answer arrived before the kill, the row is owed too.
        let (acked, in_flight_owed) = {
            let mut kill = || sandbox.kill_candidate().map_err(|e| e.to_string());
            let out = crash::ack_kill(&addr, t, &acked_table, 40, &mut kill)
                .map_err(|e| format!("(a) {e}"))?;
            (out.acked, out.completed)
        };
        restart(&scratch_extras)?;
        if in_flight_owed {
            // The statement completed and was answered: it must be there.
            crash::check_acked_rows(&addr, t, &acked_table, acked + 1)
                .map_err(|e| format!("(a) {e}"))?;
            logger.log(&format!(
            "  [CRASH] {acked} acked rows (and the in-flight one, which answered first) survived a SIGKILL"
        ));
        } else {
            crash::check_acked_prefix(&addr, t, &acked_table, acked, 1)
                .map_err(|e| format!("(a) {e}"))?;
            logger.log(&format!("  [CRASH] {acked} acked rows survived a SIGKILL"));
        }

        // The base of what the tear may not cost (`must`) and what it may
        // (`maybe`); the victims committed during (c) extend it below.
        must = (0..acked).collect();
        maybe = vec![acked];
        if in_flight_owed {
            must.push(acked);
            maybe.clear();
        }
        // The un-acked row may still have been committed: if it is visible
        // after (a)'s restart, it is owed too.
        if let Ok(keys) = crash::read_keys(&addr, t, &acked_table) {
            if keys.contains(&acked) && !must.contains(&acked) {
                must.push(acked);
                maybe.retain(|k| *k != acked);
            }
        }

        if lite {
            // Lite stage 5 (proposal): acked rows survive kill -9; torn tails
            // and mid-statement atomicity are not required.
            logger.log("  [CRASH] lite variant: acked rows and restarts only");
        } else {
            // (b) a bulk UPDATE killed mid-flight is all or nothing. If the
            // answer arrives before the kill lands the server finished the
            // update, so that rep proved nothing: retry with a bigger table.
            rows = crash::BULK_ROWS;
            // The last attempt's own row count: `rows` doubles between
            // attempts, but the final table holds only what that attempt built.
            // `owed` is the probe's post-kill verdict: true when the UPDATE's
            // `OK` arrived before the kill (then the new value must survive).
            // Four attempts (50k .. 400k rows) give even a fast engine a real
            // chance of being killed mid-statement.
            for attempt in 0..4 {
                bulk_table = format!("c{ns:x}_bulk{attempt}");
                bulk_rows = rows;
                owed = {
                    let mut kill = || sandbox.kill_candidate().map_err(|e| e.to_string());
                    crash::bulk_update_and_kill(
                        &addr,
                        t,
                        &bulk_table,
                        rows,
                        crash::BULK_CHUNK,
                        &mut kill,
                    )
                    .map_err(|e| format!("(b) {e}"))?
                };
                restart(&scratch_extras)?;
                // Every attempt's table is pinned right after its own
                // restart: an answered update is owed (owed = true), a
                // mid-statement kill leaves either value.
                value_after_b = crash::check_uniform(&addr, t, &bulk_table, rows, owed)
                    .map_err(|e| format!("(b) {e}"))?;
                if !owed {
                    break;
                }
                logger.log(&format!(
                "  [CRASH] the {rows}-row bulk UPDATE answered before the kill; retrying bigger"
            ));
                rows *= 2;
            }
            if owed {
                logger.log(
                    "  [WARN] the bulk UPDATE kept answering before the kill; the atomicity \
                 check is weaker this run (the acked update is still required)",
                );
            }
            if value_after_b.is_empty() {
                value_after_b = crash::check_uniform(&addr, t, &bulk_table, bulk_rows, owed)
                    .map_err(|e| format!("(b) {e}"))?;
            }
            logger.log("  [CRASH] the killed bulk UPDATE was all-or-nothing");

            // (c) a torn tail. Kill first: a running server may rewrite the
            // file. Then cut into the file the victim commit touched: a partial
            // trailing record may cost the newest statement, never an older
            // committed row. A commit that lands on a WAL checkpoint rewrites
            // in place and grows nothing (or nothing tearable), so retry with
            // another victim instead of skipping.
            let mut torn: Option<(std::path::PathBuf, u64)> = None;
            for attempt in 0..3 {
                let before = crash::snapshot_files(&scratch).map_err(|e| format!("(c) {e}"))?;
                crash::append_row(&addr, t, &acked_table, acked + 1 + attempt)
                    .map_err(|e| format!("(c) {e}"))?;
                victims += 1;
                let _ = sandbox.kill_candidate();
                if let Some(path) =
                    crash::changed_file(&scratch, &before).map_err(|e| format!("(c) {e}"))?
                {
                    let cut = crash::torn_tail(&path, 256).map_err(|e| format!("(c) {e}"))?;
                    if cut > 0 {
                        torn = Some((path, cut));
                        break;
                    }
                }
                // Nothing changed (or nothing tearable): bring the store back
                // and try one more commit.
                restart(&scratch_extras)?;
            }
            let Some((path, cut)) = torn else {
                return Err(
                    "(c) three acked INSERTs changed no file: commits never reached the store"
                        .to_string(),
                );
            };
            logger.log(&format!(
                "  [CRASH] (c) torn tail: {cut} bytes off {}",
                path.display()
            ));
            restart(&scratch_extras)?;
            // Only the newest victim may be missing: the older ones were
            // acked before a clean restart, so they are owed.
            for v in 1..victims {
                must.push(acked + v);
            }
            if victims > 0 {
                maybe.push(acked + victims);
            }
            crash::check_keys(&addr, t, &acked_table, &must, &maybe)
                .map_err(|e| format!("(c) {e}"))?;
            // The torn tail may cost the newest statement, so a uniform old
            // value is fine here.
            let after_c = crash::check_uniform(&addr, t, &bulk_table, bulk_rows, false)
                .map_err(|e| format!("(c) {e}"))?;
            if after_c != value_after_b {
                return Err(format!(
                    "(c) the bulk column changed from {value_after_b} (read after (b)) to \
                     {after_c} across the tear: a committed update was lost"
                ));
            }
            logger.log(&format!(
                "  [CRASH] {cut} torn bytes from {}: every older row intact",
                path.display()
            ));
        }

        // (d) a second restart changes nothing: the key set and the bulk
        // column must be exactly what the last state-reading step saw.
        let state_before_d =
            crash::read_keys(&addr, t, &acked_table).map_err(|e| format!("(d) {e}"))?;
        let value_before_d = if lite || value_after_b.is_empty() {
            String::new()
        } else {
            crash::check_uniform(&addr, t, &bulk_table, bulk_rows, false)
                .map_err(|e| format!("(d) {e}"))?
        };
        restart(&scratch_extras)?;
        crash::check_keys(&addr, t, &acked_table, &must, &maybe).map_err(|e| format!("(d) {e}"))?;
        let state_after_d =
            crash::read_keys(&addr, t, &acked_table).map_err(|e| format!("(d) {e}"))?;
        if state_after_d != state_before_d {
            return Err(format!(
                "(d) the second restart changed the store: before {state_before_d:?}, after \
                 {state_after_d:?}"
            ));
        }
        if !value_before_d.is_empty() {
            let after_d = crash::check_uniform(&addr, t, &bulk_table, bulk_rows, false)
                .map_err(|e| format!("(d) {e}"))?;
            if after_d != value_before_d {
                return Err(format!(
                    "(d) the second restart changed the bulk column from {value_before_d} to \
                     {after_d}"
                ));
            }
        }
        if !lite {
            crash::check_uniform(&addr, t, &bulk_table, bulk_rows, false)
                .map_err(|e| format!("(d) {e}"))?;
        }
        logger.log("  [CRASH] a double restart changed nothing");
        Ok(())
    })();

    // Always: back onto the real store, then drop the scratch copy.
    let restored = restart(extras);
    let _ = std::fs::remove_dir_all(&scratch);
    body?;
    restored?;
    Ok(())
}

/// `wait_for_port` only proves the socket accepts; under a loaded host the
/// application may not serve yet, and the next request then dies on a reset
/// (the http stages 9/10 restart flaked exactly so on 2026-10-16). Probe
/// like the pre-verification setup does.

/// One raw `GET /` probe against a candidate: true when any HTTP response
/// arrives (any status). A socket that accepts but never answers (a slow
/// startup self-test, a wedged accept loop) does not count as ready.
use crate::sandbox::omp::{AgentExecutionLimits, AgentRunSpec, OmpRunner, OmpSessionStats};
use crate::sandbox::opencode::{OpenCodeRunner, OPENCODE_VERSION};
use crate::sandbox::AgentRunner;
use crate::verifier::compliance::ComplianceChecker;

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

/// The last `n` non-empty lines of `text` (round evidence tails).
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
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

/// The lite variants a suite grades at: every earlier lite pass from the
/// ladder, plus this attempt's lite profile at the ticket being graded
/// (round-16 I1, shared by the main and post-failover suites).
fn suite_variants(
    passed_lite: &[(u32, Vec<String>)],
    lite_profile: &[String],
    wanted: &[u32],
) -> Vec<(u32, Vec<String>)> {
    let mut variants = passed_lite.to_vec();
    if !lite_profile.is_empty() {
        if let Some(&cur) = wanted.iter().max() {
            variants.push((cur, lite_profile.to_vec()));
        }
    }
    variants
}

/// Ticket 19: whether a failed control action is a harness fault. A rejoin
/// is the harness recreating a container, so its failure always is. A
/// kill, pause or resume that fails on a node whose container already
/// exited is the candidate's crash and stays a model failure; on a node
/// that is still up, or one Docker cannot inspect, Docker itself failed
/// (round-18 N2).
fn repl_control_failure_is_infra(
    action: crate::verifier::sql::replication::HaAction,
    node_state: Option<&str>,
) -> bool {
    matches!(
        action,
        crate::verifier::sql::replication::HaAction::Rejoin { .. }
    ) || !matches!(node_state, Some("exited") | Some("dead"))
}

/// Ticket 15 (tests & CI gate, sdb-sql): the ticket ships `./test.sh`
/// (offline, under 120 s), a coverage signal (Go measured here; other
/// stacks self-report `coverage: NN.N%` in coverage.txt) and tests that
/// catch regressions: reverting the stage-7 (aggregates) and stage-8
/// (joins) commits in a scratch copy must make the suite red. The lite
/// variant lowers the coverage gate to 60% and keeps sensitivity.
fn run_tests_gate_blocking(
    sandbox: SandboxManager,
    workdir: PathBuf,
    logger: Arc<dyn PipelineLogger>,
    lite: bool,
) -> crate::verifier::StageResult {
    let name = "Stage 15: Tests and CI gate".to_string();
    let fail = |e: String| crate::verifier::StageResult {
        stage: 15,
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
            120,
        )
    };
    for attempt in 1..=2u32 {
        match run_suite(&workdir) {
            Ok((true, _)) => logger.log(&format!(
                "  [TESTS] ./test.sh green (run {attempt}/2, offline, <= 120s)"
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
    let (coverage, measured) = if workdir.join("go.mod").is_file() {
        match sandbox.run_workspace_command(
            &workdir,
            "go test ./... -coverprofile=/tmp/sdb-cov.out >/dev/null 2>&1; go tool cover -func=/tmp/sdb-cov.out | tail -n 1",
            120,
        ) {
            Ok((_, out)) => (parse_go_coverage_total(&out), true),
            Err(e) => return fail(format!("coverage measurement failed: {e}")),
        }
    } else {
        (
            std::fs::read_to_string(workdir.join("coverage.txt"))
                .ok()
                .and_then(|t| parse_self_coverage(&t)),
            false,
        )
    };
    let Some(cov) = coverage else {
        return fail(
            "no coverage signal: Go projects are measured by the harness via `go test -coverprofile`; other languages must write a `coverage: NN.N%` line into coverage.txt"
                .to_string(),
        );
    };
    let gate = if lite { 60.0 } else { 80.0 };
    // Round-15 M2: only Go is measured here. Say so, and say it in the
    // prompt too, so the number is not mistaken for an independent check.
    logger.log(&format!(
        "  [TESTS] coverage {cov:.1}% ({}; gate {gate}%)",
        if measured {
            "measured"
        } else {
            "self-report, not verified"
        }
    ));
    if cov < gate {
        return fail(format!("coverage {cov:.1}% is below the {gate}% gate"));
    }

    // Sensitivity: the stage-7 and stage-8 features must be covered by the
    // candidate's own tests. Prefer reversing only that commit's own diff
    // (`git apply -R --3way` in a copy that includes .git); when later
    // tickets touched the same lines, fall back to restoring every file
    // the commit changed to its parent version. Either way the suite must
    // go red (round-15 M3).
    let Some(parent) = workdir.parent() else {
        return fail("workspace has no parent directory".to_string());
    };
    let log = match git_in(&workdir, &["log", "--format=%H %s"]) {
        Ok(l) => l,
        Err(e) => return fail(format!("git log failed: {e}")),
    };
    for stage in [7u32, 8u32] {
        let Some(commit) = parse_stage_commit(&log, stage) else {
            return fail(format!(
                "no `stage {stage}` commit in the workspace history (the sensitivity check needs the feature it reverts)"
            ));
        };
        let parent_commit = match git_in(&workdir, &["rev-parse", &format!("{commit}^")]) {
            Ok(p) => p.trim().to_string(),
            Err(e) => return fail(format!("stage {stage}: parent commit: {e}")),
        };
        let patch = match git_in(&workdir, &["diff", "--no-color", &parent_commit, &commit]) {
            Ok(p) => p,
            Err(e) => return fail(format!("stage {stage}: diff: {e}")),
        };
        if patch.trim().is_empty() {
            return fail(format!(
                "the stage-{stage} commit changed no files; the sensitivity check cannot revert it"
            ));
        }
        let changed = match git_in(&workdir, &["diff", "--name-only", &parent_commit, &commit]) {
            Ok(c) => c,
            Err(e) => return fail(format!("stage {stage}: changed files: {e}")),
        };
        let scenario = parent.join(format!("s15-stage{stage}-{}", std::process::id()));
        let patch_path = parent.join(format!("s15-stage{stage}-{}.patch", std::process::id()));
        let _ = std::fs::remove_dir_all(&scenario);
        let _ = std::fs::remove_file(&patch_path);
        if let Err(e) = std::fs::write(&patch_path, &patch) {
            return fail(format!("stage {stage}: write patch: {e}"));
        }
        if let Err(e) = crate::campaign::copy_dir_with_git(&workdir, &scenario) {
            let _ = std::fs::remove_dir_all(&scenario);
            let _ = std::fs::remove_file(&patch_path);
            return fail(format!("stage {stage}: scratch copy failed: {e}"));
        }
        // The copy's files have fresh mtimes; refresh the index's stat
        // cache first or `--3way` mistakes a clean file for a dirty one.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&scenario)
            .args(["update-index", "--refresh"])
            .output();
        let applied = std::process::Command::new("git")
            .arg("-C")
            .arg(&scenario)
            .args(["apply", "-R", "--3way"])
            .arg(&patch_path)
            .output();
        let narrow = matches!(applied, Ok(ref out) if out.status.success());
        if !narrow {
            // A partial/conflicted reverse is discarded; the fallback copy
            // restores whole files instead (and is logged as a fallback).
            let _ = std::fs::remove_dir_all(&scenario);
            if let Err(e) = crate::campaign::copy_dir_excluding_git(&workdir, &scenario) {
                let _ = std::fs::remove_dir_all(&scenario);
                let _ = std::fs::remove_file(&patch_path);
                return fail(format!("stage {stage}: fallback scratch copy failed: {e}"));
            }
            let revert = (|| -> Result<(), String> {
                for f in changed.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    let target = scenario.join(f);
                    match git_in(&workdir, &["show", &format!("{parent_commit}:{f}")]) {
                        Ok(content) => {
                            if let Some(p) = target.parent() {
                                let _ = std::fs::create_dir_all(p);
                            }
                            std::fs::write(&target, content)
                                .map_err(|e| format!("write {}: {e}", target.display()))?;
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
                let _ = std::fs::remove_file(&patch_path);
                return fail(format!("stage {stage}: sensitivity setup failed: {e}"));
            }
        }
        // L-a: a red suite is only meaningful if the reverted copy still
        // boots and answers a basic query. Otherwise a whole-file fallback
        // that broke the server would "prove" sensitivity trivially. The
        // full stage-0/7 harness comparison remains a known residual.
        if scenario.join("start.sh").is_file() {
            let probe = sandbox.run_workspace_command(
                &scenario,
                concat!(
                    "set -e\n",
                    "mkdir -p /tmp/sens-data\n",
                    "bash ./start.sh --port 7123 --data /tmp/sens-data >/tmp/sens.log 2>&1 &\n",
                    "pid=$!\n",
                    "trap 'kill $pid 2>/dev/null || true' EXIT\n",
                    "python3 - <<'PY'\n",
                    "import socket, sys, time\n",
                    "for _ in range(500):\n",
                    "    try:\n",
                    "        s = socket.create_connection((\"127.0.0.1\", 7123), 0.2)\n",
                    "        break\n",
                    "    except OSError:\n",
                    "        time.sleep(0.05)\n",
                    "else:\n",
                    "    sys.exit(\"server did not start within 25 s\")\n",
                    "s.settimeout(10)\n",
                    "s.sendall(b\"SELECT 1\\n\")\n",
                    "line = s.makefile(\"rb\").readline()\n",
                    "if not line.startswith(b\"ROWS\"):\n",
                    "    sys.exit(\"unexpected answer: %r\" % line)\n",
                    "PY\n",
                ),
                45,
            );
            match probe {
                Ok((true, _)) => {}
                Ok((false, out)) => {
                    let _ = std::fs::remove_dir_all(&scenario);
                    let _ = std::fs::remove_file(&patch_path);
                    return fail(format!(
                        "sensitivity is inconclusive: the reverted copy does not answer SELECT 1, so ./test.sh failing proves nothing: {}",
                        output_tail(&out, 400)
                    ));
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&scenario);
                    let _ = std::fs::remove_file(&patch_path);
                    return fail(format!("sensitivity probe could not run: {e}"));
                }
            }
        }
        let red = run_suite(&scenario);
        let _ = std::fs::remove_dir_all(&scenario);
        let _ = std::fs::remove_file(&patch_path);
        match red {
            Ok((false, _)) => {
                let how = if narrow {
                    format!("stage-{stage} diff reversed")
                } else {
                    format!("stage-{stage} whole-file fallback; the diff did not reverse cleanly")
                };
                logger.log(&format!(
                    "  [TESTS] sensitivity ok ({how}): the suite goes red"
                ));
            }
            Ok((true, _)) => {
                return fail(format!(
                    "./test.sh stayed green with the stage-{stage} {} reverted — the tests do not catch regressions",
                    if narrow {
                        "diff"
                    } else {
                        "feature (whole-file fallback)"
                    }
                ))
            }
            Err(e) => {
                return fail(format!(
                    "stage-{stage} sensitivity run could not execute: {e}"
                ))
            }
        }
    }
    crate::verifier::StageResult {
        stage: 15,
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
/// R23 D5: the ticket prompt's dollar line. Turns are never the limit;
/// the session should stop on completion or when this API budget is spent.
pub fn session_budget_line(budget_usd: f64) -> String {
    if budget_usd.is_finite() && budget_usd > 0.0 {
        include_str!("prompts/session_budget.txt")
            .replace("{{BUDGET}}", &format!("{budget_usd:.2}"))
    } else {
        "## Session budget\nTurns are not the limit; stop when the work is complete.".to_string()
    }
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
    /// Round-14 N2/N3: the run is not a measurement at all - the agent never
    /// reached its first turn (after one retry), or the provider rejected
    /// the key/billing. A campaign stops on it and never publishes the cell.
    pub harness_fault: Option<String>,
    /// The largest single provider call this attempt made (the campaign
    /// cap's reserve, round-17 P0).
    pub max_call_usd: f64,
    /// R23 D1/D5: why the final agent session ended: dollars | turns | wall |
    /// ready | crash. None for eval-only runs.
    pub stop_reason: Option<String>,
    /// R48 F2: seconds spent waiting for the host grading lock.
    pub grading_wait_s: f64,
}

/// How an agent session that returned `Err` ended (round-14 N3): its partial
/// stats, whether the agent ever showed a sign of life, and the provider
/// billing/auth reason when that was the cause. Both error envelopes carry
/// stats; anything else (a spawn failure, say) never started.
struct AgentErrorEnd {
    partial: crate::sandbox::omp::OmpSessionStats,
    started: bool,
    billing: Option<String>,
}

fn agent_error_end(e: &anyhow::Error) -> AgentErrorEnd {
    use crate::sandbox::omp::{OmpBillingError, OmpExecutionError};
    if let Some(be) = e.downcast_ref::<OmpBillingError>() {
        let p = be.partial_stats.clone();
        return AgentErrorEnd {
            started: p.steps_taken > 0 || p.total_tokens > 0,
            partial: p,
            billing: Some(be.reason.clone()),
        };
    }
    if let Some(oe) = e.downcast_ref::<OmpExecutionError>() {
        return AgentErrorEnd {
            partial: oe.partial_stats.clone(),
            started: oe.started,
            billing: None,
        };
    }
    AgentErrorEnd {
        partial: Default::default(),
        started: false,
        billing: None,
    }
}

/// Publish the WAL crash-recovery probe: 2000 MSGs on `recovery_probe`,
/// counting acks. The caller SIGKILLs the candidate next; `check_recovery`
/// then requires this state back.
///
/// Bounded by an overall 60s deadline so one wedged candidate cannot stall
/// the pipeline past the trial timeout. Acks collected before the deadline
/// are kept, never zeroed: check_recovery demands every acked message back,
/// so partial progress stays verifiable instead of discarded.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    pub pass_rate: f64,
    /// R48 F2: seconds this verification waited for the grading lock.
    pub grading_wait_s: f64,
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
        lite_profile: &'a [String],
        passed_lite: &'a [(u32, Vec<String>)],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = VerificationOutcome> + Send + 'a>> {
        Box::pin(async move {
            logger
                .log("[VERIFY] Checking anti-cheat compliance (no prebuilt frameworks/daemons)...");
            let mut disqualified = false;
            let mut disqualification_reason = None;
            // sdb-sql adds its own scan: no embedded engine or SQL parser.
            // Task-scoped so the other tasks' grading is unchanged.
            let compliance = ComplianceChecker::check_no_frameworks(workdir).and_then(|()| {
                if matches!(task, TaskType::Sql) {
                    ComplianceChecker::check_no_embedded_sql(workdir)
                } else {
                    Ok(())
                }
            });
            if let Err(violation) = compliance {
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

            let extra_throughput: Option<f64> = None;
            let extra_robustness: Vec<crate::verifier::StageResult> = Vec::new();
            let mut infra_error = false;
            let mut grading_wait_s = 0.0f64;

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
                // R48 F2: a graded SQL attempt holds the host-wide lock from
                // the candidate start through the verdict (shrink included).
                // The model's session and builds stay parallel. The wait is
                // recorded and excluded from the wall cap (D2 a).
                let _grading_lock = if matches!(task, TaskType::Sql) {
                    let path = crate::grading::lock_path();
                    match tokio::task::spawn_blocking(move || crate::grading::acquire(&path)).await
                    {
                        Ok(Ok(lock)) => {
                            grading_wait_s = lock.wait.as_secs_f64();
                            Some(lock)
                        }
                        // A lock we cannot take is a harness fault, never a
                        // quiet unlocked grading run.
                        Ok(Err(e)) => {
                            logger.log(&format!("[INFRA] grading lock: {e}"));
                            infra_error = true;
                            None
                        }
                        Err(e) => {
                            logger.log(&format!("[INFRA] grading lock task: {e}"));
                            infra_error = true;
                            None
                        }
                    }
                } else {
                    None
                };
                // Extra published ports, gated on the ticket's acceptance:
                // 8090 for the dashboard stage (10), 6395 for the admin
                // binary-protocol stage (16). Publishing an unneeded port
                // would add a collision failure mode.
                let extra_ports: Vec<u16> = Vec::new();
                let mut extras = crate::sandbox::docker::CandidateRunExtras {
                    ports: extra_ports,
                    ..Default::default()
                };
                let mut sql_data: Option<PathBuf> = None;
                if matches!(task, TaskType::Sql) {
                    // R48 F2: graded containers get the fast cores; the
                    // agent's sandbox runs on the session set.
                    extras.cpuset = Some(crate::grading::fast_cpuset());
                    // The launch contract becomes real: the harness passes
                    // --port/--data and owns the data directory.
                    let data = sql_data_dir(workdir, accepted.is_some());
                    if let Err(e) = fs::create_dir_all(&data) {
                        logger.log(&format!(
                            "[INFRA] could not create the sdb-sql data dir {}: {e}",
                            data.display()
                        ));
                        infra_error = true;
                    } else {
                        logger.log(&format!(
                            "[SETUP] sdb-sql data dir {} -> /sdb-data",
                            data.display()
                        ));
                        extras
                            .binds
                            .push((data.display().to_string(), "/sdb-data".to_string()));
                        extras.launch_args = vec![
                            "--port".to_string(),
                            port.to_string(),
                            "--data".to_string(),
                            "/sdb-data".to_string(),
                        ];
                        // Also visible to CMD-only Dockerfile images, which
                        // are started without argv.
                        extras.env.push(("SDB_PORT".to_string(), port.to_string()));
                        extras
                            .env
                            .push(("SDB_DATA_DIR".to_string(), "/sdb-data".to_string()));
                        sql_data = Some(data);
                    }
                }
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
                    // Readiness grace (proxy/http): a candidate that binds
                    // early but serves late (slow startup self-test) must
                    // not eat the verifier's single first request. Bounded
                    // so a genuinely dead server is graded as-is.
                } else {
                    logger.log(&format!("[WARN] Port {} not responding within 25s timeout. Proceeding with verification.", port));
                }

                match task {
                    TaskType::Sql => {
                        // The differential suite: one script per accepted
                        // stage with the cumulative profile, a fresh
                        // per-(verification, stage) seed, and shrink on
                        // failure. `spawn_blocking` because a shrink replays
                        // hundreds of sequences.
                        let mut wanted: Vec<u32> = accepted
                            .map(|a| a.to_vec())
                            .unwrap_or_else(|| vec![0, 1, 2, 3]);
                        wanted.sort_unstable();
                        wanted.dedup();
                        let log_stage =
                            |logger: &Arc<dyn PipelineLogger>, s: &crate::verifier::StageResult| {
                                logger.log(&format!(
                                    "  Stage {}: {} -> {} {}",
                                    s.stage,
                                    crate::config::stage_label(s.stage, &s.name),
                                    if s.passed { "PASS" } else { "FAIL" },
                                    s.error.as_deref().unwrap_or("")
                                ));
                            };
                        let mut stages: Vec<crate::verifier::StageResult> = Vec::new();
                        let mut prior_failed = false;
                        // Upgrade checks run before the suite, against the
                        // harness-owned store: data written when an earlier
                        // ticket passed must still answer (docs/PROPOSAL-sdb-sql.md,
                        // "Upgrade check"). Drift fails that ticket's stage
                        // again (regression class `upgrade`); an unreachable
                        // server is left to the suite, which fails anyway.
                        let sentinel_dir = sql_data
                            .as_ref()
                            .and_then(|d| d.parent().map(Path::to_path_buf));
                        let sentinels: Vec<crate::verifier::sql::sentinel::Sentinel> =
                            if accepted.is_some() {
                                sentinel_dir
                                    .as_deref()
                                    .map(crate::verifier::sql::sentinel::load_all)
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            };
                        let mut upgrade_failed: Option<(u32, String)> = None;
                        for sent in &sentinels {
                            match crate::verifier::sql::sentinel::check(
                                &format!("127.0.0.1:{port}"),
                                std::time::Duration::from_secs(10),
                                sent,
                            ) {
                                Ok(()) => logger.log(&format!(
                                    "  [UPGRADE] sentinel from stage {} verified ({} reads)",
                                    sent.stage,
                                    sent.queries.len()
                                )),
                                Err(e) if e.starts_with("drift:") => {
                                    logger.log(&format!(
                                        "  [UPGRADE] sentinel from stage {} FAILED: {e}",
                                        sent.stage
                                    ));
                                    upgrade_failed = Some((sent.stage, e));
                                    break;
                                }
                                Err(e) => logger.log(&format!(
                                    "  [UPGRADE] sentinel from stage {} not checkable now: {e}",
                                    sent.stage
                                )),
                            }
                        }
                        for &stage in &wanted {
                            let name = format!(
                                "Stage {stage}: {}",
                                crate::verifier::sql::driver::stage_title(stage)
                            );
                            if crate::verifier::sql::driver::scenario_only(stage) {
                                // A scenario-only ticket (ticket 12 first):
                                // its checks run after the suite, once the
                                // earlier stages are green, like the crash
                                // ladder, transactions and the perf gate. A
                                // placeholder pass lets the late-failure path
                                // flip it when its scenario fails; when an
                                // earlier stage failed the scenario never
                                // runs, so it must not display as PASS
                                // (round-15 L1).
                                if prior_failed {
                                    let st = crate::verifier::StageResult {
                                        stage,
                                        name,
                                        passed: false,
                                        error: Some("not run: an earlier stage failed".to_string()),
                                    };
                                    log_stage(logger, &st);
                                    stages.push(st);
                                    continue;
                                }
                                logger.log(&format!(
                                    "  [SCENARIO] stage {stage}: scripted checks run after the suite"
                                ));
                                let st = crate::verifier::StageResult {
                                    stage,
                                    name,
                                    passed: true,
                                    error: None,
                                };
                                log_stage(logger, &st);
                                stages.push(st);
                                continue;
                            }
                            if !crate::verifier::sql::driver::differential_supported(stage) {
                                let st = crate::verifier::StageResult {
                                    stage,
                                    name,
                                    passed: false,
                                    error: Some(
                                        "differential suite for this stage is not built yet"
                                            .to_string(),
                                    ),
                                };
                                prior_failed = true;
                                log_stage(logger, &st);
                                stages.push(st);
                                continue;
                            }
                            if prior_failed {
                                let st = crate::verifier::StageResult {
                                    stage,
                                    name,
                                    passed: false,
                                    error: Some(
                                        "skipped due to failure in earlier stages".to_string(),
                                    ),
                                };
                                log_stage(logger, &st);
                                stages.push(st);
                                continue;
                            }
                            let passed_ids: Vec<u32> =
                                wanted.iter().copied().filter(|&s| s <= stage).collect();
                            let addr = format!("127.0.0.1:{port}");
                            let seed =
                                crate::verifier::sql::driver::stage_seed(verifier_seed, stage);
                            // The shrink replay needs a *fresh* database, but
                            // the real one is the candidate's persistent
                            // store (ticket 4+) and must survive. Every trial
                            // binds a throwaway directory instead; once the
                            // stage is graded the candidate goes back on the
                            // real store for the repair round and diagnostics.
                            let instance_id = sandbox.instance_id.clone();
                            let reset_dir = workdir.to_path_buf();
                            let real_extras = extras.clone();
                            // Per instance: two verifications in the same
                            // campaign directory (or two campaigns sharing a
                            // workspace) must not share trial stores.
                            let shrink_tag: String = instance_id
                                .chars()
                                .filter(|c| c.is_ascii_alphanumeric())
                                .collect();
                            let shrink_root = sql_data
                                .as_ref()
                                .map(|d| d.with_file_name(format!("sql-shrink-{shrink_tag}")))
                                .unwrap_or_else(|| {
                                    std::env::temp_dir()
                                        .join(format!("sdb-sql-shrink-{shrink_tag}"))
                                });
                            let shrink_root_keep = shrink_root.clone();
                            let mut trial = 0u32;
                            let mut reset = move || -> Result<(), String> {
                                trial += 1;
                                let dir = shrink_root.join(format!("t{trial}"));
                                let _ = std::fs::remove_dir_all(&dir);
                                std::fs::create_dir_all(&dir)
                                    .map_err(|e| format!("shrink dir {}: {e}", dir.display()))?;
                                let mut trial_extras = real_extras.clone();
                                trial_extras.binds =
                                    vec![(dir.display().to_string(), "/sdb-data".to_string())];
                                // Non-conforming candidates may still use the
                                // workspace store; wipe it too.
                                let _ = std::fs::remove_dir_all(reset_dir.join("data"));
                                restart_candidate(
                                    &instance_id,
                                    &reset_dir,
                                    port,
                                    &trial_extras,
                                    false,
                                )
                            };
                            // Stage 4+'s persistence scenario restarts the
                            // candidate *keeping* its store.
                            let restart_instance = sandbox.instance_id.clone();
                            let restart_dir = workdir.to_path_buf();
                            let restart_extras = extras.clone();
                            let mut restart = move || -> Result<(), String> {
                                restart_candidate(
                                    &restart_instance,
                                    &restart_dir,
                                    port,
                                    &restart_extras,
                                    true,
                                )
                            };
                            // The variant each stage passed at: the
                            // current attempt's lite profile applies to the
                            // ticket being graded, earlier lite passes come
                            // from the ladder (`passed_lite`).
                            let variants = suite_variants(passed_lite, lite_profile, &wanted);
                            let run = tokio::task::spawn_blocking(move || {
                                crate::verifier::sql::driver::run_stage_with_variants(
                                    &addr,
                                    std::time::Duration::from_secs(10),
                                    seed,
                                    &passed_ids,
                                    &variants,
                                    crate::verifier::sql::driver::DEFAULT_STEPS,
                                    Some(&mut reset),
                                    Some(&mut restart),
                                )
                            })
                            .await;
                            if matches!(&run, Ok(Ok(out)) if !out.passed) {
                                // The shrink left the candidate on a
                                // throwaway bind; put it back on the real
                                // store.
                                if let Err(e) = restart_candidate(
                                    &sandbox.instance_id,
                                    workdir,
                                    port,
                                    &extras,
                                    false,
                                ) {
                                    logger.log(&format!(
                                        "[SETUP] Warning: could not restore the candidate after \
                                         the shrink: {e}"
                                    ));
                                }
                            }
                            // The trial stores are throwaway. The campaign
                            // clean-up would get them too, but a long run
                            // should not hold every trial's rows.
                            let _ = std::fs::remove_dir_all(&shrink_root_keep);
                            let st = match run {
                                Ok(Ok(out)) if out.passed => {
                                    logger.log(&format!(
                                        "    {} statements graded (seed {seed})",
                                        out.checks
                                    ));
                                    crate::verifier::StageResult {
                                        stage,
                                        name,
                                        passed: true,
                                        error: None,
                                    }
                                }
                                Ok(Ok(out)) => {
                                    if let Some(repro) = &out.repro {
                                        // Visible in the run log: a repro
                                        // with 0 trials is the unminimized
                                        // prefix (the reset was not trusted).
                                        logger.log(&format!(
                                            "    repro: {} statements ({} shrink trials)",
                                            repro.statements.len(),
                                            repro.checks
                                        ));
                                    }
                                    let mut err = out.detail.clone();
                                    if let Some(repro) = &out.repro {
                                        err.push('\n');
                                        err.push_str(&repro.render());
                                    }
                                    crate::verifier::StageResult {
                                        stage,
                                        name,
                                        passed: false,
                                        error: Some(err),
                                    }
                                }
                                Ok(Err(e)) => {
                                    infra_error = true;
                                    logger.log(&format!(
                                        "[INFRA] sql verifier fault at stage {stage}: {e}"
                                    ));
                                    crate::verifier::StageResult {
                                        stage,
                                        name,
                                        passed: false,
                                        error: Some(format!("verifier fault: {e}")),
                                    }
                                }
                                Err(join) => {
                                    infra_error = true;
                                    crate::verifier::StageResult {
                                        stage,
                                        name,
                                        passed: false,
                                        error: Some(format!("verifier task failed: {join}")),
                                    }
                                }
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        let mut late_failure: Option<(u32, String)> = upgrade_failed.take();
                        // Scenario-only stages whose late check actually ran;
                        // the rest must not keep their placeholder PASS (L1).
                        let mut late_ran: Vec<u32> = Vec::new();
                        if late_failure.is_none() && stages.iter().all(|s| s.passed) {
                            // Stage 5's suite adds the crash ladder, which
                            // needs kill/restart/truncate: the pipeline owns
                            // the container, so it runs here.
                            if wanted.contains(&5) {
                                if let Some(data) = &sql_data {
                                    late_ran.push(5);
                                    // Stage 5's variant comes from this
                                    // attempt when stage 5 *is* the current
                                    // ticket, else from the pass that earned
                                    // it: ticket 6+ must not demand a full
                                    // crash ladder if the model won stage 5
                                    // on the lite variant.
                                    let lite = if wanted.iter().copied().max() == Some(5) {
                                        lite_profile.iter().any(|p| p == "crash-acked-only")
                                    } else {
                                        passed_lite.iter().any(|(s, p)| {
                                            *s == 5 && p.iter().any(|f| f == "crash-acked-only")
                                        })
                                    };
                                    if let Err(e) = crash_ladder(
                                        sandbox,
                                        workdir,
                                        port,
                                        &extras,
                                        data,
                                        verifier_seed,
                                        lite,
                                        logger,
                                    ) {
                                        logger.log(&format!("  [CRASH] FAILED: {e}"));
                                        late_failure = Some((5, format!("[crash] {e}")));
                                    }
                                }
                            }
                            // Stage 6's suite: cross-connection isolation and
                            // a kill with a transaction open (needs the
                            // sandbox's kill/restart, so it runs here too).
                            if late_failure.is_none() && wanted.contains(&6) {
                                late_ran.push(6);
                                let ns = crate::verifier::sql::driver::stage_seed(verifier_seed, 6)
                                    ^ 0x5E27_1E10_0000_0006;
                                let addr = format!("127.0.0.1:{port}");
                                let t = std::time::Duration::from_secs(20);
                                let iso = crate::verifier::sql::txn::isolation(
                                    &addr,
                                    t,
                                    &format!("c{ns:x}_iso"),
                                );
                                let killed = iso.and_then(|()| {
                                    logger.log(
                                        "  [TXN] uncommitted rows stay invisible across connections",
                                    );
                                    let mut kill =
                                        || sandbox.kill_candidate().map_err(|e| e.to_string());
                                    let mut restart = || {
                                        restart_candidate(
                                            &sandbox.instance_id,
                                            workdir,
                                            port,
                                            &extras,
                                            false,
                                        )
                                    };
                                    crate::verifier::sql::txn::kill_with_open_transaction(
                                        &addr,
                                        t,
                                        &format!("c{ns:x}_kill"),
                                        &mut kill,
                                        &mut restart,
                                    )
                                });
                                match killed {
                                    Ok(()) => logger.log(
                                        "  [TXN] a kill with an open transaction discarded it",
                                    ),
                                    Err(e) => {
                                        logger.log(&format!("  [TXN] FAILED: {e}"));
                                        late_failure = Some((6, format!("[txn] {e}")));
                                    }
                                }
                            }
                            // Stage 12's suite: serializable concurrency
                            // (docs/PROPOSAL-sdb-sql.md, "12. Concurrency
                            // and isolation"). The lite variant is one
                            // global lock: it passes the lost-update and
                            // transfer checks but fails the reader check.
                            if late_failure.is_none() && wanted.contains(&12) {
                                late_ran.push(12);
                                let ns =
                                    crate::verifier::sql::driver::stage_seed(verifier_seed, 12)
                                        ^ 0x5E27_1E10_0000_000C;
                                let lite = if wanted.iter().copied().max() == Some(12) {
                                    lite_profile.iter().any(|p| p == "concurrency-global-lock")
                                } else {
                                    passed_lite.iter().any(|(s, p)| {
                                        *s == 12 && p.iter().any(|f| f == "concurrency-global-lock")
                                    })
                                };
                                let addr = format!("127.0.0.1:{port}");
                                match crate::verifier::sql::concurrency::scenario(
                                    &addr,
                                    std::time::Duration::from_secs(10),
                                    ns,
                                    lite,
                                ) {
                                    Ok(()) => logger.log(if lite {
                                        "  [CONCURRENCY] lite: lost update and transfers are serializable (global lock)"
                                    } else {
                                        "  [CONCURRENCY] serializable: lost update, transfers, and readers never blocked"
                                    }),
                                    Err(e) => {
                                        logger.log(&format!("  [CONCURRENCY] FAILED: {e}"));
                                        late_failure = Some((12, format!("[concurrency] {e}")));
                                    }
                                }
                            }
                            // Stage 14's suite: EXPLAIN plans and exact
                            // SHOW STATS counters (docs/PROPOSAL-sdb-sql.md,
                            // "14. Observability").
                            if late_failure.is_none() && wanted.contains(&14) {
                                late_ran.push(14);
                                let ns =
                                    crate::verifier::sql::driver::stage_seed(verifier_seed, 14)
                                        ^ 0x5E27_1E10_0000_000E;
                                let addr = format!("127.0.0.1:{port}");
                                match crate::verifier::sql::observability::scenario(
                                    &addr,
                                    std::time::Duration::from_secs(10),
                                    ns,
                                ) {
                                    Ok(()) => logger.log(
                                        "  [OBSERVABILITY] EXPLAIN names the index/scan; SHOW STATS deltas are exact",
                                    ),
                                    Err(e) => {
                                        logger.log(&format!("  [OBSERVABILITY] FAILED: {e}"));
                                        late_failure = Some((14, format!("[observability] {e}")));
                                    }
                                }
                            }
                            // Ticket 15's suite: the candidate's own
                            // tests, coverage and sensitivity
                            // (docs/PROPOSAL-sdb-sql.md, "15. Tests and CI
                            // gate"). `./test.sh` runs in the sandbox.
                            if late_failure.is_none() && wanted.contains(&15) {
                                late_ran.push(15);
                                let lite = if wanted.iter().copied().max() == Some(15) {
                                    lite_profile.iter().any(|p| p == "coverage-60")
                                } else {
                                    passed_lite.iter().any(|(s, p)| {
                                        *s == 15 && p.iter().any(|f| f == "coverage-60")
                                    })
                                };
                                let sandbox15 =
                                    SandboxManager::with_id(&format!("s15-w{}", worker));
                                let workdir15 = workdir.to_path_buf();
                                let logger15 = logger.clone();
                                match tokio::task::spawn_blocking(move || {
                                    run_tests_gate_blocking(sandbox15, workdir15, logger15, lite)
                                })
                                .await
                                {
                                    Ok(st) if st.passed => logger.log(
                                        "  [TESTS] the candidate's suite is green, covered, and sensitive to stage 7/8 regressions",
                                    ),
                                    Ok(st) => {
                                        let why = st.error.unwrap_or_default();
                                        logger.log(&format!("  [TESTS] FAILED: {why}"));
                                        late_failure =
                                            Some((15, format!("[tests] {why}")));
                                    }
                                    Err(e) => {
                                        logger.log(&format!(
                                            "  [TESTS] FAILED: the gate task failed: {e}"
                                        ));
                                        late_failure = Some((
                                            15,
                                            format!("[tests] the gate task failed: {e}"),
                                        ));
                                    }
                                }
                            }
                            // Stage 11's perf gate: an index must change
                            // speed, not answers (docs/PROPOSAL-sdb-sql.md,
                            // "11. Indexes"). Timing is not part of the
                            // differential suite and a ratio is only honest
                            // on an idle host, so it runs here, under the
                            // bench lock, once every stage passed. The gate
                            // writes two 200k-row tables and the dialect has
                            // no DROP TABLE, so it runs on a *fresh scratch
                            // store* (the crash-ladder pattern without the
                            // copy): the real store never pays for them on
                            // later tickets, and the mid-gate restart is the
                            // ticket-4 graceful one (SIGKILL is ticket 5's
                            // contract, and ticket 11 needs only [3, 4]).
                            if late_failure.is_none() && wanted.contains(&11) {
                                late_ran.push(11);
                                let lite = if wanted.iter().copied().max() == Some(11) {
                                    lite_profile.iter().any(|p| p == "index-equality-only")
                                } else {
                                    passed_lite.iter().any(|(s, p)| {
                                        *s == 11 && p.iter().any(|f| f == "index-equality-only")
                                    })
                                };
                                let addr = format!("127.0.0.1:{port}");
                                let lock = crate::bench::acquire_bench_lock();
                                if !lock.is_owned() {
                                    logger
                                        .log("  [PERF] bench lock busy: the numbers may be noisy");
                                }
                                let ns =
                                    crate::verifier::sql::driver::stage_seed(verifier_seed, 11)
                                        ^ 0x5E27_1E10_0000_0011;
                                let t = std::time::Duration::from_secs(30);
                                let run = |e: &crate::sandbox::docker::CandidateRunExtras| {
                                    restart_candidate(&sandbox.instance_id, workdir, port, e, true)
                                };
                                let scratch = sql_data.as_ref().map(|data| {
                                    data.with_file_name(format!(
                                        "{}-perf",
                                        data.file_name()
                                            .map(|n| n.to_string_lossy())
                                            .unwrap_or_default()
                                    ))
                                });
                                let scratch_extras = scratch.as_ref().map(|scratch| {
                                    let mut e = extras.clone();
                                    let real_src = sql_data
                                        .as_ref()
                                        .map(|d| d.display().to_string())
                                        .unwrap_or_default();
                                    for b in e.binds.iter_mut() {
                                        if b.0 == real_src {
                                            b.0 = scratch.display().to_string();
                                        }
                                    }
                                    e
                                });
                                let body: Result<String, String> = (|| {
                                    if let Some(scratch) = &scratch {
                                        let _ = std::fs::remove_dir_all(scratch);
                                        std::fs::create_dir_all(scratch)
                                            .map_err(|e| format!("scratch store: {e}"))?;
                                        run(scratch_extras.as_ref().expect("extras"))?;
                                        logger.log(&format!(
                                            "  [PERF] gate runs on a fresh scratch store ({})",
                                            scratch.display()
                                        ));
                                    }
                                    let e = scratch_extras.as_ref().unwrap_or(&extras);
                                    let mut restart = || run(e);
                                    crate::verifier::sql::perf::perf_gate(
                                        &addr,
                                        t,
                                        ns,
                                        lite,
                                        Some(&mut restart),
                                    )
                                })(
                                );
                                // Always back onto the real store, then
                                // drop the scratch copy.
                                let restored = run(&extras);
                                if let Some(scratch) = &scratch {
                                    let _ = std::fs::remove_dir_all(scratch);
                                }
                                match body {
                                    Ok(report) => logger.log(&format!("  [PERF] {report}")),
                                    Err(e) => {
                                        logger.log(&format!("  [PERF] FAILED: {e}"));
                                        late_failure = Some((11, format!("[perf] {e}")));
                                    }
                                }
                                if let Err(e) = restored {
                                    logger.log(&format!(
                                        "  [PERF] FAILED: back on the real store: {e}"
                                    ));
                                    if late_failure.is_none() {
                                        late_failure = Some((11, format!("[perf] restart: {e}")));
                                    }
                                }
                            }
                            // Ticket 16's store: a fresh scratch dir, the
                            // same binary restarted under a 64 MiB cap, then
                            // ~400 MiB of rows, a full aggregate, an
                            // external sort and an indexed join
                            // (docs/PROPOSAL-sdb-sql.md, "16. Larger than
                            // memory"). The heavy store is thrown away; the
                            // real store never sees it.
                            if late_failure.is_none() && wanted.contains(&16) {
                                late_ran.push(16);
                                let scratch = sql_data.as_ref().map(|data| {
                                    data.with_file_name(format!(
                                        "{}-mem",
                                        data.file_name()
                                            .map(|n| n.to_string_lossy())
                                            .unwrap_or_default()
                                    ))
                                });
                                let mut scratch_extras = extras.clone();
                                if let Some(scratch) = &scratch {
                                    let real_src = sql_data
                                        .as_ref()
                                        .map(|d| d.display().to_string())
                                        .unwrap_or_default();
                                    for b in scratch_extras.binds.iter_mut() {
                                        if b.0 == real_src {
                                            b.0 = scratch.display().to_string();
                                        }
                                    }
                                }
                                scratch_extras.memory = Some("64m".to_string());
                                let run = |e: &crate::sandbox::docker::CandidateRunExtras| {
                                    restart_candidate(&sandbox.instance_id, workdir, port, e, true)
                                };
                                let body: Result<String, String> = (|| {
                                    if let Some(scratch) = &scratch {
                                        let _ = std::fs::remove_dir_all(scratch);
                                        std::fs::create_dir_all(scratch)
                                            .map_err(|e| format!("scratch store: {e}"))?;
                                        logger.log(
                                            "  [MEMORY] gate runs on a fresh scratch store under a 64 MiB cap",
                                        );
                                    }
                                    run(&scratch_extras)?;
                                    let addr = format!("127.0.0.1:{port}");
                                    crate::verifier::sql::memory::scenario(
                                        &addr,
                                        &crate::verifier::sql::memory::MemoryConfig::full(),
                                    )
                                })(
                                );
                                // Always back onto the real store (and its
                                // normal memory limit), then drop the heavy
                                // scratch copy.
                                let restored = run(&extras);
                                if let Some(scratch) = &scratch {
                                    let _ = std::fs::remove_dir_all(scratch);
                                }
                                match body {
                                    Ok(report) => logger.log(&format!("  [MEMORY] {report}")),
                                    Err(e) => {
                                        logger.log(&format!("  [MEMORY] FAILED: {e}"));
                                        late_failure = Some((16, format!("[memory] {e}")));
                                    }
                                }
                                if let Err(e) = restored {
                                    logger.log(&format!(
                                        "  [MEMORY] FAILED: back on the real store: {e}"
                                    ));
                                    if late_failure.is_none() {
                                        late_failure = Some((16, format!("[memory] restart: {e}")));
                                    }
                                }
                            }
                            // Ticket 17's incident: the candidate runs on a
                            // fresh 16 MiB tmpfs; the harness fills it with a
                            // foreign filler, a too-large write must answer
                            // `ERR full` without being acked or applied, the
                            // seeds must stay readable, and the same write
                            // must succeed after the filler is removed
                            // (docs/PROPOSAL-sdb-sql.md, "17. Disk full").
                            if late_failure.is_none() && wanted.contains(&17) {
                                late_ran.push(17);
                                let mut disk_extras = extras.clone();
                                disk_extras.binds.clear();
                                disk_extras.tmpfs = vec!["/sdb-data:size=16m".to_string()];
                                let run = |e: &crate::sandbox::docker::CandidateRunExtras| {
                                    restart_candidate(&sandbox.instance_id, workdir, port, e, true)
                                };
                                let container = sandbox.candidate_container_name();
                                let mut control = |op: crate::verifier::sql::disk::DiskOp| {
                                    let (what, cmd) = match op {
                                        crate::verifier::sql::disk::DiskOp::Fill => (
                                            "fill",
                                            // Append (a second Fill after a failed
                                            // statement must not truncate the
                                            // filler) and eat the sub-block tail
                                            // `bs=1M` leaves, so the volume is full
                                            // to the byte.
                                            "dd if=/dev/zero of=/sdb-data/fill bs=1M count=64 status=none oflag=append conv=notrunc 2>/dev/null || true;                                              dd if=/dev/zero of=/sdb-data/fill bs=4096 count=256 status=none oflag=append conv=notrunc 2>/dev/null || true;                                              dd if=/dev/zero of=/sdb-data/fill bs=1 count=8192 status=none oflag=append conv=notrunc 2>/dev/null || true",
                                        ),
                                        crate::verifier::sql::disk::DiskOp::Free => {
                                            ("free", "rm -f /sdb-data/fill")
                                        }
                                    };
                                    let out = std::process::Command::new("docker")
                                        .args(["exec", &container, "sh", "-c", cmd])
                                        .output()
                                        .map_err(|e| format!("{what}: docker exec: {e}"))?;
                                    if !out.status.success() {
                                        return Err(format!(
                                            "{what}: {}",
                                            String::from_utf8_lossy(&out.stderr).trim()
                                        ));
                                    }
                                    Ok(())
                                };
                                let body: Result<String, String> = (|| {
                                    run(&disk_extras)?;
                                    logger.log(
                                        "  [DISK] incident runs on a fresh 16 MiB tmpfs at /sdb-data",
                                    );
                                    let addr = format!("127.0.0.1:{port}");
                                    crate::verifier::sql::disk::incident(
                                        &addr,
                                        &crate::verifier::sql::disk::DiskConfig::full(),
                                        &mut control,
                                    )
                                })(
                                );
                                let restored = run(&extras);
                                match body {
                                    Ok(report) => logger.log(&format!("  [DISK] {report}")),
                                    Err(e) => {
                                        logger.log(&format!("  [DISK] FAILED: {e}"));
                                        late_failure = Some((17, format!("[disk] {e}")));
                                    }
                                }
                                if let Err(e) = restored {
                                    logger.log(&format!(
                                        "  [DISK] FAILED: back on the real store: {e}"
                                    ));
                                    if late_failure.is_none() {
                                        late_failure = Some((17, format!("[disk] restart: {e}")));
                                    }
                                }
                            }
                            // Ticket 18's hardening: the candidate is
                            // restarted on a fresh scratch store with a
                            // password and room for 256 connection threads,
                            // then driven through AUTH, fuzzing, the
                            // nesting/size/time limits, a slow-loris client
                            // and the connection cap (docs/PROPOSAL-sdb-sql.md,
                            // "18. Hostile input and limits"). The password
                            // must not reach an error or the container logs.
                            if late_failure.is_none() && wanted.contains(&18) {
                                late_ran.push(18);
                                let password = format!(
                                    "sdb-sec-{:x}",
                                    crate::verifier::sql::driver::stage_seed(verifier_seed, 18)
                                );
                                let scratch = sql_data.as_ref().map(|data| {
                                    data.with_file_name(format!(
                                        "{}-sec",
                                        data.file_name()
                                            .map(|n| n.to_string_lossy())
                                            .unwrap_or_default()
                                    ))
                                });
                                let mut sec_extras = extras.clone();
                                if let Some(scratch) = &scratch {
                                    let real_src = sql_data
                                        .as_ref()
                                        .map(|d| d.display().to_string())
                                        .unwrap_or_default();
                                    for b in sec_extras.binds.iter_mut() {
                                        if b.0 == real_src {
                                            b.0 = scratch.display().to_string();
                                        }
                                    }
                                }
                                sec_extras
                                    .env
                                    .push(("SDB_SQL_PASSWORD".to_string(), password.clone()));
                                // Ticket 18's 5 s statement budget is scoped
                                // to this restart; tickets 11/16 run long
                                // statements without it (round-16 I3).
                                sec_extras.env.push((
                                    "SDB_SQL_STATEMENT_TIMEOUT_MS".to_string(),
                                    "5000".to_string(),
                                ));
                                sec_extras.pids_limit = Some("1024".to_string());
                                let run = |e: &crate::sandbox::docker::CandidateRunExtras| {
                                    restart_candidate(&sandbox.instance_id, workdir, port, e, true)
                                };
                                let container = sandbox.candidate_container_name();
                                let body: Result<String, String> = (|| {
                                    if let Some(scratch) = &scratch {
                                        let _ = std::fs::remove_dir_all(scratch);
                                        std::fs::create_dir_all(scratch)
                                            .map_err(|e| format!("scratch store: {e}"))?;
                                        logger.log(
                                            "  [SECURITY] scenario runs on a fresh scratch store with SDB_SQL_PASSWORD set",
                                        );
                                    }
                                    run(&sec_extras)?;
                                    let addr = format!("127.0.0.1:{port}");
                                    let report = crate::verifier::sql::security::scenario(
                                        &addr,
                                        &crate::verifier::sql::security::SecurityConfig::full(
                                            password.clone(),
                                        ),
                                    )?;
                                    // The password may not leak into logs either
                                    // (the container is still the security one).
                                    let out = std::process::Command::new("docker")
                                        .args(["logs", &container])
                                        .output()
                                        .map_err(|e| format!("docker logs: {e}"))?;
                                    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                                    if text.contains(&password) {
                                        return Err(
                                            "the password appeared in the candidate's logs"
                                                .to_string(),
                                        );
                                    }
                                    Ok(report)
                                })(
                                );
                                let restored = run(&extras);
                                if let Some(scratch) = &scratch {
                                    let _ = std::fs::remove_dir_all(scratch);
                                }
                                match body {
                                    Ok(report) => logger.log(&format!("  [SECURITY] {report}")),
                                    Err(e) => {
                                        logger.log(&format!("  [SECURITY] FAILED: {e}"));
                                        late_failure = Some((18, format!("[security] {e}")));
                                    }
                                }
                                if let Err(e) = restored {
                                    logger.log(&format!(
                                        "  [SECURITY] FAILED: back on the real store: {e}"
                                    ));
                                    if late_failure.is_none() {
                                        late_failure =
                                            Some((18, format!("[security] restart: {e}")));
                                    }
                                }
                            }
                            // Ticket 19's cluster: three nodes on the HA
                            // containers (or two for the lite manual
                            // PROMOTE), seeded through the wire, failed
                            // over, rejoined, then the cumulative
                            // differential suite runs against the current
                            // primary (docs/PROPOSAL-sdb-sql.md,
                            // "19. Replication and failover").
                            if late_failure.is_none() && wanted.contains(&19) {
                                late_ran.push(19);
                                let lite = if wanted.iter().copied().max() == Some(19) {
                                    lite_profile.iter().any(|p| p == "two-nodes-promote")
                                } else {
                                    passed_lite.iter().any(|(s, p)| {
                                        *s == 19 && p.iter().any(|f| f == "two-nodes-promote")
                                    })
                                };
                                let node_count = if lite { 2 } else { 3 };
                                let base = port + 60;
                                let ports: Vec<u16> =
                                    (0..node_count).map(|i| base + i as u16).collect();
                                let nodes_env = ports
                                    .iter()
                                    .enumerate()
                                    .map(|(i, p)| format!("ha{}:{}", i + 1, p))
                                    .collect::<Vec<_>>()
                                    .join(",");
                                let started =
                                    sandbox.start_sql_ha_nodes(workdir, &ports, &nodes_env);
                                if let Err(e) = started {
                                    logger.log(&format!(
                                        "  [INFRA] REPL cluster could not start: {e}"
                                    ));
                                    infra_error = true;
                                    sandbox.stop_ha_nodes(node_count as u8);
                                    late_failure = Some((19, format!("[repl] start: {e}")));
                                } else {
                                    logger.log(&format!(
                                        "  [REPL] cluster of {node_count} nodes on ports {ports:?} ready"
                                    ));
                                    let seed =
                                        crate::verifier::sql::driver::stage_seed(verifier_seed, 19);
                                    let cfg = if lite {
                                        crate::verifier::sql::replication::ReplicationConfig::lite(
                                            seed,
                                        )
                                    } else {
                                        crate::verifier::sql::replication::ReplicationConfig::full(
                                            seed,
                                        )
                                    };
                                    // A control action that fails on a node
                                    // that is still up is a harness fault,
                                    // not the model's (round-18 N2).
                                    let control_fault = std::cell::Cell::new(false);
                                    let mut control =
                                        |action: crate::verifier::sql::replication::HaAction| {
                                            let done = match action {
                                            crate::verifier::sql::replication::HaAction::Kill(
                                                node,
                                            ) => sandbox
                                                .kill_ha_node((node + 1) as u8)
                                                .map_err(|e| format!("kill node {}: {e}", node + 1)),
                                            crate::verifier::sql::replication::HaAction::Pause(
                                                node,
                                            ) => sandbox
                                                .pause_ha_node((node + 1) as u8)
                                                .map_err(|e| format!("pause node {}: {e}", node + 1)),
                                            crate::verifier::sql::replication::HaAction::Resume(
                                                node,
                                            ) => sandbox
                                                .resume_ha_node((node + 1) as u8)
                                                .map_err(|e| format!("resume node {}: {e}", node + 1)),
                                            crate::verifier::sql::replication::HaAction::Rejoin {
                                                node,
                                                primary,
                                            } => {
                                                let alias = format!("ha{}", primary + 1);
                                                sandbox
                                                    .respawn_sql_ha_node(
                                                        workdir,
                                                        (node + 1) as u8,
                                                        ports[node],
                                                        Some((&alias, ports[primary])),
                                                        &nodes_env,
                                                    )
                                                    .map_err(|e| {
                                                        format!("rejoin node {}: {e}", node + 1)
                                                    })
                                            }
                                        };
                                            if done.is_err()
                                                && repl_control_failure_is_infra(
                                                    action,
                                                    sandbox
                                                        .ha_node_state((action.node() + 1) as u8)
                                                        .as_deref(),
                                                )
                                            {
                                                control_fault.set(true);
                                            }
                                            done
                                        };
                                    let outcome = if lite {
                                        crate::verifier::sql::replication::lite_scenario(
                                            &ports,
                                            &cfg,
                                            &mut control,
                                        )
                                    } else {
                                        crate::verifier::sql::replication::scenario(
                                            &ports,
                                            &cfg,
                                            &mut control,
                                        )
                                    };
                                    let body = outcome.and_then(|(winner, report)| {
                                        logger.log(&format!("  [REPL] {report}"));
                                        let passed: Vec<u32> = wanted
                                            .iter()
                                            .copied()
                                            .filter(|s| *s <= 13)
                                            .collect();
                                        let addr = format!("127.0.0.1:{}", ports[winner]);
                                        let seed = crate::verifier::sql::driver::stage_seed(
                                            verifier_seed,
                                            13,
                                        );
                                        // The post-failover suite must grade at
                                        // the variant each ticket passed at,
                                        // exactly like the main suite (round-16
                                        // I1): a ticket-8 lite pass must not be
                                        // asked for LEFT JOIN after failover.
                                        let variants =
                                            suite_variants(passed_lite, lite_profile, &wanted);
                                        let out = crate::verifier::sql::driver::run_stage_with_variants(
                                            &addr,
                                            std::time::Duration::from_secs(10),
                                            seed,
                                            &passed,
                                            &variants,
                                            crate::verifier::sql::driver::DEFAULT_STEPS,
                                            None,
                                            None,
                                        )?;
                                        if !out.passed {
                                            return Err(format!(
                                                "the cumulative differential suite failed on the current primary: {}",
                                                out.detail
                                            ));
                                        }
                                        logger.log(
                                            "  [REPL] the cumulative differential suite passes against the current primary",
                                        );
                                        Ok(())
                                    });
                                    if body.is_err() {
                                        for i in 0..node_count {
                                            let name =
                                                sandbox.ha_node_container_name((i + 1) as u8);
                                            if let Ok(out) = std::process::Command::new("docker")
                                                .args(["logs", &name])
                                                .output()
                                            {
                                                let mut text = String::from_utf8_lossy(&out.stdout)
                                                    .to_string();
                                                text.push_str(&String::from_utf8_lossy(
                                                    &out.stderr,
                                                ));
                                                logger.log(&format!(
                                                    "  [REPL] node {} logs: {}",
                                                    i + 1,
                                                    output_tail(&text, 1200)
                                                ));
                                            }
                                        }
                                    }
                                    sandbox.stop_ha_nodes(node_count as u8);
                                    match body {
                                        Ok(()) => {}
                                        Err(e) if control_fault.get() => {
                                            logger.log(&format!(
                                                "  [INFRA] REPL cluster control failed: {e}"
                                            ));
                                            infra_error = true;
                                            late_failure = Some((19, format!("[repl] {e}")));
                                        }
                                        Err(e) => {
                                            logger.log(&format!("  [REPL] FAILED: {e}"));
                                            late_failure = Some((19, format!("[repl] {e}")));
                                        }
                                    }
                                }
                            }
                            // Ticket 20's PostgreSQL wire listener. The
                            // candidate is restarted on a scratch store with
                            // --pg-port and a password; the harness acts as a
                            // frontend (SSL refusal, startup, simple and
                            // extended query, SQLSTATE), then a stock psql
                            // from the local postgres image cross-checks it.
                            if late_failure.is_none() && wanted.contains(&20) {
                                late_ran.push(20);
                                let lite = if wanted.iter().copied().max() == Some(20) {
                                    lite_profile.iter().any(|p| p == "pg-simple")
                                } else {
                                    passed_lite.iter().any(|(s, p)| {
                                        *s == 20 && p.iter().any(|f| f == "pg-simple")
                                    })
                                };
                                let pg_port = port + 50;
                                let password = format!(
                                    "sdb-pg-{:x}",
                                    crate::verifier::sql::driver::stage_seed(verifier_seed, 20)
                                );
                                let scratch = sql_data.as_ref().map(|data| {
                                    data.with_file_name(format!(
                                        "{}-pg",
                                        data.file_name()
                                            .map(|n| n.to_string_lossy())
                                            .unwrap_or_default()
                                    ))
                                });
                                let mut pg_extras = extras.clone();
                                if let Some(scratch) = &scratch {
                                    let real_src = sql_data
                                        .as_ref()
                                        .map(|d| d.display().to_string())
                                        .unwrap_or_default();
                                    for b in pg_extras.binds.iter_mut() {
                                        if b.0 == real_src {
                                            b.0 = scratch.display().to_string();
                                        }
                                    }
                                }
                                pg_extras.ports.push(pg_port);
                                pg_extras.launch_args.push("--pg-port".to_string());
                                pg_extras.launch_args.push(pg_port.to_string());
                                pg_extras
                                    .env
                                    .push(("SDB_SQL_PASSWORD".to_string(), password.clone()));
                                let run = |e: &crate::sandbox::docker::CandidateRunExtras| {
                                    restart_candidate(&sandbox.instance_id, workdir, port, e, true)
                                };
                                let container = sandbox.candidate_container_name();
                                let body: Result<String, String> = (|| {
                                    if let Some(scratch) = &scratch {
                                        let _ = std::fs::remove_dir_all(scratch);
                                        std::fs::create_dir_all(scratch)
                                            .map_err(|e| format!("scratch store: {e}"))?;
                                    }
                                    run(&pg_extras)?;
                                    logger.log(&format!(
                                        "  [PG] PostgreSQL wire scenario on port {pg_port} (password set)"
                                    ));
                                    let mut cfg = if lite {
                                        crate::verifier::sql::pgwire::PgConfig::lite(Some(
                                            password.clone(),
                                        ))
                                    } else {
                                        crate::verifier::sql::pgwire::PgConfig::full(Some(
                                            password.clone(),
                                        ))
                                    };
                                    // The 23505 probe needs ticket 9's PRIMARY
                                    // KEY; a cell that never passed 9 must not be
                                    // graded on it (round-16 I2).
                                    cfg.unique_keys = wanted.contains(&9);
                                    let report = crate::verifier::sql::pgwire::scenario(
                                        &format!("127.0.0.1:{pg_port}"),
                                        &cfg,
                                    )?;
                                    // Stock psql cross-check when the local
                                    // postgres image is available.
                                    let image = std::process::Command::new("docker")
                                        .args(["image", "inspect", "postgres:16-alpine"])
                                        .output()
                                        .map(|o| o.status.success())
                                        .unwrap_or(false);
                                    if image {
                                        let out = std::process::Command::new("docker")
                                            .args([
                                                "run",
                                                "--rm",
                                                "--network",
                                                "host",
                                                "-e",
                                                &format!("PGPASSWORD={password}"),
                                                "postgres:16-alpine",
                                                "psql",
                                                "-h",
                                                "127.0.0.1",
                                                "-p",
                                                &pg_port.to_string(),
                                                "-U",
                                                "sdb",
                                                "-d",
                                                "sdb",
                                                "-tAc",
                                                "SELECT 1",
                                            ])
                                            .output()
                                            .map_err(|e| format!("psql: {e}"))?;
                                        if !out.status.success() {
                                            return Err(format!(
                                                "stock psql failed: {}",
                                                output_tail(
                                                    &format!(
                                                        "{}{}",
                                                        String::from_utf8_lossy(&out.stdout),
                                                        String::from_utf8_lossy(&out.stderr)
                                                    ),
                                                    400
                                                )
                                            ));
                                        }
                                        let text =
                                            String::from_utf8_lossy(&out.stdout).trim().to_string();
                                        if text != "1" {
                                            return Err(format!(
                                                "stock psql SELECT 1 printed {text:?}"
                                            ));
                                        }
                                        logger.log("  [PG] stock psql SELECT 1 -> 1");
                                    } else {
                                        infra_error = true;
                                        return Err(
                                            "postgres:16-alpine is required for the stock psql cross-check"
                                                .to_string(),
                                        );
                                    }
                                    let out = std::process::Command::new("docker")
                                        .args(["logs", &container])
                                        .output()
                                        .map_err(|e| format!("docker logs: {e}"))?;
                                    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                                    if text.contains(&password) {
                                        return Err(
                                            "the password appeared in the candidate's logs"
                                                .to_string(),
                                        );
                                    }
                                    Ok(report)
                                })(
                                );
                                let restored = run(&extras);
                                if let Some(scratch) = &scratch {
                                    let _ = std::fs::remove_dir_all(scratch);
                                }
                                match body {
                                    Ok(report) => logger.log(&format!("  [PG] {report}")),
                                    Err(e) => {
                                        logger.log(&format!("  [PG] FAILED: {e}"));
                                        late_failure = Some((20, format!("[pg] {e}")));
                                    }
                                }
                                if let Err(e) = restored {
                                    logger.log(&format!(
                                        "  [PG] FAILED: back on the real store: {e}"
                                    ));
                                    if late_failure.is_none() {
                                        late_failure = Some((20, format!("[pg] restart: {e}")));
                                    }
                                }
                            }
                        }
                        // L1/L-d: a ticket whose late check never ran (a
                        // differential stage or a late check failed) must
                        // not keep its placeholder PASS. Scenario-only
                        // stages and the late-checked differential stages 5,
                        // 6 and 11 are all in scope.
                        // A failed late check counts too: it is applied to
                        // `stages` only below (round-18 N5).
                        let failed_stage = stages
                            .iter()
                            .find(|s| !s.passed)
                            .map(|s| s.stage)
                            .or(late_failure.as_ref().map(|(n, _)| *n));
                        for st in stages.iter_mut().filter(|s| {
                            (crate::verifier::sql::driver::scenario_only(s.stage)
                                || matches!(s.stage, 5 | 6 | 11))
                                && s.passed
                                && !late_ran.contains(&s.stage)
                        }) {
                            st.passed = false;
                            st.error = Some(match failed_stage {
                                Some(n) => {
                                    format!("not run: late checks skipped after stage {n} failed")
                                }
                                None => "not run: late checks skipped".to_string(),
                            });
                            log_stage(logger, st);
                        }
                        if let Some((lost, why)) = late_failure {
                            // A late check failed: the stage that owns the
                            // broken contract goes red, tagged with what
                            // broke (upgrade drift, crash safety, or
                            // transactions) so a report can group it.
                            let reason = if why.starts_with('[') {
                                why
                            } else if why.starts_with("drift:") {
                                format!("[upgrade] {why}")
                            } else {
                                format!("[late] {why}")
                            };
                            if let Some(st) = stages.iter_mut().find(|s| s.stage == lost) {
                                st.passed = false;
                                st.error = Some(reason.clone());
                            } else if let Some(st) = stages.first_mut() {
                                st.passed = false;
                                st.error = Some(format!("{reason} (data from stage {lost})"));
                            }
                            logger.log(&format!(
                                "  Stage {lost}: {} -> FAIL {reason}",
                                crate::config::stage_label(
                                    lost,
                                    &format!(
                                        "Stage {lost}: {}",
                                        crate::verifier::sql::driver::stage_title(lost)
                                    )
                                )
                            ));
                        } else if stages.iter().all(|s| s.passed) {
                            // A passing suite that includes ticket 4 leaves
                            // a sentinel for later tickets (its own seed
                            // keeps namespaces apart from the differential
                            // script), over the grammar it was graded on:
                            // never a ticket that failed or was skipped
                            // (review round 14, G1).
                            let top = wanted.iter().copied().max().unwrap_or(0);
                            let variants = suite_variants(passed_lite, lite_profile, &wanted);
                            if let Some(profile) =
                                crate::verifier::sql::driver::sentinel_profile(&wanted, &variants)
                            {
                                if let Some(dir) = &sentinel_dir {
                                    let seed = crate::verifier::sql::driver::stage_seed(
                                        verifier_seed,
                                        top,
                                    ) ^ 0x5E27_1E10_0000_0004;
                                    let addr = format!("127.0.0.1:{port}");
                                    let built =
                                        crate::verifier::sql::sentinel::build(seed, top, &profile)
                                            .and_then(|s| {
                                                crate::verifier::sql::sentinel::write(
                                                    &addr,
                                                    std::time::Duration::from_secs(10),
                                                    &s,
                                                )
                                                .map(|()| s)
                                            });
                                    match built {
                                        Ok(s) => {
                                            let path = dir.join(format!("sentinel-{top}.json"));
                                            match crate::verifier::sql::sentinel::save(&path, &s) {
                                                Ok(()) => {
                                                    logger.log(&format!(
                                                        "  [UPGRADE] sentinel for stage {top} \
                                                         written: {} writes, {} reads (seed {})",
                                                        s.writes.len(),
                                                        s.queries.len(),
                                                        s.seed
                                                    ));
                                                    // A candidate may flush on
                                                    // SIGTERM (ticket 4's
                                                    // contract) and the sandbox
                                                    // tear-down is `rm -f`: give
                                                    // it a clean stop now that
                                                    // this store is the one the
                                                    // next ticket reads.
                                                    let sb =
                                                        crate::sandbox::docker::SandboxManager::with_id(
                                                            &sandbox.instance_id,
                                                        );
                                                    if let Err(e) = sb.stop_candidate_gracefully(5)
                                                    {
                                                        logger.log(&format!(
                                                            "[WARN] graceful flush after the \
                                                             sentinel failed: {e}"
                                                        ));
                                                    }
                                                }
                                                Err(e) => logger.log(&format!(
                                                    "[WARN] sentinel not saved: {e}"
                                                )),
                                            }
                                        }
                                        Err(e) => logger.log(&format!(
                                            "[WARN] sentinel for stage {top} not written: {e}"
                                        )),
                                    }
                                }
                            }
                        }
                        let ps = stages.iter().filter(|s| s.passed).count() as u32;
                        let ts = stages.len() as u32;
                        let pr = if ts == 0 {
                            0.0
                        } else {
                            ps as f64 / ts as f64 * 100.0
                        };
                        (pr, ps, ts, stages)
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
                grading_wait_s,
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
        if let Some(md) = config
            .campaign_stage
            .as_ref()
            .and_then(|c| c.spec_md.as_deref())
        {
            fs::write(config.workdir.join("SPEC.md"), md)?;
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
            TaskType::Sql => {
                if let Err(e) = sandbox
                    .start_reference_sql(crate::web::server::worker_port(5481, config.worker))
                {
                    logger.log(&format!(
                        "[SETUP] Warning: reference SQL unavailable: {}",
                        e
                    ));
                }
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
        // Round-10 D2: the harness note (failed/skipped predecessors, the
        // rollback, the effective acceptance suite) leads the prompt so it
        // cannot be missed; the templating above only touches ports.
        let prompt_content = match config
            .campaign_stage
            .as_ref()
            .and_then(|c| c.preamble.as_deref())
        {
            Some(note) => format!("{note}\n\n{prompt_content}"),
            None => prompt_content,
        };
        let budget_note = session_budget_line(config.budget_usd);
        let prompt_content = format!(
            "{}\n\n{}\n\n### READY FOR VERIFICATION SIGNAL\nWhen you believe your implementation is complete and properly running on the requested port, signal that you are ready for evaluation by creating `/workspace/READY` (for example, run `touch READY` or `touch /workspace/READY`), or simply conclude your session. The benchmark framework will immediately detect this and run the automated verification suite!\n",
            prompt_content, budget_note
        );

        // Clean any stale signal file from previous runs
        let _ = fs::remove_file(config.workdir.join("READY"));

        // 5. Run agent harness (omp | opencode) unless eval_only.
        let htag = match config.harness {
            Harness::Omp => "OMP",
            Harness::Opencode => "OPENCODE",
        };
        let mut agent_failed = false;
        // Round-13 R1: a harness fault (the agent never reached its first
        // turn) is never a model verdict and its round is not graded.
        let mut agent_never_ran = false;
        // Round-14 N2/N3: why this run is not a measurement at all (the
        // agent never started, or the provider rejected the key/billing).
        // A campaign stops on it and the cell never publishes.
        let mut harness_fault: Option<String> = None;
        // Campaign rounds (§5.1 of gui-spec.md): one record per
        // verification, the initial try and every repair round.
        let mut campaign_rounds: Vec<crate::report::archive::CampaignRound> = Vec::new();
        let mut round_started = std::time::Instant::now();
        // R23 D3: the attempt's remaining provider API dollars after the
        // cell's spend and largest-call reserve. None outside capped
        // campaigns. The compute floor is reported only, never enforced.
        let cap_headroom = config
            .campaign_stage
            .as_ref()
            .and_then(|c| c.cap_headroom_usd);
        let (mut omp_stats, mut prompt_tokens, mut cached_tokens, mut completion_tokens) =
            if !config.eval_only {
                logger.log(&format!(
                "[{htag}] Spawning {htag} agent inside isolated Docker sandbox with model '{}' (Effort: {})...",
                config.model, config.effort
            ));

                // R26 R23-7: the first session gets the whole turn guard;
                // dollars, not a 50-turn checkpoint, decide when it stops.
                let initial_turn_limit = crate::scoring::initial_turn_limit(config.max_turns);

                let limits = AgentExecutionLimits {
                    max_turns: initial_turn_limit,
                    max_budget_usd: if config.budget_usd > 0.0 {
                        Some(config.budget_usd)
                    } else {
                        None
                    },
                    max_effective_usd: None,
                    timeout_seconds: if config.timeout_min > 0 {
                        Some(config.timeout_min * 60)
                    } else {
                        None
                    },
                };

                // Round-13 R1: an agent that exits before its first turn
                // measured nothing. That is a harness fault, never a model
                // verdict: retry once, and if it never runs, record a fault
                // and grade nothing. Before this, `omp -p <note>` read the
                // note as a flag, so every post-failure attempt "graded" the
                // previous ticket's code with 0 turns.
                let mut attempt_no = 0u32;
                let mut acc = OmpSessionStats::default();
                let mut ran = false;
                loop {
                    attempt_no += 1;
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
                            acc.prompt_tokens += stats.prompt_tokens;
                            acc.cached_tokens += stats.cached_tokens;
                            acc.completion_tokens += stats.completion_tokens;
                            acc.cache_write_tokens += stats.cache_write_tokens;
                            acc.total_tokens += stats.total_tokens;
                            acc.steps_taken += stats.steps_taken;
                            acc.cost_usd += stats.cost_usd;
                            acc.max_call_usd = acc.max_call_usd.max(stats.max_call_usd);
                            acc.effort_effective = stats.effort_effective.clone();
                            acc.stop_reason = stats.stop_reason.clone();
                            ran = true;
                            break;
                        }
                        Err(e) => {
                            if let Some(ref cf) = cancel_flag {
                                if cf.load(Ordering::SeqCst) {
                                    sandbox.cleanup();
                                    return Err(anyhow!("Benchmark run was cancelled by user"));
                                }
                            }
                            let end = agent_error_end(&e);
                            acc.prompt_tokens += end.partial.prompt_tokens;
                            acc.cached_tokens += end.partial.cached_tokens;
                            acc.completion_tokens += end.partial.completion_tokens;
                            acc.cache_write_tokens += end.partial.cache_write_tokens;
                            acc.total_tokens += end.partial.total_tokens;
                            acc.steps_taken += end.partial.steps_taken;
                            acc.cost_usd += end.partial.cost_usd;
                            acc.max_call_usd = acc.max_call_usd.max(end.partial.max_call_usd);
                            acc.stop_reason = end.partial.stop_reason.clone();
                            logger.log(&format!("[{htag} ERROR] Agent execution failed: {}", e));
                            if let Some(reason) = end.billing {
                                // Round-14 N3: a dead key or exhausted
                                // credit fails every retry the same way;
                                // never retry it, and stop the cell.
                                logger.log(&format!(
                                    "[{htag} FAULT] provider billing/auth: {reason}; not retried, and the cell stops"
                                ));
                                harness_fault = Some(format!("provider billing/auth: {reason}"));
                                if end.started {
                                    agent_failed = true;
                                } else {
                                    agent_never_ran = true;
                                }
                                break;
                            }
                            if !end.started && attempt_no < 2 {
                                logger.log(&format!(
                                    "[{htag} RETRY] the agent exited before its first turn; retrying the round once"
                                ));
                                continue;
                            }
                            if !end.started {
                                logger.log(&format!(
                                    "[{htag} FAULT] the agent never reached its first turn (twice): a harness fault, and this round is not graded"
                                ));
                                agent_never_ran = true;
                                harness_fault = Some(format!(
                                    "the agent never reached its first turn (twice): {}",
                                    crate::campaign::truncate_line(&e.to_string(), 160)
                                ));
                            } else {
                                agent_failed = true;
                            }
                            break;
                        }
                    }
                }
                debug_assert!(ran || agent_never_ran || agent_failed);
                let stats = acc;
                let p = stats.prompt_tokens;
                let c = stats.cached_tokens;
                let comp = stats.completion_tokens;
                (stats, p, c, comp)
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
        // References are only for the agent and the bench: detach them
        // while verifying so a candidate cannot proxy to `ref-*`.
        sandbox.detach_references();
        let mut grading_wait_s = 0.0f64;
        let mut v_outcome = if agent_never_ran {
            // Round-13 R1: nothing the agent touched exists to grade; a
            // fault outcome carries no stage results.
            logger.log(&format!(
                "[{htag} FAULT] verification skipped: the agent never ran, so there is nothing to grade"
            ));
            VerificationOutcome {
                pass_rate: 0.0,
                passed_stages: 0,
                total_stages: 0,
                stage_results: Vec::new(),
                extra_robustness: Vec::new(),
                extra_throughput: None,
                candidate_logs: String::new(),
                disqualified: false,
                disqualification_reason: None,
                infra_error: true,
                grading_wait_s: 0.0,
            }
        } else {
            Self::run_verification(
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
                config
                    .campaign_stage
                    .as_ref()
                    .map(|c| c.lite_profile.as_slice())
                    .unwrap_or(&[]),
                config
                    .campaign_stage
                    .as_ref()
                    .map(|c| c.passed_lite.as_slice())
                    .unwrap_or(&[]),
            )
            .await
        };
        grading_wait_s += v_outcome.grading_wait_s;
        campaign_rounds.push(crate::report::archive::CampaignRound {
            round: 0,
            passed_stages: v_outcome.passed_stages,
            total_stages: v_outcome.total_stages,
            pass_rate: v_outcome.pass_rate,
            turns: Some(omp_stats.steps_taken),
            cost_usd: crate::cost::calculator::ModelPricing::compute_cost(
                round_started.elapsed().as_secs_f64(),
                prompt_tokens + cached_tokens + completion_tokens,
                cached_tokens,
            ),
            duration_seconds: Some(round_started.elapsed().as_secs_f64()),
            stage_results: v_outcome.stage_results.clone(),
            candidate_logs_tail: Some(tail_lines(&v_outcome.candidate_logs, 20)),
            stop_reason: omp_stats.stop_reason.clone(),
        });
        // The repair agent needs the references again (it probes exact
        // output formats while fixing); the next verification detaches.
        sandbox.reattach_references();

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

                    // Cap/budget spend guard FIRST (round-17 P0): a repair
                    // gets only what the attempt's API budget has left, and
                    // must not start when either floor is gone. Break before
                    // stopping the candidate or writing FEEDBACK_REPORT.md.
                    let spent_api = omp_stats.cost_usd;
                    let attempt_budget = if config.budget_usd > 0.0 {
                        config.budget_usd
                    } else {
                        f64::INFINITY
                    };
                    let (repair_budget, _repair_cap) = match crate::campaign::repair_round_spend(
                        attempt_budget,
                        spent_api,
                        cap_headroom,
                    ) {
                        Ok(v) => v,
                        Err(reason) => {
                            logger.log(&format!(
                                "[LIMIT] Repair round {round} not started: {reason}"
                            ));
                            break;
                        }
                    };

                    // Pool check FIRST: an exhausted pool must break before
                    // stopping the candidate, writing FEEDBACK_REPORT.md, or
                    // recording a round that never runs. (Previously all three
                    // happened first, so the manifest recorded a phantom round.)
                    let global_max = config.max_turns;
                    let turns_remaining = match crate::scoring::repair_turn_allowance(
                        global_max,
                        omp_stats.steps_taken,
                    ) {
                        crate::scoring::TurnAllowance::Exhausted => {
                            logger.log(&format!(
                            "[LIMIT] Global turn budget ({} turns) reached across attempts. Concluding run.",
                            global_max
                        ));
                            break;
                        }
                        crate::scoring::TurnAllowance::Uncapped => None,
                        crate::scoring::TurnAllowance::Capped(n) => Some(n),
                    };
                    feedback_rounds_used = round;
                    round_started = std::time::Instant::now();
                    let round_tokens_before = (prompt_tokens, cached_tokens, completion_tokens);
                    let mut round_steps: Option<u32> = None;
                    // R26 R23-7: this repair session's own stop reason.
                    let mut round_stop_reason: Option<String>;

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
                        max_budget_usd: repair_budget,
                        max_effective_usd: None,
                        timeout_seconds: if config.timeout_min > 0 {
                            Some(config.timeout_min * 60)
                        } else {
                            None
                        },
                    };

                    let round_run_id = format!("{}_r{}", run_id, round);
                    // Round-14 N3: a repair agent that never starts is
                    // retried once like round 0; a billing trip or a second
                    // dead start ends the repair loop (nothing new to grade)
                    // and stops the cell.
                    let mut round_attempt = 0u32;
                    let mut round_dead = false;
                    loop {
                        round_attempt += 1;
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
                                omp_stats.cost_usd += r_stats.cost_usd;
                                omp_stats.max_call_usd =
                                    omp_stats.max_call_usd.max(r_stats.max_call_usd);
                                omp_stats.stop_reason = r_stats.stop_reason.clone();
                                round_steps = Some(r_stats.steps_taken);
                                round_stop_reason = r_stats.stop_reason.clone();
                                logger.log(&format!(
                                    "[FEEDBACK] Round {} agent completed! Steps: {}, Round tokens: prompt={}, completion={}",
                                    round, r_stats.steps_taken, r_stats.prompt_tokens, r_stats.completion_tokens
                                ));
                                break;
                            }
                            Err(e) => {
                                logger.log(&format!(
                                    "[FEEDBACK ERROR] Round {} agent failed: {}",
                                    round, e
                                ));
                                // Count partial work against the pool even on
                                // failure, mirroring the main agent path: a
                                // failed round still burned turns and tokens.
                                let end = agent_error_end(&e);
                                prompt_tokens += end.partial.prompt_tokens;
                                cached_tokens += end.partial.cached_tokens;
                                completion_tokens += end.partial.completion_tokens;
                                omp_stats.steps_taken += end.partial.steps_taken;
                                omp_stats.total_tokens += end.partial.total_tokens;
                                omp_stats.cost_usd += end.partial.cost_usd;
                                omp_stats.max_call_usd =
                                    omp_stats.max_call_usd.max(end.partial.max_call_usd);
                                omp_stats.stop_reason = end.partial.stop_reason.clone();
                                round_stop_reason = end.partial.stop_reason.clone();
                                if let Some(ref cf) = cancel_flag {
                                    if cf.load(Ordering::SeqCst) {
                                        break;
                                    }
                                }
                                if let Some(reason) = end.billing {
                                    logger.log(&format!(
                                        "[FEEDBACK FAULT] provider billing/auth: {reason}; not retried, and the cell stops"
                                    ));
                                    harness_fault =
                                        Some(format!("provider billing/auth: {reason}"));
                                    round_dead = true;
                                    break;
                                }
                                if !end.started && round_attempt < 2 {
                                    logger.log(&format!(
                                        "[FEEDBACK RETRY] Round {round} agent exited before its first turn; retrying once"
                                    ));
                                    continue;
                                }
                                if !end.started {
                                    logger.log(&format!(
                                        "[FEEDBACK FAULT] Round {round} agent never reached its first turn (twice): a harness fault; the round is not graded"
                                    ));
                                    harness_fault = Some(format!(
                                        "repair round {round}: the agent never reached its first turn (twice): {}",
                                        crate::campaign::truncate_line(&e.to_string(), 160)
                                    ));
                                    round_dead = true;
                                }
                                break;
                            }
                        }
                    }
                    if round_dead {
                        // Not recorded as a round: it never ran, so it
                        // measured nothing; the previous verdict stands.
                        feedback_rounds_used = round - 1;
                        break;
                    }

                    if let Some(ref cf) = cancel_flag {
                        if cf.load(Ordering::SeqCst) {
                            sandbox.cleanup();
                            return Err(anyhow!("Benchmark run was cancelled by user"));
                        }
                    }

                    // 4. Re-verify the updated candidate
                    let _ = fs::remove_file(config.workdir.join("READY"));
                    sandbox.detach_references();
                    // Fresh seed per verification (docs/PROPOSAL-sdb-sql.md):
                    // re-running the same seeded names against a candidate
                    // that persisted them fails at statement 1 with `exists`
                    // before anything is graded (live cell 2026-10-16).
                    // sdb-sql only: every other task keeps re-verifying on
                    // the run seed, so its grading (and RULES stamp) is
                    // unchanged.
                    let repair_seed = if matches!(config.task, TaskType::Sql) {
                        format!("{verifier_seed}-repair{round}")
                    } else {
                        verifier_seed.clone()
                    };
                    let next_outcome = Self::run_verification(
                        config.task,
                        &config.workdir,
                        &sandbox,
                        port,
                        &repair_seed,
                        false,
                        config.worker,
                        &logger,
                        config
                            .campaign_stage
                            .as_ref()
                            .map(|c| c.accepted_stages.as_slice()),
                        config
                            .campaign_stage
                            .as_ref()
                            .map(|c| c.lite_profile.as_slice())
                            .unwrap_or(&[]),
                        config
                            .campaign_stage
                            .as_ref()
                            .map(|c| c.passed_lite.as_slice())
                            .unwrap_or(&[]),
                    )
                    .await;

                    logger.log(&format!(
                        "[FEEDBACK] Round {} test result: {} of {} stages passed ({:.1}%)",
                        round,
                        next_outcome.passed_stages,
                        next_outcome.total_stages,
                        next_outcome.pass_rate
                    ));
                    campaign_rounds.push(crate::report::archive::CampaignRound {
                        round,
                        passed_stages: next_outcome.passed_stages,
                        total_stages: next_outcome.total_stages,
                        pass_rate: next_outcome.pass_rate,
                        turns: round_steps,
                        cost_usd: crate::cost::calculator::ModelPricing::compute_cost(
                            round_started.elapsed().as_secs_f64(),
                            (prompt_tokens + cached_tokens + completion_tokens).saturating_sub(
                                round_tokens_before.0
                                    + round_tokens_before.1
                                    + round_tokens_before.2,
                            ),
                            cached_tokens.saturating_sub(round_tokens_before.1),
                        ),
                        duration_seconds: Some(round_started.elapsed().as_secs_f64()),
                        stage_results: next_outcome.stage_results.clone(),
                        candidate_logs_tail: Some(tail_lines(&next_outcome.candidate_logs, 20)),
                        stop_reason: round_stop_reason.clone(),
                    });

                    if next_outcome.pass_rate > best_pass_rate {
                        best_pass_rate = next_outcome.pass_rate;
                    }

                    sandbox.reattach_references();
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
                        grading_wait_s += v_outcome.grading_wait_s;
                    }
                }
            }
        }

        logger.log(&format!(
            "[COST] metered ${:.4}, largest call ${:.4}",
            omp_stats.cost_usd, omp_stats.max_call_usd
        ));

        let mut pass_rate = v_outcome.pass_rate;
        let mut passed_stages = v_outcome.passed_stages;
        let total_stages = v_outcome.total_stages;
        let mut stage_results = v_outcome.stage_results;
        let extra_robustness = v_outcome.extra_robustness;
        let disqualified = v_outcome.disqualified;
        let _disqualification_reason = v_outcome.disqualification_reason;
        let infra_error = v_outcome.infra_error;

        // R43: only sdb-sql remains; its performance is the perf score
        // (round 45), not the retired live throughput benchmark.
        let throughput: Option<f64> = None;
        let reference_throughput: Option<f64> = None;
        let baseline_source: Option<String> = None;

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
            .max(prompt_tokens + cached_tokens + completion_tokens + omp_stats.cache_write_tokens);

        let pricing = ModelPricing::for_model_async(&config.model).await;
        let breakdown = pricing.compute_cost_with_cache_and_write(
            prompt_tokens,
            cached_tokens,
            completion_tokens,
            omp_stats.cache_write_tokens,
        );

        let mut live_spend_delta = None;
        if let (Some(key), Some(init)) = (&config.api_key, initial_spend) {
            sleep(Duration::from_millis(1500)).await;
            if let Some(fin) = ModelPricing::query_openrouter_key_usage(key).await {
                if fin >= init {
                    live_spend_delta = Some(fin - init);
                }
            }
        }

        // R24 R2: the committed snapshot is the scored cost. The live key
        // delta is audit-only; a provider promo or lag cannot move a score.
        let (cost_usd, cost_source) =
            ModelPricing::resolve_api_cost(live_spend_delta, breakdown.total_cost_usd);
        if let Some(live) = live_spend_delta {
            logger.log(&format!(
                "[COST] snapshot ${:.4}; live key delta ${:.4} recorded for audit only",
                breakdown.total_cost_usd, live
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
            rounds: campaign_rounds.clone(),
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
            live_spend_delta_usd: live_spend_delta,
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
            sqlite_version: Some(crate::report::rules::sqlite_version().to_string()),
            cap_price_basis: Some(crate::report::rules::CAP_PRICE_BASIS.to_string()),
            sandbox_image: crate::sandbox::omp_updater::sandbox_image_id(),
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
            // Round-12 P1: the static portal is the sdb-sql board now, and
            // only `campaign run`, the engine's rebuild endpoint and
            // hide/poison publish it. Step 13 wrote the retired legacy page
            // into the real portal/ after every saved run — in tests it
            // wiped the served board from an isolated runs dir.
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
        let stop_reason = omp_stats.stop_reason.clone().or_else(|| {
            // A non-zero exit / billing trip can end before the runner
            // classifies its stats; the pipeline still knows it crashed.
            (agent_failed || harness_fault.is_some()).then(|| "crash".to_string())
        });
        Ok(PipelineOutput {
            manifest,
            benchmark_result,
            agent_failed,
            disqualified,
            infra_error,
            harness_fault,
            max_call_usd: omp_stats.max_call_usd,
            stop_reason,
            grading_wait_s,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-14 N3: how an agent error ended. Billing never reads as "never
    /// started" by accident, the runner's own sign-of-life flag wins over a
    /// zero step count, and an unknown error never started.
    #[test]
    fn test_agent_error_end_classifies_billing_starts_and_unknowns() {
        use crate::sandbox::omp::{OmpBillingError, OmpExecutionError, OmpSessionStats};
        let billing = anyhow::Error::new(OmpBillingError {
            reason: "provider.auth: User not found.".into(),
            partial_stats: OmpSessionStats::default(),
        });
        let end = agent_error_end(&billing);
        assert_eq!(
            end.billing.as_deref(),
            Some("provider.auth: User not found.")
        );
        assert!(!end.started);
        let billing_mid = anyhow::Error::new(OmpBillingError {
            reason: "402".into(),
            partial_stats: OmpSessionStats {
                steps_taken: 7,
                total_tokens: 9000,
                ..Default::default()
            },
        });
        let end = agent_error_end(&billing_mid);
        assert!(end.started && end.billing.is_some());
        assert_eq!(end.partial.steps_taken, 7, "partial stats are kept");
        // A lost session file: 0 steps, but the runner saw stdout.
        let lost = anyhow::Error::new(OmpExecutionError {
            message: "exit 1".into(),
            partial_stats: OmpSessionStats::default(),
            started: true,
        });
        assert!(agent_error_end(&lost).started, "graded, not retried");
        let argv = anyhow::Error::new(OmpExecutionError {
            message: "exit 2".into(),
            partial_stats: OmpSessionStats::default(),
            started: false,
        });
        let end = agent_error_end(&argv);
        assert!(!end.started && end.billing.is_none());
        assert!(!agent_error_end(&anyhow::anyhow!("Failed to spawn omp")).started);
    }

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

        fn text(&self) -> String {
            self.0.lock().unwrap().join("\n")
        }
    }

    impl PipelineLogger for RecordingLogger {
        fn log(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
    }

    /// R31 F1: a live-proof assertion failure must carry the pipeline log
    /// (the original infra_error panic did not, and the scratch dir was
    /// already gone).
    fn assert_proof_result(infra_error: bool, status: &str, logger: &RecordingLogger) {
        assert!(
            !infra_error,
            "infra_error; status {status}; log:\n{}",
            logger.text()
        );
    }

    fn assert_proof_ok(out: &PipelineOutput, logger: &RecordingLogger) {
        assert_proof_result(out.infra_error, &out.manifest.status, logger);
    }

    /// R30 F3: every needle failure carries the pipeline log.
    fn assert_logged(logger: &RecordingLogger, needle: &str) {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }

    /// R31-2/R37 F5: a failed proof execute prints the recorded log before
    /// the panic, not only the error.
    async fn expect_proof(
        fut: impl std::future::Future<Output = anyhow::Result<PipelineOutput>>,
        logger: &RecordingLogger,
        context: &str,
    ) -> PipelineOutput {
        match fut.await {
            Ok(out) => out,
            Err(e) => panic!("{context}: {e}; log:\n{}", logger.text()),
        }
    }

    #[tokio::test]
    #[should_panic(expected = "[INFRA]")]
    async fn r37_f5_proof_execute_err_prints_log() {
        let logger = RecordingLogger::default();
        logger.log("[INFRA] seeded execute failure");
        let _: PipelineOutput =
            expect_proof(async { Err(anyhow::anyhow!("boom")) }, &logger, "proof").await;
    }

    fn assert_proof_score(
        out: &PipelineOutput,
        logger: &RecordingLogger,
        passed_stages: u32,
        total_stages: u32,
    ) {
        assert_proof_ok(out, logger);
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (passed_stages, total_stages),
            "score mismatch; log:\n{}",
            logger.text()
        );
    }

    /// R48 F1: two SQL workers never share a host port. This extends
    /// round 37's table to the live worker slots 0-7: candidate 5480,
    /// reference 5481, the admin/dashboard ports and the proxy pool all
    /// move by 100 per worker.
    #[test]
    fn r48_f1_two_workers_share_no_port() {
        use crate::web::server::worker_port;
        use std::collections::HashSet;
        let ports = |w: u8| -> HashSet<u16> {
            [5480, 5481, 6395, 8090, 8101, 8102, 8103]
                .into_iter()
                .map(|base| worker_port(base, w))
                .collect()
        };
        for (a, b) in [(0u8, 1u8), (1, 2), (2, 3), (3, 4), (6, 7)] {
            assert!(
                ports(a).is_disjoint(&ports(b)),
                "workers {a} and {b} share a host port"
            );
        }
        assert_eq!(worker_port(5480, 2), 5680);
        assert_eq!(worker_port(5481, 2), 5681);
        assert_eq!(worker_port(8101, 2), 8301);
    }

    /// R37 F1: for every pipeline test, compute every host port it can
    /// bind and reject two concurrent tests sharing one. Tests serialized
    /// by SQL_PERF_LIVE may share with each other.
    #[test]
    fn r37_f1_worker_ports_do_not_collide() {
        use std::collections::{HashMap, HashSet};
        let source = include_str!("pipeline.rs");
        let mut current = String::new();
        let mut entries: Vec<(String, u8)> = Vec::new();
        let mut serialized: HashSet<String> = HashSet::new();
        for line in source.lines() {
            let line = line.trim();
            // Build the markers so scripts/live-proofs.sh's awk does not
            // mistake this source for a test holding the lock.
            let async_prefix = "async".to_string() + " fn ";
            if let Some(rest) = line
                .strip_prefix(&async_prefix)
                .or_else(|| line.strip_prefix("fn "))
            {
                current = rest.split('(').next().unwrap_or("").trim().to_string();
            }
            let lock_name = "SQL_PERF_LIVE".to_string() + ".lock";
            if line.contains(&lock_name) {
                serialized.insert(current.clone());
            }
            if let Some(rest) = line.strip_prefix("let worker = ") {
                if let Some(num) = rest.strip_suffix("u8;") {
                    entries.push((current.clone(), num.parse::<u8>().expect("worker id")));
                }
            } else if let Some(rest) = line.strip_prefix("worker: ") {
                if let Some(num) = rest.strip_suffix(',') {
                    if let Ok(id) = num.parse::<u8>() {
                        entries.push((current.clone(), id));
                    }
                }
            }
        }
        assert!(
            entries.len() >= 41,
            "worker table lost entries: {entries:?}"
        );
        let task = |_name: &str| TaskType::Sql;
        let mut owners: HashMap<u16, String> = HashMap::new();
        let mut max_port = 0u16;
        for (name, worker) in &entries {
            assert!(
                *worker >= 8,
                "{name} uses worker {worker}; workers 0..7 are live cell slots"
            );
            let task_kind = task(name);
            let base = task_kind.default_port();
            // A test binds its own task's reference, plus the shared admin,
            // dashboard, HA and proxy-pool ports. A campaign only starts the
            // reference of its own task, so the other refs are not here.
            let mut ports = vec![
                crate::web::server::worker_port(base, *worker),
                crate::web::server::worker_port(6395, *worker),
                crate::web::server::worker_port(8090, *worker),
                crate::web::server::worker_port(7001, *worker),
                crate::web::server::worker_port(7002, *worker),
                crate::web::server::worker_port(7003, *worker),
                crate::web::server::worker_port(8101, *worker),
                crate::web::server::worker_port(8102, *worker),
                crate::web::server::worker_port(8103, *worker),
            ];
            ports.push(crate::web::server::worker_port(5481, *worker));
            ports.push(crate::web::server::worker_port(base + 50, *worker));
            ports.push(crate::web::server::worker_port(base + 60, *worker));
            ports.push(crate::web::server::worker_port(base + 61, *worker));
            ports.push(crate::web::server::worker_port(base + 62, *worker));
            for port in ports {
                max_port = max_port.max(port);
                if let Some(other) = owners.get(&port) {
                    if other != name && !(serialized.contains(other) && serialized.contains(name)) {
                        panic!("{name} and {other} both bind host port {port}");
                    }
                } else {
                    owners.insert(port, name.clone());
                }
            }
        }
        assert!(
            max_port < 20_000,
            "worker ports leave the free_port range: {max_port}"
        );
    }

    #[test]
    #[should_panic(expected = "[INFRA]")]
    fn r31_f1_proof_assert_prints_log() {
        let logger = RecordingLogger::default();
        logger.log("[INFRA] seeded failure");
        assert_proof_result(true, "infra_error", &logger);
    }

    /// A test's temp dir, removed when the guard drops — on success and on
    /// panic alike (the suite used to leave hundreds of `/tmp/covtest-*`
    /// dirs behind, round-3 review finding 9). Derefs to the path, so
    /// `dir.join(..)` and `&dir` keep working.
    struct TestDir(PathBuf);
    impl std::ops::Deref for TestDir {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }
    impl AsRef<Path> for TestDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Temp fixture root unique to this process and test tag.
    fn covtest_dir(tag: &str) -> TestDir {
        // The host's /tmp is a tmpfs. The live memory/disk proofs keep
        // multi-hundred-MB stores under this directory and cap the
        // candidate at 64/192 MiB; on tmpfs those files are unreclaimable
        // shmem, so the cap would test the host's scratch store instead of
        // the candidate. Keep this test scratch on a real disk, matching
        // the production campaign directories. SDB_COVTEST_ROOT overrides
        // it on CI hosts where /var/tmp is small.
        let root = std::env::var_os("SDB_COVTEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/var/tmp"));
        let dir = root.join(format!("covtest-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&dir);
        TestDir(dir)
    }

    /// Minimal RESP candidate: raw sockets + a thread per connection, so
    /// PING/ECHO/SET/GET/DEL/EXISTS pass stages 1-2 (and the advisory
    /// robustness probes), while the deliberately missing TTL/INCR/PUBSUB
    /// commands keep stages 3-5 red. That keeps pass_rate < 100%, so the
    /// pipeline skips the bench while still building the full manifest.

    /// Stage-2 live fixture: RESP on `argv[1]` + SDB1 v2 admin on 6395,
    /// one shared store whose values live in an append log under
    /// SDB_DATA_DIR (offset index in memory), idle-gated compaction to
    /// <= 3x live bytes, SIGKILL-safe (flush per write, partial tail
    /// dropped on replay). Implements exactly what stages 14-16 grade.

    /// Write the stage-2 mock candidate into `dir`, RESP on `port`.

    /// Proxy-campaign live fixture: a minimal reverse proxy that forwards
    /// `GET /` to `upstream1:80` (the run-network nginx pool) with valid
    /// HTTP/1.1 framing. Clears stage 1 and exercises the sdb-edge path.

    /// Write the proxy mock into `dir`, listening on `port`.

    /// sdb-edge reference candidate (live ladder proof): a stdlib-only
    /// reverse proxy with round-robin over the three upstream aliases,
    /// upstream timeouts + retry (hung-upstream failover), active health
    /// probes and `GET /status`, a TTL cache (`SDB_PROXY_CACHE_TTL`,
    /// `X-Cache`/`Age`), and request limits (8KiB head -> 431, >1MiB
    /// declared body -> 413, slow-loris reclaimed in ~3s). Env override
    /// `SDB_EDGE_UPSTREAMS` points it at local stubs for development.

    /// Write the sdb-edge reference proxy into `dir`, listening on `port`.

    /// sdb-queue reference broker (live ladder proof): stdlib-only job
    /// queue with connection-owned visibility leases, nacks/delays,
    /// dead-lettering after 3 attempts (inspection reads do not count),
    /// redrive, per-queue STATS, delayed enqueue, and an append-only
    /// fsync-before-ack log under `./data` (torn tails dropped on
    /// replay). Env override `SDB_QUEUE_DATA` points it elsewhere for
    /// local development.

    /// Write the sdb-queue reference broker into `dir`, listening on `port`.

    /// sdb-http reference file server (live ladder proof): stdlib-only
    /// HTTP/1.1 server with keep-alive, echo/user-agent endpoints, a
    /// file store under `./data/files`, byte ranges (incl. suffix/open
    /// and 416), ETag/Last-Modified validators with strict conditional
    /// semantics, gzip negotiation, chunked + `Expect: 100-continue`
    /// uploads with a 16MiB declared-body cap, a 8KiB head cap, path
    /// traversal rejection and a ~3s stalled-head reclaim. Env override
    /// `SDB_HTTP_BASE` points the store elsewhere for local development.

    /// A deliberately broken sdb-sql candidate (review finding 2's fixture):
    /// it speaks the protocol and answers every statement `OK 0`, so the
    /// first setup INSERT mismatches on its affected count. Every start
    /// drops a marker file into its `--data` directory, so a test can tell
    /// which store each instance wrote to. Stateless, so a replay from a
    /// fresh store reproduces the failure exactly.
    const ALWAYS_OK_SQL_PY: &str = r#"import os, socket, sys, threading, time


def serve(conn):
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    f = conn.makefile("rwb")
    try:
        for _ in f:
            f.write(b"OK 0\n")
            f.flush()
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def main():
    port = 5480
    data = "/tmp"
    args = sys.argv[1:]
    for i, a in enumerate(args):
        if a == "--port" and i + 1 < len(args):
            port = int(args[i + 1])
        elif a == "--data" and i + 1 < len(args):
            data = args[i + 1]
    try:
        os.makedirs(data, exist_ok=True)
        with open(os.path.join(data, "marker-%d.txt" % time.time_ns()), "w") as f:
            f.write(str(os.getpid()))
    except OSError:
        pass
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(64)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


main()
"#;

    /// Write the always-OK sdb-sql mock into `dir`, listening on `port`.
    fn write_always_ok_sql_mock(dir: &Path, port: u16) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("server.py"), ALWAYS_OK_SQL_PY).unwrap();
        let start = format!(
            "#!/bin/bash\nif [ $# -eq 0 ]; then set -- --port {port} --data /tmp; fi\n\
             exec python3 -u /workspace/server.py \"$@\"\n"
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
            task: TaskType::Sql,
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

    /// Ticket 15's gate end-to-end against a real fixture (Docker + the
    /// sandbox image): a repo with stage-6/7/8 commits, a green offline
    /// `test.sh` and a coverage line passes (both reverts go red); a
    /// toothless suite fails; the lite gate accepts 65% where the full gate
    /// does not.
    #[test]
    fn test_tests_gate_sensitivity_end_to_end() {
        // The perf-gate live proofs measure ratios; keep the extra Docker
        // load out of their windows (same serialization as the SQL proofs).
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("s15-ci-test-{}", std::process::id()));
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
        std::fs::write(good.join("engine.py"), "BASE = True\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 6: Transactions [pass]"]);
        std::fs::write(good.join("engine.py"), "BASE = True\nAGG = True\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 7: Aggregates [pass]"]);
        std::fs::write(
            good.join("engine.py"),
            "BASE = True\nAGG = True\nJOIN = True\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 8: Joins [pass]"]);
        std::fs::write(good.join("coverage.txt"), "coverage: 85.0%\n").unwrap();
        let good_suite = "#!/bin/sh\nset -e\ngrep -q '^AGG = True' engine.py\ngrep -q '^JOIN = True' engine.py\n";
        std::fs::write(good.join("test.sh"), good_suite).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                good.join("test.sh"),
                std::fs::Permissions::from_mode(0o755),
            );
        }

        let st = run_tests_gate_blocking(
            SandboxManager::with_id(&format!("s15-good-{}", std::process::id())),
            good.clone(),
            Arc::new(DummyLogger),
            false,
        );
        assert!(st.passed, "good fixture must pass: {:?}", st.error);

        // Toothless: always green, so neither revert can fail it.
        std::fs::write(good.join("test.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        let st = run_tests_gate_blocking(
            SandboxManager::with_id(&format!("s15-bad-{}", std::process::id())),
            good.clone(),
            Arc::new(DummyLogger),
            false,
        );
        assert_eq!(st.stage, 15);
        assert!(!st.passed, "toothless suite must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("stayed green"),
            "expected 'stayed green': {:?}",
            st.error
        );

        // Coverage: 65% fails the full 80% gate but passes the lite 60% one.
        std::fs::write(good.join("test.sh"), good_suite).unwrap();
        std::fs::write(good.join("coverage.txt"), "coverage: 65.0%\n").unwrap();
        let st = run_tests_gate_blocking(
            SandboxManager::with_id(&format!("s15-cov-{}", std::process::id())),
            good.clone(),
            Arc::new(DummyLogger),
            false,
        );
        assert!(!st.passed, "65% must fail the 80% gate");
        assert!(
            st.error.as_deref().unwrap_or("").contains("below the 80%"),
            "expected the 80% verdict: {:?}",
            st.error
        );
        let st = run_tests_gate_blocking(
            SandboxManager::with_id(&format!("s15-lite-{}", std::process::id())),
            good.clone(),
            Arc::new(DummyLogger),
            true,
        );
        assert!(st.passed, "65% must pass the lite 60% gate: {:?}", st.error);
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

    /// R23 D5: the ticket prompt carries the dollars and says turns are
    /// not the limit.
    #[test]
    fn r23_d5_session_budget_line_has_the_dollars_and_no_turn_limit() {
        let line = session_budget_line(0.25);
        assert!(line.contains("$0.25"), "{line}");
        assert!(line.contains("Turns are not the limit"), "{line}");
        let uncapped = session_budget_line(0.0);
        assert!(uncapped.contains("Turns are not the limit"), "{uncapped}");
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
            task: TaskType::Sql,
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
            worker: 14,
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
            task: TaskType::Sql,
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
            // Dedicated worker 114: this test only asserts the archive path;
            // workers 0..7 belong to live campaign cells on this host, so
            // keep test traffic on its own offset.
            worker: 114,
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
            TaskType::Sql,
            1,
            2,
            &stages,
            "server started on port 5480\ncommand error: SELECT foo",
        );
        assert!(prompt.contains("# Automated Verification Failure Report"));
        assert!(prompt.contains("sql"));
        assert!(prompt.contains("passed 1 of 2 stages (50.0%)"));
        assert!(prompt.contains("[PASS] Stage 1: PING / PONG"));
        assert!(prompt.contains("[FAIL] Stage 2: SET / GET"));
        assert!(prompt.contains("Diagnostic details: expected +OK, got -ERR"));
        assert!(prompt.contains("server started on port 6379"));
        assert!(prompt.contains("Directives for Surgical Bug Fixing:"));
        assert!(prompt.contains("DO NOT wipe your workspace"));
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
    fn test_format_feedback_prompt_zero_stages_and_tail_limit() {
        // Zero totals: the percentage guard must not divide by zero.
        let p = BenchmarkPipeline::format_feedback_prompt(TaskType::Sql, 0, 0, &[], "");
        assert!(p.contains("passed 0 of 0 stages (0.0%)"));
        assert!(!p.contains("Service Container Runtime Output"));
        // Container logs are tail-capped at 60 lines: older lines drop off.
        let logs: Vec<String> = (0..80).map(|i| format!("line {i:02}")).collect();
        let p =
            BenchmarkPipeline::format_feedback_prompt(TaskType::Sql, 1, 2, &[], &logs.join("\n"));
        assert!(p.contains("line 79"));
        assert!(p.contains("line 20"));
        assert!(!p.contains("line 19"));
    }

    /// Full REAL pipeline drive with no model: eval-only Redis on a mock
    /// RESP candidate that clears stages 1-2 and deliberately fails 3-5.
    /// Asserts the verdict/manifest shape of a partially-passing trial and
    /// that the bench is skipped below 100%.

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
        assert_proof_ok(&out, &logger);
        assert!(!out.disqualified && !out.agent_failed);
        assert_eq!(out.manifest.status, "failed_tests");
        assert_eq!(out.manifest.pass_rate, 0.0);
        assert_proof_score(&out, &logger, 0, 6);
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
        assert_proof_ok(&out, &logger);
        assert_eq!(out.manifest.status, "disqualified_cheat");
        assert_eq!(out.manifest.pass_rate, 0.0);
        assert_proof_score(&out, &logger, 0, 6);
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

    /// Runnable image, unusable entrypoint: `docker run` fails with
    /// "no such file or directory", which is the model's packaging fault
    /// (not infra). Driven through `run_verification` directly.

    /// A cancel that lands while verification is running must surface as an
    /// Err after the sandbox is cleaned up, never as a scored trial.

    /// Campaign ticket with a partial acceptance subset: only stages 1-3
    /// count toward the verdict, the destructive AOF stage is skipped, and
    /// the manifest records the subset totals.

    /// Stage-2 wave live proof (eval-only, no model): a disk-resident mock
    /// candidate driven through the REAL pipeline for stages 14-16 —
    /// host-bind restart under the 192MB cap, SIGKILL re-reads, the
    /// footprint gate, and the SDB1 v2 admin channel end to end.

    /// sdb-edge live smoke (eval-only, no model): a minimal forwarding
    /// proxy + the run-network nginx pool, driven through the real pipeline
    /// with acceptance [1]. Validates the proxy campaign path: task
    /// adapter, upstream pool startup, acceptance-scoped verdict and the
    /// stage-log contract.

    /// sdb-edge FULL-ladder live proof (eval-only, no model): the
    /// reference proxy (routing, retry, health surface, cache, limits)
    /// driven through the real pipeline with acceptance [1,2,3,4,6,7,8,9]
    /// — includes the docker-pause failover stages, the cache restart with
    /// SDB_PROXY_CACHE_TTL, and the 128MB-capped limits stage.

    /// sdb-queue FULL-ladder live proof (eval-only, no model): the
    /// reference broker driven through the real pipeline with acceptance
    /// [1..9] — including the durability half (prepare -> SIGKILL ->
    /// restart -> verify) and the 128MB-capped hardening restart.

    /// sdb-http FULL-ladder live proof (eval-only, no model): the
    /// reference file server driven through the real pipeline with
    /// acceptance [1..10] — including the 128MB-capped restart that runs
    /// stages 9/10 (uploads + hardening).

    /// sdb-sql stages 0-3 live proof (eval-only, no model): the Python
    /// sqlite3 reference adapter (`verifier::sql::ref_server`) driven
    /// through the real pipeline with acceptance [0, 1, 2, 3]. All four
    /// stage scripts run against one long-lived server, which also proves
    /// the seed-namespaced schemas cannot collide.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stages_0_3_live() {
        let worker = 112u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqllive");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stages 0-3 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqllive");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 3,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: Some("# SPEC.md fixture\n".to_string()),
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("sdb-sql stages 0-3 must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        for needle in [
            "Stage 0: Genesis: tables, insert, select -> PASS",
            "Stage 1: Expressions and WHERE -> PASS",
            "Stage 2: Sorting and paging -> PASS",
            "Stage 3: UPDATE and DELETE -> PASS",
        ] {
            assert_logged(&logger, needle);
        }
        assert_proof_score(&out, &logger, 4, 4);
        // The harness-owned contract lands in the prepared workspace.
        assert_eq!(
            fs::read_to_string(workdir.join("SPEC.md")).unwrap(),
            "# SPEC.md fixture\n"
        );
        cleanup_covtest_network("sqllive");
        let _ = fs::remove_dir_all(&dir);
    }

    /// R45 F1 acceptance (live): the reference against itself scores
    /// 1.0 ± 0.1 on every workload over 5 alternating rounds. Both sides
    /// run in the sandbox (2 CPUs, 2 GiB) through the production runner.
    #[test]
    fn r45_f1_reference_scores_one() {
        use crate::verifier::sql::perfscore::{run_workspace_at, PerfConfig};
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let dir = covtest_dir("perfref");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, 5480).unwrap();
        let score = run_workspace_at(&workdir, 4, &PerfConfig::default(), &dir.join("runs"))
            .expect("the reference workspace must start and score");
        for w in &score.workloads {
            assert!(
                (w.ratio - 1.0).abs() <= 0.1,
                "{} ratio {:.3} (correct {}) note {:?}: {:?}",
                w.id,
                w.ratio,
                w.correct,
                w.note,
                w.numbers
            );
        }
        let value = score.score.expect("the reference must score");
        assert!(
            (value - 1.0).abs() <= 0.1,
            "score {value:.3}: {:?}",
            score.workloads
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// R49 F2 (live): a reference with `synchronous=OFF` as the candidate
    /// floors W4 on the real device flush counter, while the FULL
    /// reference passes its own check and the phase keeps a score.
    #[test]
    fn r49_f2_unsynced_candidate_floors_live() {
        // The lock line is first so scripts/live-proofs.sh names this test,
        // not the helper fns below it.
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        use crate::verifier::sql::perfscore::{
            run_pair_with_flush, DeviceFlushCounter, PerfConfig,
        };
        use std::process::{Command, Stdio};
        struct RefProc(std::process::Child);
        impl Drop for RefProc {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        fn free_port() -> u16 {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        }
        fn start(
            script: &std::path::Path,
            port: u16,
            data: &std::path::Path,
            sync: &str,
        ) -> RefProc {
            let child = Command::new("python3")
                .arg("-u")
                .arg(script)
                .args(["--port", &port.to_string(), "--data"])
                .arg(data)
                .env("SDB_SQL_SYNCHRONOUS", sync)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("start the reference");
            let proc = RefProc(child);
            crate::verifier::sql::driver::wait_until_answering(
                &format!("127.0.0.1:{port}"),
                std::time::Duration::from_secs(10),
            )
            .expect("the reference answers");
            proc
        }
        let dir = covtest_dir("perfunsync");
        let ws = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&ws, 5480).unwrap();
        let ref_data = dir.join("ref-data");
        let cand_data = dir.join("cand-data");
        fs::create_dir_all(&ref_data).unwrap();
        fs::create_dir_all(&cand_data).unwrap();
        let ref_port = free_port();
        let cand_port = free_port();
        let script = ws.join("sql_server.py");
        let _ref = start(&script, ref_port, &ref_data, "FULL");
        let _cand = start(&script, cand_port, &cand_data, "OFF");
        let ref_flush = DeviceFlushCounter::for_dir(&ref_data).expect("reference flush counter");
        let cand_flush = DeviceFlushCounter::for_dir(&cand_data).expect("candidate flush counter");
        let cfg = PerfConfig {
            rounds: 2,
            rows_w1: 1_000,
            lookups: 20,
            w4_seconds: 0.5,
            timeout: std::time::Duration::from_secs(15),
            workload_deadline: std::time::Duration::from_secs(60),
            ..PerfConfig::small_for_tests()
        };
        let score = run_pair_with_flush(
            &format!("127.0.0.1:{ref_port}"),
            &format!("127.0.0.1:{cand_port}"),
            1,
            &cfg,
            Some(&ref_flush),
            Some(&cand_flush),
        );
        let w4 = score
            .workloads
            .iter()
            .find(|w| w.id == "W4")
            .expect("phase 1 runs W4");
        assert!(
            !w4.correct,
            "synchronous=OFF must floor W4: {:?}",
            w4.numbers
        );
        assert!(
            w4.note.as_deref().unwrap_or("").contains("not durable"),
            "{:?}",
            w4.note
        );
        assert!(
            score.score.is_some(),
            "the candidate floor keeps the phase scored: {:?}",
            score.error
        );
    }

    /// R45 F5 acceptance (live): the CLI path measures a finished
    /// workspace and prints a score of 1.0 ± 0.1. The small config keeps
    /// the phase-4 shape (all five workloads) without the 30 s W5.
    #[test]
    fn r45_f5_perfscore_cli() {
        use crate::verifier::sql::perfscore::{cli_with_at, render, PerfConfig};
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let dir = covtest_dir("perfcli");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, 5480).unwrap();
        let score = cli_with_at(
            &workdir,
            &PerfConfig::fixture_for_tests(),
            &dir.join("runs"),
        )
        .expect("the reference workspace must start and score");
        let text = render(&score);
        assert!(text.contains("perf score"), "{text}");
        assert_eq!(score.workloads.len(), 5, "{text}");
        let value = score.score.unwrap_or_else(|| {
            panic!(
                "the reference must score: error {:?}; {:?}",
                score.error, score.workloads
            )
        });
        assert!((value - 1.0).abs() <= 0.1, "score {value:.3}\n{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 4 live proof (eval-only, no model): the reference
    /// passes the cumulative suite through stage 4 — the differential
    /// script plus the persistence scenario, which restarts the candidate
    /// container on the same harness-owned store — and leaves a sentinel
    /// for later tickets.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage4_persistence_live() {
        let worker = 75u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlpersist");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-4 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlpersist");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 4,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-4 fixture run must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        for needle in [
            "Stage 0: Genesis: tables, insert, select -> PASS",
            "Stage 4: Persistence -> PASS",
            "  [UPGRADE] sentinel for stage 4 written",
        ] {
            assert_logged(&logger, needle);
        }
        assert_proof_score(&out, &logger, 5, 5);
        assert!(
            dir.join("sentinel-4.json").is_file(),
            "the sentinel must be saved next to the store"
        );
        cleanup_covtest_network("sqlpersist");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 5 live proof (eval-only, no model): the reference
    /// survives the crash ladder under Docker — acked rows across a kill
    /// -9, a mid-flight 50k-row UPDATE, a torn newest file, and a double
    /// restart — and the stage result stays green.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage5_crash_live() {
        let worker = 80u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlcrash");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-5 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlcrash");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 5,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 5],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-5 fixture run must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        for needle in [
            "Stage 5: Crash safety -> PASS",
            "[CRASH] 40 acked rows",
            "[CRASH] the killed bulk UPDATE was all-or-nothing",
            "[CRASH] a double restart changed nothing",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 6, 6);
        cleanup_covtest_network("sqlcrash");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stage 5's lite variant (proposal: acked rows survive kill -9; no
    /// torn tail or mid-statement atomicity) runs the narrowed ladder: the
    /// reference passes and the bulk/torn checks do not run.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage5_lite_live() {
        let worker = 85u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlcrashlite");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-5 lite fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlcrashlite");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 5,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 5],
            lite_profile: vec!["crash-acked-only".to_string()],
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-5 lite fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 5: Crash safety -> PASS",
            "[CRASH] lite variant: acked rows and restarts only",
            "[CRASH] 40 acked rows",
            "[CRASH] a double restart changed nothing",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        for absent in [
            "[CRASH] the killed bulk UPDATE was all-or-nothing",
            "torn bytes",
        ] {
            assert!(
                !logger.contains(absent),
                "the lite variant must not run {absent:?}"
            );
        }
        assert_proof_score(&out, &logger, 6, 6);
        cleanup_covtest_network("sqlcrashlite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A stage-6 ticket after the model won stage 5 on the lite variant:
    /// the suite keeps grading stage 5 lite (the full crash ladder would
    /// demand work ticket 5 never asked for) while stage 6 runs in full.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage6_after_lite_stage5() {
        let worker = 87u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqltxnlite5");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-6 fixture after lite stage 5").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqltxnlite5");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 6,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 5, 6],
            lite_profile: Vec::new(),
            passed_lite: vec![(5, vec!["crash-acked-only".to_string()])],
            passed_stages: (0..=5).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-6 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 5: Crash safety -> PASS",
            "Stage 6: Transactions -> PASS",
            "[CRASH] lite variant: acked rows and restarts only",
            "[TXN] a kill with an open transaction discarded it",
        ] {
            assert_logged(&logger, needle);
        }
        for absent in [
            "[CRASH] the killed bulk UPDATE was all-or-nothing",
            "torn bytes",
        ] {
            assert!(
                !logger.contains(absent),
                "stage 5 must stay lite in a later ticket: {absent:?}"
            );
        }
        assert_proof_score(&out, &logger, 7, 7);
        cleanup_covtest_network("sqltxnlite5");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stage 7 after a failed storage tier: the acceptance list carries
    /// only the *passed* stages plus the current ticket, so stage 7's suite
    /// runs the aggregate grammar against stages 0-4 and never touches the
    /// crash ladder or the transaction checks (finding: the raw declared
    /// list made one storage failure sink every later ticket).
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage7_without_storage_stages() {
        let worker = 100u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlno5");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-7 fixture without stage 5").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlno5");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 7,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 7],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: vec![0, 1, 2, 3, 4],
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-7 fixture run must complete");
        assert_proof_ok(&out, &logger);
        assert!(
            logger.contains("Stage 7: Aggregates -> PASS"),
            "missing the stage-7 pass; log:\n{}",
            logger.text()
        );
        for absent in ["[CRASH]", "[TXN]"] {
            assert!(
                !logger.contains(absent),
                "{absent} ran although stage 5/6 are not in the suite"
            );
        }
        assert_proof_score(&out, &logger, 6, 6);
        cleanup_covtest_network("sqlno5");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage-11 live proof (eval-only, no model): the reference
    /// server driven through the real pipeline with acceptance [0..11],
    /// including the perf gate (two 200k-row tables, an indexed copy, 500
    /// alternating lookups, a restart, and the measured ratios).
    /// Ticket 13 live proof: the reference (SQLite) runs the whole accepted
    /// suite through stage 13, including the ALTER-heavy differential
    /// script, the schema-change stage and the cumulative extras.
    /// Ticket 14 live proof: the reference answers the plan and the counter
    /// statements, and the cumulative suite through stage 14 passes.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage14_observability_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 42u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqlobs");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqlobs");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-14 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlobs");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 14,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=14).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..14).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-14 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 14: Observability -> PASS",
            "Stage 13: Schema change -> PASS",
            "Stage 12: Concurrency and isolation -> PASS",
            "Stage 11: Indexes + perf gate -> PASS",
            "[OBSERVABILITY] EXPLAIN names the index/scan",
            "[UPGRADE] sentinel for stage 14 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 15, 15);
        cleanup_covtest_network("sqlobs");
        let _ = fs::remove_dir_all(&dir);
    }

    /// R27 F4: stage-12 full live proof. The reference adapter runs the
    /// cumulative suite through stage 12; the concurrency scenario checks
    /// lost update, transfers and non-blocking readers.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage12_concurrency_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 30u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqlconc");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqlconc");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-12 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlconc");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 12,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=12).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..12).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-12 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 12: Concurrency and isolation -> PASS",
            "[CONCURRENCY] serializable: lost update, transfers, and readers never blocked",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 13, 13);
        cleanup_covtest_network("sqlconc");
        let _ = fs::remove_dir_all(&dir);
    }

    /// R27 F4: stage-12 lite live proof. The global-lock profile must pass
    /// the lost-update and transfer checks; it does not claim non-blocking
    /// readers.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage12_concurrency_lite_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 28u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqlconclite");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqlconclite");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-12 lite fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlconclite");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 12,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=12).collect(),
            lite_profile: vec!["concurrency-global-lock".to_string()],
            passed_lite: Vec::new(),
            passed_stages: (0..12).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-12 lite fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 12: Concurrency and isolation -> PASS",
            "[CONCURRENCY] lite: lost update and transfers are serializable (global lock)",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 13, 13);
        cleanup_covtest_network("sqlconclite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 15's gate needs `./test.sh`, a coverage line and stage-7/8
    /// commits; stage 16's acceptance includes ticket 15, so both live
    /// proofs install the same fixture. `sql_server.py` stays byte-identical
    /// to the harness reference (the anti-cheat check recognizes it).
    fn install_tests_ci_fixture(workdir: &std::path::Path, port: u16) {
        install_tests_ci_fixture_with_coverage(workdir, port, 85.0);
    }

    /// R27 F4: the same stage-15 fixture with a chosen self-reported
    /// coverage, so the lite proof can show the 60% gate accepting a value
    /// the full 80% gate would reject.
    fn install_tests_ci_fixture_with_coverage(workdir: &std::path::Path, port: u16, coverage: f64) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(workdir)
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
        git(&["config", "user.email", "ref@example.com"]);
        git(&["config", "user.name", "reference"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 6: Transactions [pass]"]);
        // The feature commits carry files the suite greps for; the reference
        // adapter itself stays untouched.
        std::fs::write(workdir.join("feature_aggregates.py"), "AGG = True\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 7: Aggregates [pass]"]);
        std::fs::write(workdir.join("feature_joins.py"), "JOIN = True\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "stage 8: Joins [pass]"]);
        std::fs::write(
            workdir.join("coverage.txt"),
            format!("coverage: {coverage:.1}%\n"),
        )
        .unwrap();
        let suite = format!(
            "#!/bin/sh\n\
             set -eu\n\
             grep -q '^AGG = True' feature_aggregates.py\n\
             grep -q '^JOIN = True' feature_joins.py\n\
             ./start.sh &\n\
             pid=$!\n\
             trap 'kill $pid 2>/dev/null || true' EXIT\n\
             python3 - {port} <<'PY'\n\
             import socket, sys, time\n\
             port = int(sys.argv[1])\n\
             for _ in range(100):\n\
             \x20   try:\n\
             \x20       s = socket.create_connection((\"127.0.0.1\", port), 1)\n\
             \x20       break\n\
             \x20   except OSError:\n\
             \x20       time.sleep(0.05)\n\
             else:\n\
             \x20   raise SystemExit(\"server did not start\")\n\
             s.sendall(b\"SELECT 1;\\n\")\n\
             line = s.makefile(\"rb\").readline()\n\
             s.close()\n\
             if not line.startswith(b\"ROWS\"):\n\
             \x20   raise SystemExit(\"unexpected answer: %r\" % line)\n\
             PY\n"
        );
        std::fs::write(workdir.join("test.sh"), suite).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                workdir.join("test.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }

    /// Ticket 15 live proof (eval-only, no model): the reference workspace
    /// ships an offline `test.sh`, a coverage line and stage-7/8 commits; the
    /// real pipeline runs the cumulative suite through stage 15 and the gate
    /// (two green runs, coverage, both sensitivity reverts) passes.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage15_tests_ci_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 45u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqltests");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqltests");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-15 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqltests");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 15,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=15).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..15).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-15 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 15: Tests and CI gate -> PASS",
            "Stage 14: Observability -> PASS",
            "[TESTS] ./test.sh green (run 1/2, offline, <= 120s)",
            "[TESTS] ./test.sh green (run 2/2, offline, <= 120s)",
            "[TESTS] coverage 85.0% (self-report, not verified; gate 80%)",
            "[TESTS] sensitivity ok (stage-7 diff reversed)",
            "[TESTS] sensitivity ok (stage-8 diff reversed)",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 16, 16);
        cleanup_covtest_network("sqltests");
        let _ = fs::remove_dir_all(&dir);
    }

    /// R27 F4: stage-15 lite live proof. The fixture self-reports 65%
    /// coverage: the lite 60% gate accepts it, where the full 80% gate
    /// would fail it. The suite and both sensitivity reverts still pass.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage15_tests_ci_lite_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 43u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqltestslite");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqltestslite");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture_with_coverage(&workdir, port, 65.0);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-15 lite fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqltestslite");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 15,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=15).collect(),
            lite_profile: vec!["coverage-60".to_string()],
            passed_lite: Vec::new(),
            passed_stages: (0..15).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-15 lite fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 15: Tests and CI gate -> PASS",
            "Stage 14: Observability -> PASS",
            "[TESTS] coverage 65.0% (self-report, not verified; gate 60%)",
            "[TESTS] sensitivity ok (stage-7 diff reversed)",
            "[TESTS] sensitivity ok (stage-8 diff reversed)",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 16, 16);
        cleanup_covtest_network("sqltestslite");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn suite_variants_carry_the_lite_grammar_into_later_suites() {
        let passed_lite = vec![(8u32, vec!["join-inner-only".to_string()])];
        let wanted: Vec<u32> = (0..=19).collect();
        let variants = suite_variants(&passed_lite, &["two-nodes-promote".to_string()], &wanted);
        assert_eq!(
            variants,
            vec![
                (8u32, vec!["join-inner-only".to_string()]),
                (19u32, vec!["two-nodes-promote".to_string()]),
            ]
        );
        assert_eq!(
            suite_variants(&passed_lite, &[], &wanted),
            vec![(8u32, vec!["join-inner-only".to_string()])]
        );
    }

    #[test]
    fn repl_control_failures_split_harness_faults_from_crashes() {
        use crate::verifier::sql::replication::HaAction;
        // Docker failed on a node that is still up, or could not say.
        assert!(repl_control_failure_is_infra(
            HaAction::Pause(1),
            Some("running")
        ));
        assert!(repl_control_failure_is_infra(
            HaAction::Resume(1),
            Some("paused")
        ));
        assert!(repl_control_failure_is_infra(HaAction::Kill(0), None));
        // The candidate's node already exited: a model failure.
        assert!(!repl_control_failure_is_infra(
            HaAction::Kill(0),
            Some("exited")
        ));
        assert!(!repl_control_failure_is_infra(
            HaAction::Pause(2),
            Some("dead")
        ));
        // A rejoin recreates the container: always the harness.
        assert!(repl_control_failure_is_infra(
            HaAction::Rejoin {
                node: 0,
                primary: 1
            },
            Some("exited")
        ));
    }

    /// Ticket 16 live proof (eval-only, no model): the reference runs the
    /// cumulative suite through stage 16; the memory extra restarts it on a
    /// fresh scratch store under a 64 MiB cap, loads 400k x 900 B rows and
    /// checks the full aggregate, the external sort and the indexed join
    /// without being OOM-killed.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage16_memory_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 51u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqlmemory");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqlmemory");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-16 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlmemory");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 16,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=16).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..16).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-16 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 16: Larger than memory -> PASS",
            "Stage 15: Tests and CI gate -> PASS",
            "[MEMORY] gate runs on a fresh scratch store under a 64 MiB cap",
            "[MEMORY] 400000 rows (~343 MiB of text)",
            "aggregate, ORDER BY and indexed join exact",
            "[UPGRADE] sentinel for stage 16 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 17, 17);
        cleanup_covtest_network("sqlmemory");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 17 live proof (eval-only, no model): the reference runs the
    /// cumulative suite through stage 17; the incident extra restarts it on
    /// a 16 MiB tmpfs, fills the volume through the harness, checks that a
    /// too-large write is refused without being acked or applied, re-reads
    /// the seeds, frees the filler and retries the write successfully.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage17_disk_full_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 55u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqldisk");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqldisk");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-17 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqldisk");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 17,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=17).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..17).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-17 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 17: Incident: disk full -> PASS",
            "Stage 16: Larger than memory -> PASS",
            "[DISK] incident runs on a fresh 16 MiB tmpfs at /sdb-data",
            "[DISK] seed 40 rows; ENOSPC refused the write and never applied it",
            "the retry succeeded after the filler was removed",
            "[UPGRADE] sentinel for stage 17 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 18, 18);
        cleanup_covtest_network("sqldisk");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 18 live proof (eval-only, no model): the reference runs the
    /// cumulative suite through stage 18; the hardening extra restarts it
    /// with a password on a fresh scratch store and drives AUTH, fuzzing,
    /// the nesting/size/time limits, a slow-loris client and the
    /// 256-connection cap.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage18_security_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 60u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        cleanup_covtest_network("sqlsec");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqlsec");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-18 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlsec");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 18,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=18).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..18).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = expect_proof(
            BenchmarkPipeline::execute(config, logger.clone(), None),
            &logger,
            "the stage-18 fixture run must complete",
        )
        .await;
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 18: Hostile input and limits -> PASS",
            "Stage 17: Incident: disk full -> PASS",
            "Stage 16: Larger than memory -> PASS",
            "[SECURITY] scenario runs on a fresh scratch store with SDB_SQL_PASSWORD set",
            "[SECURITY] auth enforced; fuzz/mutations survived; nesting, 16777216-byte, 5s and slow-loris limits held; 256-connection cap enforced",
            "[UPGRADE] sentinel for stage 18 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 19, 19);
        cleanup_covtest_network("sqlsec");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 19 live proof (eval-only, no model): three reference nodes,
    /// a sync-acked transaction, a primary kill with election, a new write,
    /// the old primary rejoined, and the cumulative suite on the winner.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage19_replication_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 65u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let ha_ports = [port + 60, port + 61, port + 62];
        cleanup_covtest_network("sqlrepl");
        for p in [port, ha_ports[0], ha_ports[1], ha_ports[2]] {
            if let Ok(out) = std::process::Command::new("docker")
                .args(["ps", "-aq", "--filter", &format!("publish={p}")])
                .output()
            {
                let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                if !ids.is_empty() {
                    let mut args = vec!["rm".to_string(), "-f".to_string()];
                    args.extend(ids);
                    let _ = std::process::Command::new("docker").args(&args).output();
                }
            }
        }
        let dir = covtest_dir("sqlrepl");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-19 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlrepl");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 19,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=19).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..19).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-19 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 19: Replication and failover -> PASS",
            "Stage 18: Hostile input and limits -> PASS",
            "[REPL] cluster of 3 nodes on ports",
            "[REPL] 3 nodes; the replicas were frozen across the COMMIT",
            "[REPL] the cumulative differential suite passes against the current primary",
            "[UPGRADE] sentinel for stage 19 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 20, 20);
        cleanup_covtest_network("sqlrepl");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 19 lite live proof: two nodes, kill the primary, manual
    /// `PROMOTE`, then the cumulative suite on the promoted node.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage19_replication_lite_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 63u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let ha_ports = [port + 60, port + 61];
        cleanup_covtest_network("sqlrepllite");
        for p in [port, ha_ports[0], ha_ports[1]] {
            if let Ok(out) = std::process::Command::new("docker")
                .args(["ps", "-aq", "--filter", &format!("publish={p}")])
                .output()
            {
                let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                if !ids.is_empty() {
                    let mut args = vec!["rm".to_string(), "-f".to_string()];
                    args.extend(ids);
                    let _ = std::process::Command::new("docker").args(&args).output();
                }
            }
        }
        let dir = covtest_dir("sqlrepllite");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-19-lite fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlrepllite");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 19,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=19).collect(),
            lite_profile: vec!["two-nodes-promote".to_string()],
            passed_lite: Vec::new(),
            passed_stages: (0..19).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-19 lite fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 19: Replication and failover -> PASS",
            "[REPL] cluster of 2 nodes on ports",
            "[REPL] 2 nodes; the replica was frozen across the COMMIT",
            "[REPL] the cumulative differential suite passes against the current primary",
            "[UPGRADE] sentinel for stage 19 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 20, 20);
        cleanup_covtest_network("sqlrepllite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 20 live proof (eval-only, no model): the reference serves
    /// PostgreSQL protocol 3.0 on a second port; the harness frontend and a
    /// stock psql check SSL, startup/auth, simple + extended query,
    /// transactions and SQLSTATE.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage20_pgwire_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 73u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let pg_port = port + 50;
        cleanup_covtest_network("sqlpg");
        for p in [port, pg_port] {
            if let Ok(out) = std::process::Command::new("docker")
                .args(["ps", "-aq", "--filter", &format!("publish={p}")])
                .output()
            {
                let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                if !ids.is_empty() {
                    let mut args = vec!["rm".to_string(), "-f".to_string()];
                    args.extend(ids);
                    let _ = std::process::Command::new("docker").args(&args).output();
                }
            }
        }
        let dir = covtest_dir("sqlpg");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-20 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlpg");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 20,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=20).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..20).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-20 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 20: PostgreSQL wire protocol -> PASS",
            "Stage 19: Replication and failover -> PASS",
            "[PG] PostgreSQL wire scenario on port",
            "[PG] stock psql SELECT 1 -> 1",
            "[PG] SSL refused; startup/auth/ParameterStatus ok; simple + extended query",
            "[UPGRADE] sentinel for stage 20 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 21, 21);
        cleanup_covtest_network("sqlpg");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 20 lite live proof: the simple-query protocol only (no
    /// extended query), plus the psql cross-check.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage20_pgwire_lite_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 88u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let pg_port = port + 50;
        cleanup_covtest_network("sqlpglite");
        for p in [port, pg_port] {
            if let Ok(out) = std::process::Command::new("docker")
                .args(["ps", "-aq", "--filter", &format!("publish={p}")])
                .output()
            {
                let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                if !ids.is_empty() {
                    let mut args = vec!["rm".to_string(), "-f".to_string()];
                    args.extend(ids);
                    let _ = std::process::Command::new("docker").args(&args).output();
                }
            }
        }
        let dir = covtest_dir("sqlpglite");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        install_tests_ci_fixture(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-20-lite fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlpglite");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 20,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=20).collect(),
            lite_profile: vec!["pg-simple".to_string()],
            passed_lite: Vec::new(),
            passed_stages: (0..20).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-20 lite fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 20: PostgreSQL wire protocol -> PASS",
            "[PG] PostgreSQL wire scenario on port",
            "[PG] stock psql SELECT 1 -> 1",
            "[PG] SSL refused; startup/auth/ParameterStatus ok; simple query, transactions and SQLSTATE",
            "[UPGRADE] sentinel for stage 20 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 21, 21);
        cleanup_covtest_network("sqlpglite");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage13_schema_change_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 33u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        // A previous failed run's reference holds this worker's host port;
        // a new run would silently talk to that stale binary. Clear it.
        cleanup_covtest_network("sqlschema");
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={port}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
        let dir = covtest_dir("sqlschema");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-13 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlschema");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 13,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=13).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..13).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-13 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 13: Schema change -> PASS",
            "Stage 12: Concurrency and isolation -> PASS",
            "Stage 11: Indexes + perf gate -> PASS",
            "Stage 4: Persistence -> PASS",
            "[UPGRADE] sentinel for stage 13 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 14, 14);
        cleanup_covtest_network("sqlschema");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The SQL live proofs through stage 11 each run the cumulative suite,
    /// including the perf gate (which needs an idle host) and their own
    /// reference container. Run them one at a time; three parallel gates
    /// turn timing into noise.
    static SQL_PERF_LIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage11_indexes_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 21u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlidx");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-11 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlidx");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 11,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=11).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..11).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-11 fixture run must complete");
        assert_proof_ok(&out, &logger);
        // gui-spec.md §5.1: every verification round is recorded in the
        // manifest (the initial try here; eval-only runs have no repairs).
        assert!(
            out.manifest
                .rounds
                .iter()
                .any(|r| r.round == 0 && !r.stage_results.is_empty()),
            "round 0 recorded with results: {:?}",
            out.manifest.rounds
        );
        for needle in [
            "Stage 11: Indexes + perf gate -> PASS",
            "[PERF] gate runs on a fresh scratch store",
            "[PERF] point p50",
            "after restart",
            "[UPGRADE] sentinel for stage 11 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 12, 12);
        cleanup_covtest_network("sqlidx");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 10 live proof (eval-only, no model): the reference
    /// passes the subquery suite under Docker — scalar subqueries,
    /// `[NOT] IN (subquery)`/`IN (list)`, correlated `[NOT] EXISTS` and
    /// `INSERT ... SELECT` — plus everything 0-9 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage10_subqueries_live() {
        let worker = 8u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlsub");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-10 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlsub");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 10,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=10).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: (0..10).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-10 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 10: Subqueries -> PASS",
            "Stage 9: Constraints -> PASS",
            "[UPGRADE] sentinel for stage 10 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 11, 11);
        cleanup_covtest_network("sqlsub");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 9 live proof (eval-only, no model): the reference
    /// passes the constraint suite under Docker — constrained DDL, key
    /// auto-assignment, defaults, UNIQUE/NOT NULL and foreign keys
    /// (including the violation probes) — plus everything 0-8 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage9_constraints_live() {
        let worker = 105u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlcons");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-9 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlcons");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 9,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=9).collect(),
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-9 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 9: Constraints -> PASS",
            "Stage 8: Joins -> PASS",
            "[UPGRADE] sentinel for stage 9 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 10, 10);
        cleanup_covtest_network("sqlcons");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 8 live proof (eval-only, no model): the reference
    /// passes the join suite under Docker — comma/`INNER`/`LEFT` joins with
    /// `ON`, qualified references, `SELECT *` over a join, the ambiguity
    /// probe — plus everything 0-7 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage8_joins_live() {
        let worker = 102u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqljoin");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-8 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqljoin");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 8,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-8 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 8: Joins -> PASS",
            "Stage 7: Aggregates -> PASS",
            "[UPGRADE] sentinel for stage 8 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 9, 9);
        cleanup_covtest_network("sqljoin");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stage 11's lite variant through the pipeline: the attempt carries
    /// `index-equality-only`, so the perf gate drops its range round and
    /// only the equality point lookups are gated.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage11_lite_live() {
        let worker = 24u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlidxlite");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-11 lite fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlidxlite");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 11,
            prompt_path,
            continuity: true,
            accepted_stages: (0..=11).collect(),
            lite_profile: vec!["index-equality-only".to_string()],
            passed_lite: Vec::new(),
            passed_stages: (0..11).collect(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-11 lite fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in ["Stage 11: Indexes + perf gate -> PASS", "[PERF] point p50"] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert!(
            !logger.contains("range p50"),
            "the lite gate must not measure ranges; log:\n{}",
            logger.text()
        );
        assert_proof_score(&out, &logger, 12, 12);
        cleanup_covtest_network("sqlidxlite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stage 8's lite variant through the pipeline: the attempt carries
    /// `join-inner-only`, so the graded suite keeps comma/inner joins and
    /// never runs a `LEFT JOIN`.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage8_lite_live() {
        let worker = 104u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqljoinlite");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-8 lite fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqljoinlite");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 8,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
            lite_profile: vec!["join-inner-only".to_string()],
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-8 lite fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in ["Stage 8: Joins -> PASS", "Stage 7: Aggregates -> PASS"] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 9, 9);
        cleanup_covtest_network("sqljoinlite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 7 live proof (eval-only, no model): the reference
    /// passes the aggregate suite under Docker — `COUNT`/`SUM`/`MIN`/`MAX`,
    /// grouped queries with `HAVING`, the empty-group contract and the
    /// aggregate error probes — plus everything 0-6 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage7_aggregates_live() {
        let worker = 97u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlagg");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-7 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlagg");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 7,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 5, 6, 7],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-7 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 7: Aggregates -> PASS",
            "Stage 6: Transactions -> PASS",
            "Stage 5: Crash safety -> PASS",
            "[UPGRADE] sentinel for stage 7 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 8, 8);
        cleanup_covtest_network("sqlagg");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 6 live proof (eval-only, no model): the reference
    /// passes the transactions suite under Docker — cross-connection
    /// isolation (two connections, uncommitted rows invisible) and a kill
    /// -9 with a transaction open.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage6_transactions_live() {
        let worker = 70u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqltxn");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql stage-6 fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqltxn");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 6,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4, 5, 6],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-6 fixture run must complete");
        assert_proof_ok(&out, &logger);
        for needle in [
            "Stage 6: Transactions -> PASS",
            "[TXN] uncommitted rows stay invisible across connections",
            "[TXN] a kill with an open transaction discarded it",
            "[UPGRADE] sentinel for stage 6 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_proof_score(&out, &logger, 7, 7);
        cleanup_covtest_network("sqltxn");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A sentinel whose data is gone fails the stage that wrote it
    /// (regression class `upgrade`): the check runs before the suite and
    /// the stage result is overridden even though the suite itself passes.
    #[tokio::test]
    async fn test_pipeline_sql_sentinel_drift_fails_the_writing_stage() {
        let worker = 117u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqldrift");
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql sentinel-drift fixture").unwrap();
        // A sentinel the store never saw: its reads cannot answer.
        let sent = crate::verifier::sql::sentinel::build(
            0x5EED_0001,
            4,
            &crate::verifier::sql::generator::Profile::through(4),
        )
        .unwrap();
        crate::verifier::sql::sentinel::save(&dir.join("sentinel-4.json"), &sent).unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqldrift");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 4,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0, 1, 2, 3, 4],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the drift fixture run must complete");
        assert_proof_ok(&out, &logger);
        assert!(
            logger.contains("[UPGRADE] sentinel from stage 4 FAILED"),
            "the drift must be reported"
        );
        assert!(
            logger.contains("Stage 4: Persistence -> FAIL"),
            "the writing stage must fail"
        );
        assert!(
            logger.contains("[upgrade]"),
            "the stage error must carry the upgrade class"
        );
        // The suite itself passed: stages 0-3 are green, only stage 4 is red.
        assert_proof_score(&out, &logger, 4, 5);
        cleanup_covtest_network("sqldrift");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding 2: a failing sql stage must still get a *minimized*
    /// repro. The shrink trials run on throwaway stores, so the replay is
    /// trustworthy (with the old reset, the candidate's persistent store
    /// answered `exists` and every report fell back to the unminimized
    /// prefix). The real store must survive with only the initial and the
    /// restore starts.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_shrink_uses_a_throwaway_store() {
        let worker = 118u8;
        let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
        let dir = covtest_dir("sqlshrink");
        let workdir = dir.join("workspace");
        write_always_ok_sql_mock(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-sql shrink fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "sqlshrink");
        config.task = TaskType::Sql;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-sql".to_string(),
            stage: 0,
            prompt_path,
            continuity: true,
            accepted_stages: vec![0],
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed_stages: Vec::new(),
            spec_md: None,
            preamble: None,
            cap_headroom_usd: None,
        });
        let logger = Arc::new(RecordingLogger::default());
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the shrink fixture run must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        assert!(
            logger.contains("Stage 0: Genesis: tables, insert, select -> FAIL"),
            "the always-OK candidate must fail stage 0"
        );

        // The shrink logged its work: trials > 0 and a small repro.
        let line = logger
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|l| l.contains("shrink trials"))
            .cloned()
            .expect("the sql arm must log the repro size");
        let toks: Vec<&str> = line.split_whitespace().collect();
        let statements: usize = toks[1].parse().expect("statement count");
        let trials: usize = toks[3]
            .trim_start_matches('(')
            .parse()
            .expect("trial count");
        assert!(trials > 0, "the shrink never ran: {line}");
        assert!(statements <= 8, "repro not minimized: {line}");

        // Throwaway stores live next to the real one, keyed per instance and
        // cleaned once the stage is graded; the real store saw only the
        // initial start plus the post-shrink restore, never a trial's rows.
        let markers = |p: &Path| -> usize {
            fs::read_dir(p)
                .map(|it| {
                    it.flatten()
                        .filter(|e| e.file_name().to_string_lossy().starts_with("marker-"))
                        .count()
                })
                .unwrap_or(0)
        };
        let real = dir.join("sql-data");
        let real_markers = markers(&real);
        assert!(
            (1..=2).contains(&real_markers),
            "the real store was written by {real_markers} starts (expected initial + restore)"
        );
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .map(|it| {
                it.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with("sql-shrink"))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "shrink stores must not accumulate: {leftovers:?}"
        );

        cleanup_covtest_network("sqlshrink");
        let _ = fs::remove_dir_all(&dir);
    }
}
