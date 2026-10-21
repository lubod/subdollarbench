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
use crate::sandbox::docker::{RunGuard, SandboxManager, PROXY_UPSTREAM_PORTS};

/// Upstream pool ports for a worker (base pool + worker*100 offset).
fn worker_upstream_ports(worker: u8) -> [u16; 3] {
    [
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[0], worker),
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[1], worker),
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[2], worker),
    ]
}

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
async fn wait_until_serving(port: u16, timeout_secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while !candidate_serves_http(port) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// One raw `GET /` probe against a candidate: true when any HTTP response
/// arrives (any status). A socket that accepts but never answers (a slow
/// startup self-test, a wedged accept loop) does not count as ready.
fn candidate_serves_http(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    if s.write_all(b"GET / HTTP/1.0\r\nHost: x\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 64];
    matches!(s.read(&mut buf), Ok(n) if n > 0) && buf[0] == b'H'
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
use crate::verifier::queue::QueueVerifier;
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

/// The last `n` non-empty lines of `text` (round evidence tails).
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
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
    /// Round-14 N2/N3: the run is not a measurement at all - the agent never
    /// reached its first turn (after one retry), or the provider rejected
    /// the key/billing. A campaign stops on it and never publishes the cell.
    pub harness_fault: Option<String>,
    /// The largest single provider call this attempt made (the campaign
    /// cap's reserve, round-17 P0).
    pub max_call_usd: f64,
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
                let mut extras = crate::sandbox::docker::CandidateRunExtras {
                    ports: extra_ports,
                    ..Default::default()
                };
                let mut sql_data: Option<PathBuf> = None;
                if matches!(task, TaskType::Sql) {
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
                    // Readiness grace (proxy/http): a candidate that binds
                    // early but serves late (slow startup self-test) must
                    // not eat the verifier's single first request. Bounded
                    // so a genuinely dead server is graded as-is.
                    if matches!(task, TaskType::Proxy | TaskType::Http) {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(20);
                        let mut served = candidate_serves_http(port);
                        while !served && std::time::Instant::now() < deadline {
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            served = candidate_serves_http(port);
                        }
                        if served {
                            logger.log("[SETUP] Candidate answered a readiness probe (GET /).");
                        } else {
                            logger.log(
                                "[SETUP] Warning: candidate did not answer a readiness probe within 20s; grading as-is.",
                            );
                        }
                    }
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
                                                let _ = sandbox.connect_candidate_to_network();
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
                                                let _ = sandbox.connect_candidate_to_network();
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
                        // Single-shot http keeps its 5-stage board; the
                        // sdb-http campaign adds stages 6-10 (ranges,
                        // validators, compression, upload limits, hardening)
                        // which run only when a ticket explicitly accepts
                        // them.
                        let allowed = |n: u32| accepted.map(|a| a.contains(&n)).unwrap_or(true);
                        let campaign_only =
                            |n: u32| accepted.map(|a| a.contains(&n)).unwrap_or(false);
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
                        let fail_stage =
                            |stage: u32, name: &str, err: String| crate::verifier::StageResult {
                                stage,
                                name: name.to_string(),
                                passed: false,
                                error: Some(err),
                            };
                        let verifier = HttpVerifier::new(port, Some(verifier_seed));
                        let mut stages: Vec<crate::verifier::StageResult> = Vec::new();
                        let mut prior_failed = false;
                        let mut base_ran = false;
                        if allowed(1) {
                            base_ran = true;
                            let st = verifier.test_stage1_root().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(2) {
                            base_ran = true;
                            let st = verifier.test_stage2_not_found().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(3) {
                            base_ran = true;
                            let st = verifier.test_stage3_echo().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(4) {
                            base_ran = true;
                            let st = verifier.test_stage4_user_agent().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(5) {
                            base_ran = true;
                            let st = verifier.test_stage5_file_storage().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if base_ran {
                            for s in verifier.run_robustness().await {
                                log_robust(logger, std::slice::from_ref(&s));
                                extra_robustness.push(s);
                            }
                        }
                        if campaign_only(6) {
                            let st = if prior_failed {
                                fail_stage(
                                    6,
                                    "Stage 6: Range requests (206 + Content-Range)",
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                verifier.test_stage6_range().await
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if campaign_only(7) {
                            let st = if prior_failed {
                                fail_stage(
                                    7,
                                    "Stage 7: Conditional GET (ETag / Last-Modified -> 304)",
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                verifier.test_stage7_conditional().await
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if campaign_only(8) {
                            let st = if prior_failed {
                                fail_stage(
                                    8,
                                    "Stage 8: gzip content negotiation",
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                verifier.test_stage8_compression().await
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if campaign_only(9) || campaign_only(10) {
                            // Upload limits and hardening run under a 128MB
                            // cap: restart the candidate, then restore it.
                            if prior_failed {
                                if campaign_only(9) {
                                    let st = fail_stage(
                                        9,
                                        "Stage 9: Chunked uploads, 100-continue, size limits",
                                        "skipped due to failure in earlier stages".to_string(),
                                    );
                                    log_stage(logger, &st);
                                    stages.push(st);
                                }
                                if campaign_only(10) {
                                    let st = fail_stage(
                                        10,
                                        "Stage 10: Hardening (traversal, limits, slow-loris)",
                                        "skipped due to failure in earlier stages".to_string(),
                                    );
                                    log_stage(logger, &st);
                                    stages.push(st);
                                }
                            } else {
                                let limits_extras = crate::sandbox::docker::CandidateRunExtras {
                                    memory: Some("128m".to_string()),
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
                                    Err(e) => {
                                        let msg =
                                            format!("memory-capped candidate start failed: {e}");
                                        for (stage, stage_name) in [
                                            (9u32, "Stage 9: Chunked uploads, 100-continue, size limits"),
                                            (10, "Stage 10: Hardening (traversal, limits, slow-loris)"),
                                        ] {
                                            if campaign_only(stage) {
                                                let st = fail_stage(stage, stage_name, msg.clone());
                                                log_stage(logger, &st);
                                                stages.push(st);
                                            }
                                        }
                                    }
                                    Ok(()) => {
                                        let _ = sandbox.connect_candidate_to_network();
                                        if !sandbox.wait_for_port(port, 25).await {
                                            let msg = "candidate did not reopen its port under the memory cap".to_string();
                                            for (stage, stage_name) in [
                                                (9u32, "Stage 9: Chunked uploads, 100-continue, size limits"),
                                                (10, "Stage 10: Hardening (traversal, limits, slow-loris)"),
                                            ] {
                                                if campaign_only(stage) {
                                                    let st =
                                                        fail_stage(stage, stage_name, msg.clone());
                                                    log_stage(logger, &st);
                                                    stages.push(st);
                                                }
                                            }
                                        } else {
                                            wait_until_serving(port, 20).await;
                                            if campaign_only(9) {
                                                let st = verifier.test_stage9_uploads().await;
                                                prior_failed |= !st.passed;
                                                log_stage(logger, &st);
                                                stages.push(st);
                                            }
                                            if campaign_only(10) {
                                                let st = if prior_failed {
                                                    fail_stage(
                                                        10,
                                                        "Stage 10: Hardening (traversal, limits, slow-loris)",
                                                        "skipped due to failure in earlier stages"
                                                            .to_string(),
                                                    )
                                                } else {
                                                    verifier.test_stage10_hardening().await
                                                };
                                                log_stage(logger, &st);
                                                stages.push(st);
                                            }
                                        }
                                    }
                                }
                                let _ = sandbox.stop_candidate();
                                let _ =
                                    sandbox.start_candidate_in_docker_with(workdir, port, &extras);
                                let _ = sandbox.wait_for_port(port, 25).await;
                                wait_until_serving(port, 20).await;
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
                        // Pool health is a container-state question: the host
                        // publishes exist only for harness-side fixture reads
                        // (which fall back to byte-identical in-process stubs),
                        // and rebuilding the pool here would kill the
                        // candidate's pooled connections and health state right
                        // before routing stages (observed as spurious
                        // round-robin/failover flakes). Only a missing or
                        // stopped pool is rebuilt.
                        let pool_ok = (1u8..=3u8).all(|i| sandbox.upstream_container_running(i));
                        if !pool_ok {
                            logger.log(
                                "[SETUP] Upstream pool missing/stopped at verify; rebuilding...",
                            );
                            match sandbox.start_upstream_pool(&worker_upstream_ports(worker)) {
                                Ok(()) if (1u8..=3u8).all(|i| sandbox.upstream_container_running(i)) => {
                                    logger.log("[SETUP] Upstream pool rebuilt.");
                                }
                                Ok(()) => logger.log(
                                    "[SETUP] Warning: rebuilt upstream pool not all running; proxy stages may flap.",
                                ),
                                Err(e) => logger.log(&format!(
                                    "[SETUP] Warning: upstream pool rebuild failed: {}. Proxy stages will fail.",
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
                                            // Restarts land on the default
                                            // bridge: re-attach so upstream
                                            // aliases resolve.
                                            if let Err(e) = sandbox.connect_candidate_to_network() {
                                                logger.log(&format!(
                                                    "[SETUP] Warning: candidate network attach failed: {}. Upstream dials may fail.",
                                                    e
                                                ));
                                            }
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
                                        if let Err(e) = sandbox.connect_candidate_to_network() {
                                            logger.log(&format!(
                                                "[SETUP] Warning: candidate network attach failed: {}. Upstream dials may fail.",
                                                e
                                            ));
                                        }
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
                                                         written: {} writes, {} reads",
                                                        s.writes.len(),
                                                        s.queries.len()
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
                    TaskType::Queue => {
                        let allowed = |n: u32| accepted.map(|a| a.contains(&n)).unwrap_or(true);
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
                        let fail_stage =
                            |stage: u32, name: &str, err: String| crate::verifier::StageResult {
                                stage,
                                name: name.to_string(),
                                passed: false,
                                error: Some(err),
                            };
                        let verifier = QueueVerifier::new(port, Some(verifier_seed));
                        let mut stages: Vec<crate::verifier::StageResult> = Vec::new();
                        let mut prior_failed = false;
                        if allowed(1) {
                            let st = verifier.test_stage1_basic().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(2) {
                            let st = verifier.test_stage2_leases().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(3) {
                            let st = verifier.test_stage3_nack().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(4) {
                            let st = verifier.test_stage4_dlq().await;
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(5) {
                            // Durability is destructive: prepare durable
                            // state, SIGKILL, restart, verify.
                            let name5 = "Stage 5: Durability across kill -9";
                            let st = if prior_failed {
                                fail_stage(
                                    5,
                                    name5,
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                let prep = verifier.test_stage5_durability_prepare().await;
                                if !prep.passed {
                                    prep
                                } else {
                                    logger.log("  [QUEUE] durability: SIGKILL + restart...");
                                    let crash = sandbox.kill_candidate().and_then(|_| {
                                        std::thread::sleep(std::time::Duration::from_secs(2));
                                        sandbox.restart_candidate()
                                    });
                                    match crash {
                                        Err(e) => fail_stage(
                                            5,
                                            name5,
                                            format!("SIGKILL/restart failed: {e}"),
                                        ),
                                        Ok(()) => {
                                            let _ = sandbox.connect_candidate_to_network();
                                            if !sandbox.wait_for_port(port, 25).await {
                                                fail_stage(
                                                    5,
                                                    name5,
                                                    "broker did not reopen its port after restart"
                                                        .to_string(),
                                                )
                                            } else {
                                                verifier.test_stage5_durability_verify().await
                                            }
                                        }
                                    }
                                }
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(6) {
                            let st = if prior_failed {
                                fail_stage(
                                    6,
                                    "Stage 6: No duplicate delivery under contention",
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                verifier.test_stage6_concurrency().await
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(7) {
                            let st = if prior_failed {
                                fail_stage(
                                    7,
                                    "Stage 7: Observability (STATS counters)",
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                verifier.test_stage7_stats().await
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(8) {
                            let st = if prior_failed {
                                fail_stage(
                                    8,
                                    "Stage 8: Delayed delivery + DLQ redrive",
                                    "skipped due to failure in earlier stages".to_string(),
                                )
                            } else {
                                verifier.test_stage8_delay_redrive().await
                            };
                            prior_failed |= !st.passed;
                            log_stage(logger, &st);
                            stages.push(st);
                        }
                        if allowed(9) {
                            let name9 = "Stage 9: Limits and hostile input";
                            let st = if prior_failed {
                                fail_stage(
                                    9,
                                    name9,
                                    "skipped due to failure in earlier stages".to_string(),
                                )
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
                                    Err(e) => fail_stage(
                                        9,
                                        name9,
                                        format!("memory-capped candidate start failed: {e}"),
                                    ),
                                    Ok(()) => {
                                        let _ = sandbox.connect_candidate_to_network();
                                        if !sandbox.wait_for_port(port, 25).await {
                                            fail_stage(
                                                9,
                                                name9,
                                                "candidate did not reopen its port under the memory cap"
                                                    .to_string(),
                                            )
                                        } else {
                                            verifier.test_stage9_hardening().await
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
                    Ok(()) => {
                        if (1u8..=3u8).all(|i| sandbox.upstream_container_running(i)) {
                            logger.log("[SETUP] Upstream nginx pool on :8101/:8102/:8103 ready.");
                        } else {
                            logger.log(
                                "[SETUP] Warning: upstream pool not all running; proxy stages may flap.",
                            );
                        }
                    }
                    Err(e) => logger.log(&format!(
                        "[SETUP] Warning: upstream pool unavailable: {}. Proxy stages will fail.",
                        e
                    )),
                }
            }
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
            TaskType::Wal | TaskType::Pubsub | TaskType::Binproto | TaskType::Queue => {
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
        let attempt_started = std::time::Instant::now();
        let mut round_started = std::time::Instant::now();
        // Effective cap headroom for this attempt, after the cell's spend and
        // largest-call reserve. None outside capped campaigns (round-17 P0).
        let cap_headroom = config
            .campaign_stage
            .as_ref()
            .and_then(|c| c.cap_headroom_usd);
        // Q8: the in-session stop leaves room for the final verification's
        // compute floor (the Haiku tickets 8-10 measured ~$0.0006; $0.002
        // is a conservative reserve under the $0.01 margin).
        const FINAL_VERIFY_RESERVE_USD: f64 = 0.002;
        let cap_effective = cap_headroom.map(|c| (c - FINAL_VERIFY_RESERVE_USD).max(0.0));
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
                    max_effective_usd: cap_effective,
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
                            acc.total_tokens += stats.total_tokens;
                            acc.steps_taken += stats.steps_taken;
                            acc.cost_usd += stats.cost_usd;
                            acc.max_call_usd = acc.max_call_usd.max(stats.max_call_usd);
                            acc.effort_effective = stats.effort_effective.clone();
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
                            acc.total_tokens += end.partial.total_tokens;
                            acc.steps_taken += end.partial.steps_taken;
                            acc.cost_usd += end.partial.cost_usd;
                            acc.max_call_usd = acc.max_call_usd.max(end.partial.max_call_usd);
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
                    let total_tokens_so_far = prompt_tokens + cached_tokens + completion_tokens;
                    let floor_so_far = crate::cost::calculator::ModelPricing::compute_cost(
                        attempt_started.elapsed().as_secs_f64(),
                        total_tokens_so_far,
                        cached_tokens,
                    );
                    let attempt_budget = if config.budget_usd > 0.0 {
                        config.budget_usd
                    } else {
                        f64::INFINITY
                    };
                    let (repair_budget, repair_cap) = match crate::campaign::repair_round_spend(
                        attempt_budget,
                        spent_api,
                        cap_effective,
                    ) {
                        Ok(v) => v,
                        Err(reason) => {
                            logger.log(&format!(
                                "[LIMIT] Repair round {round} not started: {reason}"
                            ));
                            break;
                        }
                    };
                    let repair_effective = repair_cap.map(|cap| cap - (spent_api + floor_so_far));
                    if let Some(eff) = repair_effective {
                        if eff < crate::campaign::BUDGET_FLOOR_USD {
                            logger.log(&format!(
                                "[LIMIT] Repair round {round} not started: effective headroom ${eff:.4} below the floor"
                            ));
                            break;
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
                    round_started = std::time::Instant::now();
                    let round_tokens_before = (prompt_tokens, cached_tokens, completion_tokens);
                    let mut round_steps: Option<u32> = None;

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
                        max_effective_usd: repair_effective,
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
                                round_steps = Some(r_stats.steps_taken);
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
                TaskType::Wal
                | TaskType::Pubsub
                | TaskType::Binproto
                | TaskType::Queue
                | TaskType::Sql => {
                    if let Some(tp) = extra_throughput {
                        let unit = match config.task {
                            TaskType::Wal => "msgs/sec",
                            TaskType::Queue => "jobs/sec",
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
                    // No proxy denominator: the direct nginx baseline
                    // (ref-http) is not an apples-to-apples reference for a
                    // proxy and the value is discarded below, so skip the
                    // calibration attempt entirely (it logged a misleading
                    // "nginx reference FAILED" for every proxy run).
                    logger.log(
                        "[BENCH] No proxy denominator (direct nginx is not an apples-to-apples reference); rate reported, not ranked.",
                    );
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
        Ok(PipelineOutput {
            manifest,
            benchmark_result,
            agent_failed,
            disqualified,
            infra_error,
            harness_fault,
            max_call_usd: omp_stats.max_call_usd,
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

    /// sdb-edge reference candidate (live ladder proof): a stdlib-only
    /// reverse proxy with round-robin over the three upstream aliases,
    /// upstream timeouts + retry (hung-upstream failover), active health
    /// probes and `GET /status`, a TTL cache (`SDB_PROXY_CACHE_TTL`,
    /// `X-Cache`/`Age`), and request limits (8KiB head -> 431, >1MiB
    /// declared body -> 413, slow-loris reclaimed in ~3s). Env override
    /// `SDB_EDGE_UPSTREAMS` points it at local stubs for development.
    const EDGE_MOCK_SERVER_PY: &str = r#"import os
import socket
import sys
import threading
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8091

# Real campaigns: the three run-network upstreams by alias. The env override
# exists so the reference can be exercised against local stubs.
_env = os.environ.get("SDB_EDGE_UPSTREAMS")
if _env:
    UPSTREAMS = []
    for part in _env.split(","):
        h, p = part.rsplit(":", 1)
        UPSTREAMS.append((h, int(p)))
else:
    UPSTREAMS = [("upstream1", 80), ("upstream2", 80), ("upstream3", 80)]

CACHE_TTL = int(os.environ.get("SDB_PROXY_CACHE_TTL", "0") or "0")
HEAD_CAP = 8 * 1024          # oversized request head -> 431
MAX_BODY = 1024 * 1024       # declared body above this -> 413 before reading
CLIENT_READ_TIMEOUT = 3.0    # slow-loris: partial head reclaimed in ~3s
UPSTREAM_TIMEOUT = 1.0       # hung upstream: per-attempt budget
ATTEMPT_BUDGET = 2.2         # total per-request upstream budget (< 2.5s gate)
PROBE_INTERVAL = 1.0

health = [True] * len(UPSTREAMS)
health_lock = threading.Lock()
_rr = 0
_rr_lock = threading.Lock()
cache = {}
_cache_lock = threading.Lock()


def _send(sock, data):
    try:
        sock.sendall(data)
    except Exception:
        pass


def http_response(status, reason, body, keep_alive, extra=""):
    conn = "keep-alive" if keep_alive else "close"
    head = (
        f"HTTP/1.1 {status} {reason}\r\n"
        f"Content-Length: {len(body)}\r\n"
        f"Connection: {conn}\r\n"
        f"{extra}\r\n"
    )
    return head.encode("latin-1") + body


def parse_upstream(data):
    head, _, body = data.partition(b"\r\n\r\n")
    line = head.split(b"\r\n", 1)[0]
    parts = line.split(b" ")
    status = int(parts[1]) if len(parts) > 1 else 502
    headers = {}
    for raw in head.split(b"\r\n")[1:]:
        if b":" in raw:
            k, v = raw.split(b":", 1)
            headers[k.strip().lower().decode("latin-1")] = v.strip().decode("latin-1")
    return status, body, headers


def fetch_upstream(idx, path, client_ip):
    host, port = UPSTREAMS[idx]
    sock = socket.create_connection((host, port), timeout=UPSTREAM_TIMEOUT)
    try:
        sock.settimeout(UPSTREAM_TIMEOUT)
        req = (
            f"GET {path} HTTP/1.1\r\nHost: {host}\r\n"
            f"X-Forwarded-For: {client_ip}\r\nConnection: close\r\n\r\n"
        )
        sock.sendall(req.encode("latin-1"))
        data = b""
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            data += chunk
            if len(data) > 8 * 1024 * 1024:
                break
    finally:
        sock.close()
    if not data:
        raise ValueError("empty upstream response")
    return parse_upstream(data)


def healthy_order():
    global _rr
    with _rr_lock:
        start = _rr
        _rr += 1
    n = len(UPSTREAMS)
    with health_lock:
        hs = list(health)
    order = [(start + k) % n for k in range(n)]
    healthy = [i for i in order if hs[i]]
    return healthy if healthy else order


def proxy_get(path, client_ip):
    global cache
    if CACHE_TTL > 0:
        with _cache_lock:
            entry = cache.get(path)
        if entry and (time.time() - entry[1]) < CACHE_TTL:
            age = int(time.time() - entry[1])
            return 200, entry[0], f"X-Cache: HIT\r\nAge: {age}\r\n"
    status, body, up_headers = 502, b"no upstream", {}
    chosen = None
    deadline = time.time() + ATTEMPT_BUDGET
    order = healthy_order()
    for idx in order:
        if time.time() >= deadline:
            break
        try:
            status, body, up_headers = fetch_upstream(idx, path, client_ip)
            chosen = idx
            break
        except Exception:
            continue
    if (
        CACHE_TTL > 0
        and chosen is not None
        and status == 200
        and "no-store" not in up_headers.get("cache-control", "")
    ):
        with _cache_lock:
            cache[path] = (body, time.time())
    extra = "X-Cache: MISS\r\n" if CACHE_TTL > 0 else ""
    return status, body, extra


def status_body():
    with health_lock:
        hs = list(health)
    return "".join(
        f"upstream{i + 1}: {'up' if hs[i] else 'down'}\n" for i in range(len(UPSTREAMS))
    ).encode()


def read_head(sock):
    """Return the request head bytes, b'TOO_BIG', or None on close/timeout."""
    sock.settimeout(CLIENT_READ_TIMEOUT)
    buf = b""
    try:
        while b"\r\n\r\n" not in buf:
            chunk = sock.recv(1024)
            if not chunk:
                return None
            buf += chunk
            if len(buf) > HEAD_CAP:
                return b"TOO_BIG"
    except socket.timeout:
        return None
    except OSError:
        return None
    return buf


def handle_client(conn, addr):
    client_ip = addr[0]
    try:
        while True:
            head = read_head(conn)
            if head is None:
                return
            if head == b"TOO_BIG":
                _send(conn, http_response(431, "Request Header Fields Too Large", b"", False))
                return
            text = head.split(b"\r\n\r\n", 1)[0].decode("latin-1")
            lines = text.split("\r\n")
            parts = lines[0].split(" ")
            method = parts[0] if parts else ""
            path = parts[1] if len(parts) > 1 else "/"
            headers = {}
            for line in lines[1:]:
                if ":" in line:
                    k, v = line.split(":", 1)
                    headers[k.strip().lower()] = v.strip()
            cl = 0
            if headers.get("content-length"):
                try:
                    cl = int(headers["content-length"])
                except ValueError:
                    cl = 0
            if cl > MAX_BODY:
                _send(conn, http_response(413, "Payload Too Large", b"", False))
                return
            keep = headers.get("connection", "keep-alive").lower() != "close"
            if path == "/status":
                _send(conn, http_response(200, "OK", status_body(), keep))
                continue
            if method != "GET":
                _send(conn, http_response(405, "Method Not Allowed", b"", False))
                return
            status, body, extra = proxy_get(path, client_ip)
            _send(conn, http_response(status, "OK" if status == 200 else "Bad Gateway", body, keep, extra))
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def prober():
    while True:
        for idx in range(len(UPSTREAMS)):
            try:
                status, _, _ = fetch_upstream(idx, "/", "127.0.0.1")
                ok = status == 200
            except Exception:
                ok = False
            with health_lock:
                health[idx] = ok
        time.sleep(PROBE_INTERVAL)


def serve():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", PORT))
    srv.listen(128)
    print(f"edge proxy listening on {PORT} cache_ttl={CACHE_TTL}", flush=True)
    threading.Thread(target=prober, daemon=True).start()
    while True:
        conn, addr = srv.accept()
        threading.Thread(target=handle_client, args=(conn, addr), daemon=True).start()


serve()
"#;

    /// Write the sdb-edge reference proxy into `dir`, listening on `port`.
    fn write_edge_mock(dir: &Path, port: u16) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("proxy.py"), EDGE_MOCK_SERVER_PY).unwrap();
        let start = format!("#!/bin/bash\nexec python3 -u /workspace/proxy.py {port}\n");
        fs::write(dir.join("start.sh"), start).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir.join("start.sh"), fs::Permissions::from_mode(0o755));
        }
    }

    /// sdb-queue reference broker (live ladder proof): stdlib-only job
    /// queue with connection-owned visibility leases, nacks/delays,
    /// dead-lettering after 3 attempts (inspection reads do not count),
    /// redrive, per-queue STATS, delayed enqueue, and an append-only
    /// fsync-before-ack log under `./data` (torn tails dropped on
    /// replay). Env override `SDB_QUEUE_DATA` points it elsewhere for
    /// local development.
    const QUEUE_MOCK_SERVER_PY: &str = r#"import json
import os
import re
import socket
import sys
import threading
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 6385
DATA = os.environ.get("SDB_QUEUE_DATA") or os.path.join(os.getcwd(), "data")
os.makedirs(DATA, exist_ok=True)
LOGF = os.path.join(DATA, "queue.log")

MAX_ATTEMPTS = 3
MAX_PAYLOAD = 8 * 1024
NAME_RE = re.compile(r"^[A-Za-z0-9._-]+$")

lock = threading.RLock()
jobs = {}          # id -> dict
next_id = 1


def log_append(rec):
    """Append one JSON record and fsync before the caller acknowledges."""
    with open(LOGF, "a", buffering=1) as f:
        f.write(json.dumps(rec, separators=(",", ":")) + "\n")
        f.flush()
        os.fsync(f.fileno())


def now():
    return time.time()


def replay():
    global next_id
    if not os.path.exists(LOGF):
        return
    good = 0
    with open(LOGF, "rb") as f:
        for raw in f:
            if not raw.endswith(b"\n"):
                break  # torn tail
            try:
                rec = json.loads(raw)
            except Exception:
                break
            apply_record(rec)
            good += len(raw)
    size = os.path.getsize(LOGF)
    if good < size:
        # Discard the torn tail so future appends stay parseable.
        with open(LOGF, "r+b") as f:
            f.truncate(good)
    if jobs:
        next_id = max(jobs) + 1


def apply_record(rec):
    t = rec.get("t")
    if t == "enq":
        jobs[rec["id"]] = {
            "id": rec["id"],
            "origin": rec["q"],
            "queue": rec["q"],
            "payload": rec["p"],
            "attempts": 0,
            "nacks": 0,
            "state": "avail",
            "owner": 0,
            "until": 0.0,
            "not_before": rec.get("nb", 0.0),
        }
    elif t == "lease":
        j = jobs.get(rec["id"])
        if j:
            j["attempts"] = rec["a"]
    elif t == "ack":
        j = jobs.get(rec["id"])
        if j:
            j["state"] = "acked"
    elif t == "nack":
        j = jobs.get(rec["id"])
        if j:
            j["nacks"] += 1
            j["state"] = rec["st"]
            j["queue"] = rec["q"]
            j["not_before"] = rec.get("nb", 0.0)
    elif t == "redrive":
        j = jobs.get(rec["id"])
        if j:
            j["queue"] = rec["q"]
            j["state"] = "avail"
            j["attempts"] = 0
            j["not_before"] = 0.0


def expire(now_ts):
    """Leased jobs past their visibility window return (attempts kept)."""
    for j in jobs.values():
        if j["state"] == "leased" and j["until"] <= now_ts:
            j["owner"] = 0
            if j["attempts"] >= MAX_ATTEMPTS and not j["queue"].endswith(".dead"):
                j["queue"] = j["origin"] + ".dead"
                j["state"] = "dead"
            elif j["attempts"] >= MAX_ATTEMPTS:
                j["state"] = "dead"
            else:
                j["state"] = "avail"
                j["not_before"] = now_ts


def stats(q):
    mine = [j for j in jobs.values() if j["origin"] == q]
    enq = len(mine)
    acked = sum(1 for j in mine if j["state"] == "acked")
    nacked = sum(j["nacks"] for j in mine)
    inflight = sum(1 for j in mine if j["state"] == "leased")
    available = sum(1 for j in mine if j["state"] == "avail")
    dead = sum(1 for j in mine if j["state"] == "dead")
    return (
        f"+STATS enqueued={enq} acked={acked} nacked={nacked} "
        f"inflight={inflight} available={available} dead={dead}"
    )


def handle(line, conn_id):
    global next_id
    parts = line.split(" ")
    with lock:
        t = now()
        expire(t)
        if parts[0] == "PING":
            return "+PONG"
        if parts[0] == "ENQ" and len(parts) in (3, 5) and NAME_RE.match(parts[1]):
            payload = parts[2]
            delay_ms = 0.0
            if len(parts) == 5:
                if parts[3] != "IN":
                    return "-ERR syntax"
                try:
                    delay_ms = float(parts[4])
                except ValueError:
                    return "-ERR syntax"
            if len(payload) > MAX_PAYLOAD:
                return "-ERR payload too large"
            jid = next_id
            next_id += 1
            log_append(
                {"t": "enq", "id": jid, "q": parts[1], "p": payload, "nb": t + delay_ms / 1000.0}
            )
            apply_record({"t": "enq", "id": jid, "q": parts[1], "p": payload, "nb": t + delay_ms / 1000.0})
            return f"+OK {jid}"
        if parts[0] == "LEASE" and len(parts) == 3:
            try:
                lease_ms = float(parts[2])
            except ValueError:
                return "-ERR syntax"
            pick = None
            for j in sorted(jobs.values(), key=lambda x: x["id"]):
                if (
                    j["queue"] == parts[1]
                    and j["not_before"] <= t
                    and j["state"] in ("avail", "dead")
                ):
                    pick = j
                    break
            if pick is None:
                return "+NONE"
            if not pick["queue"].endswith(".dead"):
                pick["attempts"] += 1
            pick["owner"] = conn_id
            pick["until"] = t + lease_ms / 1000.0
            pick["state"] = "leased"
            log_append({"t": "lease", "id": pick["id"], "a": pick["attempts"]})
            return f"+JOB {pick['id']} {pick['attempts']} {pick['payload']}"
        if parts[0] in ("ACK", "NACK") and len(parts) in (2, 3):
            try:
                jid = int(parts[1])
            except ValueError:
                return "-ERR syntax"
            j = jobs.get(jid)
            if j is None or j["state"] != "leased":
                return "-ERR not-leased"
            if j["owner"] != conn_id:
                return "-ERR not-owner"
            if parts[0] == "ACK":
                j["state"] = "acked"
                log_append({"t": "ack", "id": jid})
                return "+OK"
            delay_ms = 0.0
            if len(parts) == 3:
                try:
                    delay_ms = float(parts[2])
                except ValueError:
                    return "-ERR syntax"
            j["owner"] = 0
            j["nacks"] += 1
            if j["attempts"] >= MAX_ATTEMPTS and not j["queue"].endswith(".dead"):
                j["queue"] = j["origin"] + ".dead"
                j["state"] = "dead"
            elif j["attempts"] >= MAX_ATTEMPTS:
                j["state"] = "dead"
            else:
                j["state"] = "avail"
                j["not_before"] = t + delay_ms / 1000.0
            log_append(
                {"t": "nack", "id": jid, "q": j["queue"], "st": j["state"], "nb": j["not_before"]}
            )
            return "+OK"
        if parts[0] == "REDRIVE" and len(parts) == 3:
            try:
                jid = int(parts[2])
            except ValueError:
                return "-ERR syntax"
            j = jobs.get(jid)
            if j is None or j["state"] != "dead" or j["queue"] != parts[1] + ".dead":
                return "-ERR unknown"
            j["queue"] = parts[1]
            j["state"] = "avail"
            j["attempts"] = 0
            j["not_before"] = 0.0
            log_append({"t": "redrive", "id": jid, "q": parts[1]})
            return "+OK"
        if parts[0] == "STATS" and len(parts) == 2:
            return stats(parts[1])
        return "-ERR syntax"


def serve_conn(conn, conn_id):
    try:
        conn.settimeout(3.0)  # slow-loris: reclaim stalled partial lines
        buf = b""
        while True:
            try:
                chunk = conn.recv(4096)
            except socket.timeout:
                return
            if not chunk:
                return
            buf += chunk
            while b"\n" in buf:
                raw, buf = buf.split(b"\n", 1)
                try:
                    line = raw.decode("latin-1").rstrip("\r")
                except Exception:
                    line = ""
                reply = handle(line, conn_id)
                conn.sendall((reply + "\n").encode("latin-1"))
            if len(buf) > MAX_PAYLOAD * 2:
                conn.sendall(b"-ERR line too long\n")
                return
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def main():
    replay()
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", PORT))
    srv.listen(128)
    print(f"queue broker listening on {PORT} data={DATA}", flush=True)
    conn_seq = 0
    while True:
        conn, _ = srv.accept()
        conn_seq += 1
        threading.Thread(target=serve_conn, args=(conn, conn_seq), daemon=True).start()


main()
"#;

    /// Write the sdb-queue reference broker into `dir`, listening on `port`.
    fn write_queue_mock(dir: &Path, port: u16) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("queue_broker.py"), QUEUE_MOCK_SERVER_PY).unwrap();
        let start = format!("#!/bin/bash\nexec python3 -u /workspace/queue_broker.py {port}\n");
        fs::write(dir.join("start.sh"), start).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir.join("start.sh"), fs::Permissions::from_mode(0o755));
        }
    }

    /// sdb-http reference file server (live ladder proof): stdlib-only
    /// HTTP/1.1 server with keep-alive, echo/user-agent endpoints, a
    /// file store under `./data/files`, byte ranges (incl. suffix/open
    /// and 416), ETag/Last-Modified validators with strict conditional
    /// semantics, gzip negotiation, chunked + `Expect: 100-continue`
    /// uploads with a 16MiB declared-body cap, a 8KiB head cap, path
    /// traversal rejection and a ~3s stalled-head reclaim. Env override
    /// `SDB_HTTP_BASE` points the store elsewhere for local development.
    const HTTP_MOCK_SERVER_PY: &str = r#"import email.utils
import gzip
import os
import re
import socket
import sys
import threading
import time
from urllib.parse import unquote

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
BASE = os.environ.get("SDB_HTTP_BASE") or os.path.join(os.getcwd(), "data")
FILES = os.path.join(BASE, "files")
os.makedirs(FILES, exist_ok=True)

HEAD_CAP = 8 * 1024
UPLOAD_CAP = 16 * 1024 * 1024
READ_TIMEOUT = 3.0
NAME_RE = re.compile(r"^[A-Za-z0-9._-]+$")
GZIP_MIN = 1024

STATUS_REASON = {
    200: "OK",
    201: "Created",
    206: "Partial Content",
    304: "Not Modified",
    400: "Bad Request",
    403: "Forbidden",
    404: "Not Found",
    405: "Method Not Allowed",
    413: "Payload Too Large",
    416: "Range Not Satisfiable",
    431: "Request Header Fields Too Large",
}


def http_date(ts):
    return email.utils.formatdate(ts, usegmt=True)


def parse_http_date(value):
    try:
        dt = email.utils.parsedate_to_datetime(value)
        if dt is None:
            return None
        return dt.timestamp()
    except Exception:
        return None


def file_info(name):
    path = os.path.join(FILES, name)
    try:
        st = os.stat(path)
    except OSError:
        return None
    return path, st.st_size, int(st.st_mtime)


def etag_for(name, size, mtime):
    return f"\"{name}-{size}-{mtime}\""


def response(status, body=b"", extra=(), close=False, reason=None):
    head = [f"HTTP/1.1 {status} {reason or STATUS_REASON.get(status, 'OK')}"]
    for k, v in extra:
        head.append(f"{k}: {v}")
    if status != 304:
        head.append(f"Content-Length: {len(body)}")
    head.append("Connection: close" if close else "Connection: keep-alive")
    return ("\r\n".join(head) + "\r\n\r\n").encode("latin-1") + (b"" if status == 304 else body)


def read_head(sock):
    sock.settimeout(READ_TIMEOUT)
    buf = b""
    try:
        while b"\r\n\r\n" not in buf:
            chunk = sock.recv(1024)
            if not chunk:
                return None
            buf += chunk
            if len(buf) > HEAD_CAP:
                return b"TOO_BIG"
    except socket.timeout:
        return None
    except OSError:
        return None
    return buf


def read_exact(sock, n):
    sock.settimeout(10.0)
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(min(65536, n - len(buf)))
        if not chunk:
            break
        buf += chunk
    return buf


def read_chunked(sock, initial=b""):
    sock.settimeout(10.0)
    data = initial
    body = b""
    while True:
        while b"\r\n" not in data:
            chunk = sock.recv(65536)
            if not chunk:
                return body
            data += chunk
        idx = data.index(b"\r\n")
        try:
            size = int(data[:idx].split(b";")[0], 16)
        except ValueError:
            return body
        data = data[idx + 2 :]
        if size == 0:
            return body
        while len(data) < size + 2:
            chunk = sock.recv(65536)
            if not chunk:
                return body
            data += chunk
        body += data[:size]
        data = data[size + 2 :]


def serve_files(conn, method, raw_path, headers, data_after_head):
    """GET /files/{name}."""
    name = raw_path[len("/files/") :]
    if not name or not NAME_RE.match(name):
        return response(404, b"not found", close=True), True
    info = file_info(name)
    if info is None:
        return response(404, b"not found", close=True), True
    path, size, mtime = info
    etag = etag_for(name, size, mtime)
    last_modified = http_date(mtime)
    # Conditional.
    inm = headers.get("if-none-match", "")
    ims = parse_http_date(headers.get("if-modified-since", ""))
    if inm and inm.strip() == etag:
        return response(304, extra=[("ETag", etag), ("Last-Modified", last_modified)]), False
    if ims is not None and mtime <= ims and not inm:
        return response(304, extra=[("ETag", etag), ("Last-Modified", last_modified)]), False
    # Range.
    status = 200
    extra = [("ETag", etag), ("Last-Modified", last_modified), ("Accept-Ranges", "bytes")]
    start, end = 0, size - 1
    range_hdr = headers.get("range", "")
    if range_hdr.startswith("bytes="):
        spec = range_hdr[len("bytes=") :]
        a, _, b = spec.partition("-")
        if a == "":
            try:
                n = int(b)
            except ValueError:
                n = 0
            if n <= 0:
                return response(416, extra=[("Content-Range", f"bytes */{size}")]), False
            start, end = max(0, size - n), size - 1
        else:
            try:
                start = int(a)
                end = int(b) if b else size - 1
            except ValueError:
                return response(400, b"bad range", close=True), True
            end = min(end, size - 1)
        if start >= size or start > end:
            return response(416, extra=[("Content-Range", f"bytes */{size}")]), False
        status = 206
        extra.append(("Content-Range", f"bytes {start}-{end}/{size}"))
    with open(path, "rb") as f:
        f.seek(start)
        body = f.read(end - start + 1)
    # gzip negotiation (identity for ranges).
    if status == 200 and len(body) > GZIP_MIN and "gzip" in headers.get("accept-encoding", "").lower():
        body = gzip.compress(body)
        extra.append(("Content-Encoding", "gzip"))
    return response(status, body, extra=extra), False


def handle_request(conn, head_bytes, data_after_head):
    text = head_bytes.split(b"\r\n\r\n", 1)[0].decode("latin-1")
    lines = text.split("\r\n")
    parts = lines[0].split(" ")
    if len(parts) != 3 or not parts[2].startswith("HTTP/1."):
        return response(400, b"bad request", close=True), True, True
    method, target, _version = parts[0], parts[1], parts[2]
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    close_after = headers.get("connection", "").lower() == "close"
    # Path traversal: reject anything that leaves the store.
    decoded = unquote(target)
    if ".." in decoded or decoded.startswith("//"):
        return response(404, b"not found", close=True), True, True
    path = decoded.split("?", 1)[0]
    text_plain = [("Content-Type", "text/plain")]
    if method == "GET" and path == "/":
        return response(200, b"sdb-http ok\n", extra=text_plain, close=close_after), close_after, False
    if method == "GET" and path.startswith("/echo/"):
        token = unquote(path[len("/echo/") :])
        return response(
            200, token.encode("latin-1", "replace"), extra=text_plain, close=close_after
        ), close_after, False
    if method == "GET" and path == "/user-agent":
        ua = headers.get("user-agent", "")
        return response(
            200, ua.encode("latin-1", "replace"), extra=text_plain, close=close_after
        ), close_after, False
    if method == "GET" and path.startswith("/files/"):
        resp, close2 = serve_files(conn, method, path, headers, data_after_head)
        return resp, close_after or close2, False
    if method == "POST" and path.startswith("/files/"):
        name = path[len("/files/") :]
        if not name or not NAME_RE.match(name):
            return response(400, b"bad name", close=True), True, True
        cl = headers.get("content-length")
        cl_n = int(cl) if cl and cl.isdigit() else None
        if cl_n is not None and cl_n > UPLOAD_CAP:
            return response(413, b"too large", close=True), True, True
        if headers.get("expect", "").lower() == "100-continue":
            try:
                conn.sendall(b"HTTP/1.1 100 Continue\r\n\r\n")
            except OSError:
                return b"", True, True
        if headers.get("transfer-encoding", "").lower() == "chunked":
            body = read_chunked(conn, data_after_head)
        elif cl_n is not None:
            body = data_after_head[:cl_n]
            if len(body) < cl_n:
                body += read_exact(conn, cl_n - len(body))
        else:
            body = data_after_head
        with open(os.path.join(FILES, name), "wb") as f:
            f.write(body)
        return response(201, b"", close=close_after), close_after, False
    if method == "GET" and path == "/status":
        return response(200, b"ok\n", close=close_after), close_after, False
    if method not in ("GET", "POST", "HEAD"):
        return response(400, b"unsupported method", close=True), True, True
    return response(404, b"not found", close=close_after), close_after, False


def handle_client(conn):
    try:
        while True:
            head = read_head(conn)
            if head is None:
                return
            if head == b"TOO_BIG":
                conn.sendall(response(431, b"too large", close=True))
                return
            head_end = head.index(b"\r\n\r\n") + 4
            data_after_head = head[head_end:]
            resp, close_after, fatal = handle_request(conn, head, data_after_head)
            if resp:
                conn.sendall(resp)
            if close_after or fatal:
                return
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", PORT))
    srv.listen(128)
    print(f"sdb-http listening on {PORT} store={FILES}", flush=True)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle_client, args=(conn,), daemon=True).start()


main()
"#;

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

    /// Write the sdb-http reference server into `dir`, listening on `port`.
    fn write_http_mock(dir: &Path, port: u16) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("http_server.py"), HTTP_MOCK_SERVER_PY).unwrap();
        let start = format!("#!/bin/bash\nexec python3 -u /workspace/http_server.py {port}\n");
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

    #[test]
    fn test_candidate_serves_http_probe() {
        // A serving stub answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
            }
        });
        assert!(
            candidate_serves_http(port),
            "a serving stub must read as ready"
        );
        handle.join().unwrap();

        // A closed port is not ready.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        assert!(
            !candidate_serves_http(closed_port),
            "a closed port is not ready"
        );

        // A socket that accepts but never answers (slow startup) must time
        // out rather than hang the readiness probe.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let silent_port = silent.local_addr().unwrap().port();
        let _guard = std::thread::spawn(move || {
            let _ = silent.accept();
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
        let started = std::time::Instant::now();
        assert!(
            !candidate_serves_http(silent_port),
            "a silent accept is not ready"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
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
            &[],
            &[],
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

    /// sdb-edge FULL-ladder live proof (eval-only, no model): the
    /// reference proxy (routing, retry, health surface, cache, limits)
    /// driven through the real pipeline with acceptance [1,2,3,4,6,7,8,9]
    /// — includes the docker-pause failover stages, the cache restart with
    /// SDB_PROXY_CACHE_TTL, and the 128MB-capped limits stage.
    #[tokio::test]
    async fn test_pipeline_eval_only_proxy_full_ladder_live() {
        let worker = 99u8;
        let port = crate::web::server::worker_port(TaskType::Proxy.default_port(), worker);
        let dir = covtest_dir("edgefull");
        let workdir = dir.join("workspace");
        write_edge_mock(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-edge full ladder fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "edgefull");
        config.task = TaskType::Proxy;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-edge".to_string(),
            stage: 6,
            prompt_path,
            continuity: true,
            accepted_stages: vec![1, 2, 3, 4, 6, 7, 8, 9],
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
            .expect("sdb-edge full ladder must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        for needle in [
            "Stage 1: Basic routing (200 + marker) -> PASS",
            "Stage 2: Round-robin across all 3 upstreams -> PASS",
            "Stage 3: Valid HTTP/1.1 framing + keep-alive -> PASS",
            "Stage 4: Failover (one upstream down) -> PASS",
            "Stage 6: Fast failover under a hung upstream -> PASS",
            "Stage 7: Active health checks + /status -> PASS",
            "Stage 8: Response caching (TTL + X-Cache) -> PASS",
            "Stage 9: Limits and hostile input -> PASS",
        ] {
            assert!(logger.contains(needle), "missing {needle:?}");
        }
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (8, 8)
        );
        cleanup_covtest_network("edgefull");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-queue FULL-ladder live proof (eval-only, no model): the
    /// reference broker driven through the real pipeline with acceptance
    /// [1..9] — including the durability half (prepare -> SIGKILL ->
    /// restart -> verify) and the 128MB-capped hardening restart.
    #[tokio::test]
    async fn test_pipeline_eval_only_queue_full_ladder_live() {
        let worker = 92u8;
        let port = crate::web::server::worker_port(TaskType::Queue.default_port(), worker);
        let dir = covtest_dir("queuelive");
        let workdir = dir.join("workspace");
        write_queue_mock(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-queue full ladder fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "queuelive");
        config.task = TaskType::Queue;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-queue".to_string(),
            stage: 8,
            prompt_path,
            continuity: true,
            accepted_stages: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
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
            .expect("sdb-queue full ladder must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        for needle in [
            "Stage 1: ENQ/LEASE/ACK round trip (FIFO) -> PASS",
            "Stage 2: Visibility leases (expiry + ownership) -> PASS",
            "Stage 3: NACK and redelivery (delayed + attempts) -> PASS",
            "Stage 4: Retries exhaust -> dead-letter queue -> PASS",
            "Stage 5: Durability across kill -9 -> PASS",
            "Stage 6: No duplicate delivery under contention -> PASS",
            "Stage 7: Observability (STATS counters) -> PASS",
            "Stage 8: Delayed delivery + DLQ redrive -> PASS",
            "Stage 9: Limits and hostile input -> PASS",
        ] {
            assert!(logger.contains(needle), "missing {needle:?}");
        }
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (9, 9)
        );
        cleanup_covtest_network("queuelive");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-http FULL-ladder live proof (eval-only, no model): the
    /// reference file server driven through the real pipeline with
    /// acceptance [1..10] — including the 128MB-capped restart that runs
    /// stages 9/10 (uploads + hardening).
    #[tokio::test]
    async fn test_pipeline_eval_only_http_full_ladder_live() {
        let worker = 91u8;
        let port = crate::web::server::worker_port(TaskType::Http.default_port(), worker);
        let dir = covtest_dir("httplive");
        let workdir = dir.join("workspace");
        write_http_mock(&workdir, port);
        let prompt_path = dir.join("ticket_prompt.md");
        fs::write(&prompt_path, "sdb-http full ladder fixture").unwrap();

        let mut config = covtest_config(workdir.clone(), worker, "httplive");
        config.task = TaskType::Http;
        config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
            campaign: "covtest-sdb-http".to_string(),
            stage: 7,
            prompt_path,
            continuity: true,
            accepted_stages: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
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
            .expect("sdb-http full ladder must complete");
        assert!(
            !out.infra_error,
            "unexpected infra error: {:?}",
            out.manifest.status
        );
        for needle in [
            "Stage 1: GET / (Root 200 OK + keep-alive) -> PASS",
            "Stage 2: 404 Not Found Handling -> PASS",
            "Stage 3: GET /echo/{str} -> PASS",
            "Stage 4: GET /user-agent Header Echo -> PASS",
            "Stage 5: POST & GET /files/{filename} -> PASS",
            "Stage 6: Range requests (206 + Content-Range) -> PASS",
            "Stage 7: Conditional GET (ETag / Last-Modified -> 304) -> PASS",
            "Stage 8: gzip content negotiation -> PASS",
            "Stage 9: Chunked uploads, 100-continue, size limits -> PASS",
            "Stage 10: Hardening (traversal, limits, slow-loris) -> PASS",
        ] {
            assert!(logger.contains(needle), "missing {needle:?}");
        }
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (10, 10)
        );
        cleanup_covtest_network("httplive");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stages 0-3 live proof (eval-only, no model): the Python
    /// sqlite3 reference adapter (`verifier::sql::ref_server`) driven
    /// through the real pipeline with acceptance [0, 1, 2, 3]. All four
    /// stage scripts run against one long-lived server, which also proves
    /// the seed-namespaced schemas cannot collide.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stages_0_3_live() {
        let worker = 93u8;
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
            assert!(logger.contains(needle), "missing {needle:?}");
        }
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (4, 4)
        );
        // The harness-owned contract lands in the prepared workspace.
        assert_eq!(
            fs::read_to_string(workdir.join("SPEC.md")).unwrap(),
            "# SPEC.md fixture\n"
        );
        cleanup_covtest_network("sqllive");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 4 live proof (eval-only, no model): the reference
    /// passes the cumulative suite through stage 4 — the differential
    /// script plus the persistence scenario, which restarts the candidate
    /// container on the same harness-owned store — and leaves a sentinel
    /// for later tickets.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage4_persistence_live() {
        let worker = 95u8;
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
            assert!(logger.contains(needle), "missing {needle:?}");
        }
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (5, 5)
        );
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
        let worker = 97u8;
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (6, 6)
        );
        cleanup_covtest_network("sqlcrash");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stage 5's lite variant (proposal: acked rows survive kill -9; no
    /// torn tail or mid-statement atomicity) runs the narrowed ladder: the
    /// reference passes and the bulk/torn checks do not run.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage5_lite_live() {
        let worker = 98u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (6, 6)
        );
        cleanup_covtest_network("sqlcrashlite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A stage-6 ticket after the model won stage 5 on the lite variant:
    /// the suite keeps grading stage 5 lite (the full crash ladder would
    /// demand work ticket 5 never asked for) while stage 6 runs in full.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage6_after_lite_stage5() {
        let worker = 85u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
        for needle in [
            "Stage 5: Crash safety -> PASS",
            "Stage 6: Transactions -> PASS",
            "[CRASH] lite variant: acked rows and restarts only",
            "[TXN] a kill with an open transaction discarded it",
        ] {
            assert!(logger.contains(needle), "missing {needle:?}");
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (7, 7)
        );
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
        let worker = 80u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (6, 6)
        );
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
        let worker = 90u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (15, 15)
        );
        cleanup_covtest_network("sqlobs");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 15's gate needs `./test.sh`, a coverage line and stage-7/8
    /// commits; stage 16's acceptance includes ticket 15, so both live
    /// proofs install the same fixture. `sql_server.py` stays byte-identical
    /// to the harness reference (the anti-cheat check recognizes it).
    fn install_tests_ci_fixture(workdir: &std::path::Path, port: u16) {
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
        std::fs::write(workdir.join("coverage.txt"), "coverage: 85.0%\n").unwrap();
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
        let worker = 86u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (16, 16)
        );
        cleanup_covtest_network("sqltests");
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
        let worker = 78u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (17, 17)
        );
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
        let worker = 77u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (18, 18)
        );
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
        let worker = 76u8;
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
        let out = BenchmarkPipeline::execute(config, logger.clone(), None)
            .await
            .expect("the stage-18 fixture run must complete");
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (19, 19)
        );
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
        let worker = 75u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (20, 20)
        );
        cleanup_covtest_network("sqlrepl");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 19 lite live proof: two nodes, kill the primary, manual
    /// `PROMOTE`, then the cumulative suite on the promoted node.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage19_replication_lite_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 74u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (20, 20)
        );
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (21, 21)
        );
        cleanup_covtest_network("sqlpg");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Ticket 20 lite live proof: the simple-query protocol only (no
    /// extended query), plus the psql cross-check.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage20_pgwire_lite_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 72u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (21, 21)
        );
        cleanup_covtest_network("sqlpglite");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_pipeline_eval_only_sql_stage13_schema_change_live() {
        let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let worker = 89u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (14, 14)
        );
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
        let worker = 88u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (12, 12)
        );
        cleanup_covtest_network("sqlidx");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 10 live proof (eval-only, no model): the reference
    /// passes the subquery suite under Docker — scalar subqueries,
    /// `[NOT] IN (subquery)`/`IN (list)`, correlated `[NOT] EXISTS` and
    /// `INSERT ... SELECT` — plus everything 0-9 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage10_subqueries_live() {
        let worker = 79u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (11, 11)
        );
        cleanup_covtest_network("sqlsub");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 9 live proof (eval-only, no model): the reference
    /// passes the constraint suite under Docker — constrained DDL, key
    /// auto-assignment, defaults, UNIQUE/NOT NULL and foreign keys
    /// (including the violation probes) — plus everything 0-8 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage9_constraints_live() {
        let worker = 81u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (10, 10)
        );
        cleanup_covtest_network("sqlcons");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 8 live proof (eval-only, no model): the reference
    /// passes the join suite under Docker — comma/`INNER`/`LEFT` joins with
    /// `ON`, qualified references, `SELECT *` over a join, the ambiguity
    /// probe — plus everything 0-7 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage8_joins_live() {
        let worker = 83u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (9, 9)
        );
        cleanup_covtest_network("sqljoin");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stage 11's lite variant through the pipeline: the attempt carries
    /// `index-equality-only`, so the perf gate drops its range round and
    /// only the equality point lookups are gated.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage11_lite_live() {
        let worker = 87u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (12, 12)
        );
        cleanup_covtest_network("sqlidxlite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stage 8's lite variant through the pipeline: the attempt carries
    /// `join-inner-only`, so the graded suite keeps comma/inner joins and
    /// never runs a `LEFT JOIN`.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage8_lite_live() {
        let worker = 82u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
        for needle in ["Stage 8: Joins -> PASS", "Stage 7: Aggregates -> PASS"] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (9, 9)
        );
        cleanup_covtest_network("sqljoinlite");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 7 live proof (eval-only, no model): the reference
    /// passes the aggregate suite under Docker — `COUNT`/`SUM`/`MIN`/`MAX`,
    /// grouped queries with `HAVING`, the empty-group contract and the
    /// aggregate error probes — plus everything 0-6 again.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage7_aggregates_live() {
        let worker = 84u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (8, 8)
        );
        cleanup_covtest_network("sqlagg");
        let _ = fs::remove_dir_all(&dir);
    }

    /// sdb-sql stage 6 live proof (eval-only, no model): the reference
    /// passes the transactions suite under Docker — cross-connection
    /// isolation (two connections, uncommitted rows invisible) and a kill
    /// -9 with a transaction open.
    #[tokio::test]
    async fn test_pipeline_eval_only_sql_stage6_transactions_live() {
        let worker = 99u8;
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
        assert!(!out.infra_error, "{:?}", out.manifest.status);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (7, 7)
        );
        cleanup_covtest_network("sqltxn");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A sentinel whose data is gone fails the stage that wrote it
    /// (regression class `upgrade`): the check runs before the suite and
    /// the stage result is overridden even though the suite itself passes.
    #[tokio::test]
    async fn test_pipeline_sql_sentinel_drift_fails_the_writing_stage() {
        let worker = 96u8;
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
        assert!(!out.infra_error);
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
        assert_eq!(
            (out.manifest.passed_stages, out.manifest.total_stages),
            (4, 5)
        );
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
        let worker = 94u8;
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
            &[],
            &[],
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
            &[],
            &[],
        )
        .await;
        assert_eq!(outcome2.pass_rate, 0.0);
        assert!(outcome2.stage_results.is_empty());
        assert!(rec2.contains("did not produce runnable code"));
        let _ = fs::remove_dir_all(&workdir);
    }
}
