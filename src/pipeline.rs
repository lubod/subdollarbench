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
fn initial_turn_cap(max_turns: u32, feedback_rounds: u32) -> Option<u32> {
    if max_turns == 0 {
        None
    } else if feedback_rounds > 0 {
        Some(std::cmp::min(max_turns, INITIAL_TURN_CAP_WITH_FEEDBACK))
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

    // Eight params (task + workdir + sandbox + port + seed + agent flag +
    // worker + logger): the verifier fans out per task and needs them all;
    // bundling into a struct is churn without a second caller.
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
                (0.0, 0, task.total_stages(), stages)
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
                (0.0, 0, task.total_stages(), Vec::new())
            } else {
                if let Err(e) = sandbox.start_candidate_in_docker(workdir, port) {
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
                        let verifier = RedisVerifier::new(port, Some(verifier_seed));
                        let summary = verifier.run_all().await;
                        let mut stages = summary.stages.clone();
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
                        if summary.pass_rate == 100.0 {
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
                        let ps = stages.iter().filter(|s| s.passed).count() as u32;
                        let ts = stages.len() as u32;
                        let pr = ps as f64 / ts as f64 * 100.0;
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
                        let routing = verifier.run_routing().await;
                        let mut stages = routing.stages.clone();
                        for s in &routing.stages {
                            log_stage(logger, s);
                        }
                        let fo = verifier.run_failover().await;
                        log_stage(logger, &fo);
                        stages.push(fo);
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
        if !config.eval_only {
            if config.workdir.exists() {
                let _ = fs::remove_dir_all(&config.workdir);
            }
            fs::create_dir_all(&config.workdir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&config.workdir, fs::Permissions::from_mode(0o777));
            }
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

        // 4. Read prompt
        let prompt_file = get_repo_root()
            .join("tasks")
            .join(config.task.to_string())
            .join("prompt.md");
        let prompt_content = fs::read_to_string(&prompt_file)
            .map_err(|_| anyhow!("Could not read prompt file: {}", prompt_file.display()))?;
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

                let initial_turn_cap = initial_turn_cap(config.max_turns, config.feedback_rounds);

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

        let pass_rate = v_outcome.pass_rate;
        let passed_stages = v_outcome.passed_stages;
        let total_stages = v_outcome.total_stages;
        let stage_results = v_outcome.stage_results;
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
                    if let Ok(tp) = BenchmarkRunner::run_redis_benchmark_async(
                        &sandbox.network_name(),
                        "candidate",
                        port,
                    )
                    .await
                    {
                        logger.log(&format!(
                            "  Candidate Throughput (median of 3): {:.0} req/sec",
                            tp
                        ));
                        throughput = Some(tp);
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
                            "  Candidate Throughput (median of 3): {:.0} req/sec",
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
                            "  Candidate Throughput (median of 3): {:.0} req/sec",
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
        } else if agent_failed {
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

    #[test]
    fn test_initial_turn_cap_checkpoints_only_with_feedback() {
        // No cap configured: unlimited either way.
        assert_eq!(initial_turn_cap(0, 0), None);
        assert_eq!(initial_turn_cap(0, 2), None);
        // Feedback on: checkpoint at 50 (or the smaller max_turns).
        assert_eq!(initial_turn_cap(100, 2), Some(50));
        assert_eq!(initial_turn_cap(30, 1), Some(30));
        // Feedback off: full budget up front.
        assert_eq!(initial_turn_cap(100, 0), Some(100));
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
            worker: 0,
            harness: Harness::Omp,
            feedback_rounds: 0,
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
}
