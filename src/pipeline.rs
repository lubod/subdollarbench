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

/// R56 F3: the first other live lane in the cells' runs dir, if any.
fn other_live_lane(runs: &std::path::Path, worker: u8) -> Option<u32> {
    crate::campaign::live_lock_holders_in(runs)
        .into_iter()
        .find(|h| h.worker != worker)
        .map(|h| h.worker as u32)
}

/// R56 F3: the note when the provider key is shared with another lane.
fn shared_key_note(runs: &std::path::Path, worker: u8) -> Option<String> {
    other_live_lane(runs, worker).map(|w| format!("shared key: w{w} live"))
}

/// R56 F3: the live delta is audit-only; with another lane it is not
/// attributable to this cell, so it is dropped and the note says why.
fn gate_spend_delta(delta: Option<f64>, note: Option<String>) -> (Option<f64>, Option<String>) {
    if note.is_some() {
        (None, note)
    } else {
        (delta, None)
    }
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
                let phase_candidate =
                    crate::verifier::sql::PhaseTimer::new("candidate start + ready wait");
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
                } else {
                    logger.log(&format!("[WARN] Port {} not responding within 25s timeout. Proceeding with verification.", port));
                }
                drop(phase_candidate);

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
                            let phase_stage = crate::verifier::sql::PhaseTimer::new(format!(
                                "stage {stage} differential suite"
                            ));
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
                            drop(phase_stage);
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
                                let _phase = crate::verifier::sql::PhaseTimer::new(
                                    "late stage 11 perf gate",
                                );
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
                                let _phase =
                                    crate::verifier::sql::PhaseTimer::new("late stage 16 memory");
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
                                let _phase = crate::verifier::sql::PhaseTimer::new(
                                    "late stage 17 disk full",
                                );
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
                                let _phase =
                                    crate::verifier::sql::PhaseTimer::new("late stage 18 security");
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
                                let _phase = crate::verifier::sql::PhaseTimer::new(
                                    "late stage 19 replication",
                                );
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
                                let _phase =
                                    crate::verifier::sql::PhaseTimer::new("late stage 20 pgwire");
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
        let runs_dir = crate::report::archive::RunArchiver::resolve_runs_dir();
        let shared_at_start = shared_key_note(&runs_dir, config.worker);

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
                "[SETUP] Private run network '{}' ready (reference alias: ref-sql)",
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
        let phase_ref = crate::verifier::sql::PhaseTimer::new("reference server start");
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
        drop(phase_ref);

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
        // (`ref-*` — fixed ports inside the run network, identical for
        // every worker). Rewrite only candidate-local
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

        let shared_note = shared_at_start.or_else(|| shared_key_note(&runs_dir, config.worker));
        let (live_spend_delta, live_spend_note) = if shared_note.is_some() {
            (None, shared_note)
        } else {
            let mut delta = None;
            if let (Some(key), Some(init)) = (&config.api_key, initial_spend) {
                sleep(Duration::from_millis(1500)).await;
                if let Some(fin) = ModelPricing::query_openrouter_key_usage(key).await {
                    if fin >= init {
                        delta = Some(fin - init);
                    }
                }
            }
            gate_spend_delta(delta, None)
        };

        // R24 R2: the committed snapshot is the scored cost. The live key
        // delta is audit-only; a provider promo or lag cannot move a score.
        let (cost_usd, cost_source) =
            ModelPricing::resolve_api_cost(live_spend_delta, breakdown.total_cost_usd);
        if let Some(live) = live_spend_delta {
            logger.log(&format!(
                "[COST] snapshot ${:.4}; live key delta ${:.4} recorded for audit only",
                breakdown.total_cost_usd, live
            ));
        } else if let Some(note) = &live_spend_note {
            logger.log(&format!(
                "[COST] snapshot ${:.4}; live key delta not recorded ({note})",
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
            rounds: campaign_rounds.clone(),
            robustness: extra_robustness,
            throughput_req_sec: throughput,
            reference_throughput_req_sec: reference_throughput,
            baseline_source: baseline_source.clone(),
            ref_drift: None,
            // Anchored rows only: an unanchored row must stay "legacy" even
            // after its task gains a reference image; otherwise regime_of
            // implies comparable ratios where throughput_score is None.
            // R43: sdb-sql measures its perf score, not a reference image.
            ref_image: None,
            // Recorded so a reader can tell an enforced run from an open one
            // without trusting the archive tooling. A run with egress_enforced
            // false (or absent, from before this field) made no isolation claim.
            egress_enforced: Some(crate::sandbox::docker::egress_filter_enabled()),
            // R65 F5: the prompt is the full input context. On the
            // Anthropic and OpenAI routes the cache-write share arrives in
            // `cacheWrite`/`cache_creation_input_tokens`, not in `input`;
            // folding it in here (never in the parser, where the cost
            // prices it separately) makes `total = prompt + cached +
            // completion` hold. The cost and floor inputs above are
            // unchanged.
            tokens: RunTokenUsage {
                prompt_tokens: prompt_tokens + omp_stats.cache_write_tokens,
                cached_tokens,
                completion_tokens,
                total_tokens: prompt_tokens
                    + omp_stats.cache_write_tokens
                    + cached_tokens
                    + completion_tokens,
            },
            cost_usd,
            cost_source: Some(cost_source.to_string()),
            cost_formula_usd: Some(breakdown.total_cost_usd),
            live_spend_delta_usd: live_spend_delta,
            live_spend_note: live_spend_note.clone(),
            stall: omp_stats.stall.clone(),
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
            // R43: sdb-sql measures its perf score, not a reference image.
            ref_image: None,
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
mod tests;
