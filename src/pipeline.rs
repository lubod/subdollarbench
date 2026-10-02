use anyhow::{anyhow, Result};
use chrono::Utc;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::bench::BenchmarkRunner;
use crate::config::{get_repo_root, TaskType};
use crate::cost::ModelPricing;
use crate::report::archive::{RunArchiver, RunManifest, RunTokenUsage};
use crate::report::judge::RunJudge;
use crate::report::leaderboard::{BenchmarkRunResult, LeaderboardManager};
use crate::report::portal::PortalExporter;
use crate::sandbox::docker::{SandboxManager, PROXY_UPSTREAM_PORTS};

/// Upstream pool ports for a worker (base pool + worker*100 offset).
fn worker_upstream_ports(worker: u8) -> [u16; 3] {
    [
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[0], worker),
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[1], worker),
        crate::web::server::worker_port(PROXY_UPSTREAM_PORTS[2], worker),
    ]
}
use crate::sandbox::omp::{AgentExecutionLimits, AgentRunSpec, OmpRunner, OmpSessionStats};
use crate::verifier::binproto::BinprotoVerifier;
use crate::verifier::compliance::ComplianceChecker;
use crate::verifier::dns::DnsVerifier;
use crate::verifier::http::HttpVerifier;
use crate::verifier::pdf::PdfVerifier;
use crate::verifier::proxy::ProxyVerifier;
use crate::verifier::pubsub::PubsubVerifier;
use crate::verifier::redis::RedisVerifier;
use crate::verifier::wal::WalVerifier;

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

/// Rewrite candidate-local port mentions for a parallel worker, leaving
/// in-network reference aliases untouched.
/// - Skipped entirely: lines containing ref-redis/ref-http/ref-dns/upstream
///   (fixed in-network names/ports, same for every worker).
/// - Rewritten with word boundaries: localhost:PORT, 0.0.0.0:PORT, -p PORT,
///   port PORT, :PORT, exposes PORT. Bare-substring replace would corrupt
///   coincidental digit runs (e.g. 16379) and ref lines.
pub fn template_ports_for_worker(content: &str, base_port: u16, worker: u8) -> String {
    if worker == 0 {
        return content.to_string();
    }
    let base = base_port.to_string();
    let off = crate::web::server::worker_port(base_port, worker).to_string();
    // Word-boundary pattern around the bare port number.
    let word_pat = ::regex::Regex::new(&format!(r"\b{}\b", ::regex::escape(&base))).unwrap();
    content
        .lines()
        .map(|line| {
            let low = line.to_lowercase();
            if low.contains("ref-redis")
                || low.contains("ref-http")
                || low.contains("ref-dns")
                || low.contains("upstream")
            {
                return line.to_string();
            }
            word_pat.replace_all(line, off.as_str()).to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
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

pub struct BenchmarkPipeline;

impl BenchmarkPipeline {
    pub async fn execute(
        config: BenchmarkConfig,
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

        // 1b. Create the run's private Docker network (reference aliases + agent attach)
        match sandbox.ensure_network() {
            Ok(()) => logger.log(&format!(
                "[SETUP] Private run network '{}' ready (reference aliases: ref-redis / ref-http / ref-dns)",
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
            logger.log(&format!(
                "[ACCOUNT] OpenRouter initial key spend: ${:.4} USD",
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
            TaskType::Dns => {
                if let Err(e) = sandbox
                    .start_reference_dns(crate::web::server::worker_port(5354, config.worker))
                {
                    logger.log(&format!(
                        "[SETUP] Warning: reference DNS unavailable: {}",
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
        // (ref-redis/ref-http/ref-dns/upstreamN — fixed ports inside the run
        // network, identical for every worker). Rewrite only candidate-local
        // references (localhost, 0.0.0.0, -p, bare port mentions) with word
        // boundaries so coincidental digit runs never match.
        let prompt_content = if config.worker == 0 {
            prompt_content
        } else {
            template_ports_for_worker(&prompt_content, config.task.default_port(), config.worker)
        };

        // 5. Run OMP Agent (unless eval_only)
        let mut agent_failed = false;
        let (omp_stats, prompt_tokens, cached_tokens, completion_tokens) = if !config.eval_only {
            logger.log(&format!(
                "[OMP] Spawning OMP agent inside isolated Docker sandbox with model '{}' (Effort: {})...",
                config.model, config.effort
            ));

            let limits = AgentExecutionLimits {
                max_turns: if config.max_turns > 0 {
                    Some(config.max_turns)
                } else {
                    None
                },
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

            let logger_sub = logger.clone();
            let model_c = config.model.clone();
            let prompt_c = prompt_content.clone();
            let work_path_buf = config.workdir.clone();
            let api_key_c = config.api_key.clone();
            let effort_c = config.effort.clone();
            let run_id_c = run_id.clone();
            let net_name = sandbox.network_name();

            let cancel_flag_c = cancel_flag.clone();
            let cancel_flag_watcher = cancel_flag.clone();
            let sandbox_clone = SandboxManager::with_id(&run_id_c);

            let mut blocking_handle = tokio::task::spawn_blocking(move || {
                let spec = AgentRunSpec {
                    model: &model_c,
                    prompt: &prompt_c,
                    workdir: &work_path_buf,
                    api_key: api_key_c.as_deref(),
                    limits,
                    effort: Some(&effort_c),
                    network: Some(net_name.as_str()),
                    task: Some(config.task),
                    run_id: Some(&run_id_c),
                    cancel_flag: cancel_flag_c,
                };
                OmpRunner::run_agent_with_logger(spec, move |line| {
                    logger_sub.log(&line);
                })
            });

            let omp_res: Result<OmpSessionStats> = tokio::select! {
                res = &mut blocking_handle => match res {
                    Ok(inner) => inner,
                    Err(e) => Err(anyhow!("OMP task execution error: {}", e)),
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
            };

            match omp_res {
                Ok(stats) => {
                    logger.log(&format!(
                        "[OMP] Finished! Tokens: prompt={}, cached={}, completion={}",
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
                    logger.log(&format!("[OMP ERROR] Agent execution failed: {}", e));
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
            logger.log("[OMP] Skipped (--eval-only)");
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

        // 6. Anti-cheat compliance verification
        logger.log("[VERIFY] Checking anti-cheat compliance (no prebuilt frameworks/daemons)...");
        let mut disqualified = false;
        let mut disqualification_reason = None;
        if let Err(violation) = ComplianceChecker::check_no_frameworks(&config.workdir) {
            logger.log(&format!("[ANTI-CHEAT VIOLATION] {}", violation));
            disqualified = true;
            disqualification_reason = Some(violation);
        } else {
            logger.log("[VERIFY] Anti-cheat compliance passed.");
        }

        // 7. Start candidate and run test suite.
        // Worker offset keeps parallel cells on disjoint ports
        // (worker 0 = legacy ports, byte-identical to single mode).
        let port = crate::web::server::worker_port(config.task.default_port(), config.worker);
        // An Err here is an I/O or Docker fault, not a statement about the
        // candidate. Collapsing it to `false` made the run log "no runnable
        // start.sh or Dockerfile found" - a fabricated cause that reads as a
        // model failure - and then score it 0%.
        let (has_runnable, candidate_check_error) =
            match sandbox.ensure_runnable_candidate(&config.workdir) {
                Ok(v) => (v, None),
                Err(e) => (false, Some(e.to_string())),
            };

        let mut throughput = None;
        let mut reference_throughput = None;
        // Verifier-measured rate (wal/proxy/pubsub soak); step 8 prefers it.
        let mut extra_throughput: Option<f64> = None;
        let mut baseline_source: Option<String> = None;
        // Latched by whichever setup path hits a harness fault. Declared out
        // here because the two sites live in different arms of the expression
        // below and therefore do not share a scope.
        let mut infra_error = false;
        let (pass_rate, passed_stages, total_stages, stage_results) = if disqualified {
            let reason = disqualification_reason
                .unwrap_or_else(|| "Anti-cheat compliance violation".to_string());
            let stages = vec![crate::verifier::StageResult {
                stage: 0,
                name: "Anti-Cheat Compliance".to_string(),
                passed: false,
                error: Some(reason),
            }];
            (0.0, 0, config.task.total_stages(), stages)
        } else if !has_runnable {
            // A workspace-inspection Err is a harness fault, not a model that
            // produced nothing. Latched so the run is reported as a non-result
            // rather than a 0% trial.
            infra_error = candidate_check_error.is_some();
            if let Some(e) = &candidate_check_error {
                // Report the real fault. This is infrastructure, not a model
                // that failed to produce code.
                logger.log(&format!(
                    "[INFRA] Could not inspect the candidate workspace ({e}). \
                     This is a harness fault, not a model result."
                ));
            } else if agent_failed {
                logger
                    .log("[SETUP] Candidate did not produce runnable code. Skipping verification.");
            } else {
                logger.log("[SETUP] No runnable start.sh or Dockerfile found in ./workspace! Skipping verification.");
            }
            (0.0, 0, config.task.total_stages(), Vec::new())
        } else {
            // A candidate that will not start is an infrastructure fault, not
            // a model that scored zero: the error used to be discarded, and
            // every verifier then failed on transport.
            if let Err(e) = sandbox.start_candidate_in_docker(&config.workdir, port) {
                logger.log(&format!(
                    "[INFRA] Candidate container failed to start ({e}); \
                     verifiers would fail on transport, not on the candidate."
                ));
                // Latched below via `infra_error`; declared here so both paths
                // converge on one flag.
                infra_error = true;
            }

            // Proxy needs its upstream pool; DNS needs ref-dns. Both live
            // on the private run network, which the candidate only joins here
            // (default bridge has neither alias). Without this, DNS
            // candidates hang in libc resolution of `ref-dns` past the 1.5s
            // verifier timeout and fail stage 5 on transport, not logic.
            // Redis and Http join too: their throughput is now measured with
            // the bench client on this network, addressed by the `candidate`
            // alias. Without the attach the alias never exists and the
            // measurement fails silently. This also aligns the sandbox with the
            // redis prompt, which already tells the model that a reference
            // server is reachable at `ref-redis` on this network - previously
            // unreachable, because the candidate launched on the default bridge.
            if matches!(
                config.task,
                TaskType::Proxy | TaskType::Dns | TaskType::Redis | TaskType::Http
            ) {
                if let Err(e) = sandbox.connect_candidate_to_network() {
                    logger.log(&format!(
                        "[SETUP] Warning: candidate network attach failed: {}. Upstream dials may fail.",
                        e
                    ));
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }

            logger.log(&format!(
                "[SETUP] Waiting for server port {} to become ready (timeout: 25s)...",
                port
            ));
            let ready = if config.task == TaskType::Dns {
                let verifier = DnsVerifier::new(port, Some(&verifier_seed));
                verifier.wait_for_ready(25).await
            } else {
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

            let (pr, ps, ts, sr) = match config.task {
                TaskType::Redis => {
                    let verifier = RedisVerifier::new(port, Some(&verifier_seed));
                    let summary = verifier.run_all().await;
                    let mut stages = summary.stages.clone();
                    for s in &summary.stages {
                        let status_str = if s.passed { "PASS" } else { "FAIL" };
                        let err_str = s.error.as_deref().unwrap_or("");
                        logger.log(&format!(
                            "  Stage {}: {} -> {} {}",
                            s.stage, s.name, status_str, err_str
                        ));
                    }
                    // Stage 6: AOF crash recovery on full pass (mirrors the WAL flow).
                    // Denominator stays 6 either way: a skipped crash stage is
                    // recorded failed, never silently dropped.
                    if summary.pass_rate == 100.0 {
                        let fail6 = |e: String| crate::verifier::StageResult {
                            stage: 6,
                            name: "Stage 6: AOF crash recovery".to_string(),
                            passed: false,
                            error: Some(e),
                        };
                        match verifier.write_probe_key().await {
                            Ok(()) => {
                                logger.log("[BENCH] AOF probe written; SIGKILLing candidate...");
                                let st = match sandbox.kill_candidate() {
                                    Ok(()) => {
                                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
                                    st.name,
                                    status_str,
                                    st.error.as_deref().unwrap_or("")
                                ));
                                stages.push(st);
                            }
                            Err(e) => logger.log(&format!(
                                "  AOF probe write failed (skipping crash stage): {}",
                                e
                            )),
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
                    (pr, ps, ts, stages)
                }
                TaskType::Http => {
                    let verifier = HttpVerifier::new(port, Some(&verifier_seed));
                    let summary = verifier.run_all().await;
                    for s in &summary.stages {
                        let status_str = if s.passed { "PASS" } else { "FAIL" };
                        let err_str = s.error.as_deref().unwrap_or("");
                        logger.log(&format!(
                            "  Stage {}: {} -> {} {}",
                            s.stage, s.name, status_str, err_str
                        ));
                    }
                    (
                        summary.pass_rate,
                        summary.passed_count,
                        summary.total_stages,
                        summary.stages,
                    )
                }
                TaskType::Dns => {
                    let verifier = DnsVerifier::new(port, Some(&verifier_seed));
                    let summary = verifier.run_all().await;
                    for s in &summary.stages {
                        let status_str = if s.passed { "PASS" } else { "FAIL" };
                        let err_str = s.error.as_deref().unwrap_or("");
                        logger.log(&format!(
                            "  Stage {}: {} -> {} {}",
                            s.stage, s.name, status_str, err_str
                        ));
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
                                s.stage, s.name, status_str, err_str
                            ));
                        };
                    let fail = |n: u32, name: &str, err: String| crate::verifier::StageResult {
                        stage: n,
                        name: name.to_string(),
                        passed: false,
                        error: Some(err),
                    };
                    let verifier = WalVerifier::new(port, Some(&verifier_seed));
                    let basic = verifier.run_basic().await;
                    let mut stages = basic.stages.clone();
                    for s in &basic.stages {
                        log_stage(&logger, s);
                    }
                    // Seed crash state, SIGKILL, restart, verify.
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
                    // Proceed on majority quorum (fsync-storm losses stay visible:
                    // check_recovery demands every acked message back).
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
                    log_stage(&logger, &rec);
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
                    log_stage(&logger, &tp_stage);
                    stages.push(tp_stage);
                    let ps = stages.iter().filter(|s| s.passed).count() as u32;
                    let ts = stages.len() as u32;
                    let pr = ps as f64 / ts as f64 * 100.0;
                    (pr, ps, ts, stages)
                }
                TaskType::Pdf => {
                    let verifier = PdfVerifier::new(port, Some(&verifier_seed));
                    let summary = verifier.run_all().await;
                    for s in &summary.stages {
                        let status_str = if s.passed { "PASS" } else { "FAIL" };
                        let err_str = s.error.as_deref().unwrap_or("");
                        logger.log(&format!(
                            "  Stage {}: {} -> {} {}",
                            s.stage, s.name, status_str, err_str
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
                    if let Err(e) = sandbox.connect_candidate_to_network() {
                        logger.log(&format!(
                            "[SETUP] Warning: candidate network attach failed: {}. Upstream dials may fail.",
                            e
                        ));
                    } else {
                        // Alias propagation after connect.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    let log_stage =
                        |logger: &Arc<dyn PipelineLogger>, s: &crate::verifier::StageResult| {
                            let status_str = if s.passed { "PASS" } else { "FAIL" };
                            let err_str = s.error.as_deref().unwrap_or("");
                            logger.log(&format!(
                                "  Stage {}: {} -> {} {}",
                                s.stage, s.name, status_str, err_str
                            ));
                        };
                    let ups: Vec<(String, u16)> = (1u8..=3u8)
                        .map(|i| {
                            (
                                sandbox.upstream_name(i),
                                crate::web::server::worker_port(
                                    PROXY_UPSTREAM_PORTS[(i - 1) as usize],
                                    config.worker,
                                ),
                            )
                        })
                        .collect();
                    let verifier = ProxyVerifier::new(port, Some(&verifier_seed), ups);
                    let routing = verifier.run_routing().await;
                    let mut stages = routing.stages.clone();
                    for s in &routing.stages {
                        log_stage(&logger, s);
                    }
                    let fo = verifier.run_failover().await;
                    log_stage(&logger, &fo);
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
                    log_stage(&logger, &tp_stage);
                    stages.push(tp_stage);
                    let ps = stages.iter().filter(|s| s.passed).count() as u32;
                    let ts = stages.len() as u32;
                    let pr = ps as f64 / ts as f64 * 100.0;
                    (pr, ps, ts, stages)
                }
                TaskType::Pubsub => {
                    let verifier = PubsubVerifier::new(port, Some(&verifier_seed));
                    let summary = verifier.run_all().await;
                    for s in &summary.stages {
                        let status_str = if s.passed { "PASS" } else { "FAIL" };
                        let err_str = s.error.as_deref().unwrap_or("");
                        logger.log(&format!(
                            "  Stage {}: {} -> {} {}",
                            s.stage, s.name, status_str, err_str
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
                    let verifier = BinprotoVerifier::new(port, Some(&verifier_seed));
                    let summary = verifier.run_all().await;
                    for s in &summary.stages {
                        let status_str = if s.passed { "PASS" } else { "FAIL" };
                        let err_str = s.error.as_deref().unwrap_or("");
                        logger.log(&format!(
                            "  Stage {}: {} -> {} {}",
                            s.stage, s.name, status_str, err_str
                        ));
                    }
                    // Throughput probe (200-frame ECHO burst); unscored until
                    // a reference harness exists (same rule as pubsub soak).
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
            };

            // Container diagnostics if any failures
            if pr < 100.0 {
                let diag = sandbox.get_candidate_logs();
                logger.log("[DIAGNOSTICS] Candidate container runtime output:");
                for line in diag.lines() {
                    logger.log(&format!("  | {}", line));
                }
            }

            // 8. Load testing if 100% pass
            if pr == 100.0 {
                logger.log(
                    "[BENCH] 100% tests passed! Running live throughput load test on candidate...",
                );
                match config.task {
                    TaskType::Redis => {
                        if let Ok(tp) = BenchmarkRunner::run_redis_benchmark(
                            &sandbox.network_name(),
                            "candidate",
                            port,
                        ) {
                            logger.log(&format!(
                                "  Candidate Throughput (median of 3): {:.0} req/sec",
                                tp
                            ));
                            throughput = Some(tp);
                        }
                        if config.batch_reference.is_none() {
                            logger.log("[BENCH] Calibrating live official Redis baseline (redis:alpine on port 6380)...");
                            match BenchmarkRunner::run_redis_benchmark(
                                &sandbox.network_name(),
                                "ref-redis",
                                6379,
                            ) {
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
                        if let Ok(tp) = BenchmarkRunner::run_wrk_benchmark(
                            &sandbox.network_name(),
                            "candidate",
                            port,
                        ) {
                            logger.log(&format!(
                                "  Candidate Throughput (median of 3): {:.0} req/sec",
                                tp
                            ));
                            throughput = Some(tp);
                        }
                        if config.batch_reference.is_none() {
                            logger.log("[BENCH] Calibrating live official Nginx baseline (nginx:alpine on port 8081)...");
                            match BenchmarkRunner::run_wrk_benchmark(
                                &sandbox.network_name(),
                                "ref-http",
                                80,
                            ) {
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
                            logger.log(&format!(
                                "  Candidate Throughput (verifier soak, median-grade): {:.0} msgs/sec",
                                tp
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
                        if let Ok(tp) = BenchmarkRunner::run_wrk_benchmark(
                            &sandbox.network_name(),
                            "candidate",
                            port,
                        ) {
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
                            match BenchmarkRunner::run_wrk_benchmark(
                                &sandbox.network_name(),
                                "ref-http",
                                80,
                            ) {
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
                    TaskType::Dns => {
                        let verifier = DnsVerifier::new(port, Some(&verifier_seed));
                        if let Some(tp) = verifier.measure_throughput().await {
                            logger.log(&format!("  Candidate Throughput: {:.0} queries/sec", tp));
                            throughput = Some(tp);
                        }
                        if config.batch_reference.is_none() {
                            // Live CoreDNS baseline on an ephemeral port: same
                            // probe, same host load window as the candidate.
                            logger.log(
                                "[BENCH] Calibrating live CoreDNS baseline (pinned image, ephemeral port)...",
                            );
                            if let Some(ref_tp) =
                                crate::bench::measure_dns_reference(&verifier_seed).await
                            {
                                logger.log(&format!(
                                    "  Official Reference CoreDNS: {:.0} queries/sec",
                                    ref_tp
                                ));
                                reference_throughput = Some(ref_tp);
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

            (pr, ps, ts, sr)
        };

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

        let cost_usd = live_spend_delta.unwrap_or(breakdown.total_cost_usd);
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

        let manifest = RunManifest {
            run_id: run_id.clone(),
            model: config.model.clone(),
            task: config.task.to_string(),
            // `infra_error` first: a harness fault that also scored 0% must
            // not be archived as `failed_tests`, which is a model verdict.
            // Ordering matters - a disqualified run is a model-side result and
            // keeps its own status even if setup also faulted.
            status: if infra_error && !disqualified {
                "infra_error".to_string()
            } else if disqualified {
                "disqualified_cheat".to_string()
            } else if pass_rate == 100.0 {
                if agent_failed {
                    "agent_killed".to_string()
                } else {
                    "completed".to_string()
                }
            } else if agent_failed {
                "agent_failed".to_string()
            } else {
                "failed_tests".to_string()
            },
            language: lang.clone(),
            effort: Some(config.effort.clone()),
            started_at,
            completed_at: completed_at.clone(),
            duration_seconds,
            turns: Some(omp_stats.steps_taken),
            pass_rate,
            passed_stages,
            total_stages,
            stages: stage_results,
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
            effective_cost_usd: Some(effective_cost_usd),
            savings_percent: breakdown.savings_percent,
            efficiency_score,
            throughput_score,
            files: scanned_files,
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
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            trial_pass_rates: Vec::new(),
            is_published: None,
            method_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };

        let benchmark_result = BenchmarkRunResult {
            id: run_id,
            model: config.model.clone(),
            task: config.task.to_string(),
            language: lang,
            effort: Some(config.effort.clone()),
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
            effective_cost_usd: Some(effective_cost_usd),
            duration_seconds: Some(duration_seconds),
            turns: Some(omp_stats.steps_taken),
            savings_percent: breakdown.savings_percent,
            efficiency_score,
            throughput_score,
            timestamp: completed_at,
            method_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            subruns: Vec::new(),
            total_trials: None,
            rules_version: Some(crate::report::rules_fingerprint()),
            pass_at_1: None,
            pass_at_k: None,
            avg_pass_rate: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            frontier_fraction: None,
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

    #[test]
    fn test_dns_candidate_joins_run_network() {
        // Regression: the pre-verify attach must cover DNS (ref-dns lives on
        // the private run network). If narrowed back to Proxy-only, stage 5
        // fails on libc resolution timeout instead of candidate logic.
        let src = include_str!("pipeline.rs");
        assert!(
            src.contains("TaskType::Proxy || config.task == TaskType::Dns"),
            "DNS candidate must join the run network alongside Proxy"
        );
    }

    struct DummyLogger;
    impl PipelineLogger for DummyLogger {
        fn log(&self, _msg: &str) {}
    }

    #[test]
    fn test_template_ports_for_worker() {
        // Ref-alias lines untouched at any worker.
        let redis_ref = "redis-cli -h ref-redis -p 6379 <cmd>";
        assert_eq!(template_ports_for_worker(redis_ref, 6379, 2), redis_ref);
        let dns_ref = "forward to `ref-dns`, port `53`";
        assert_eq!(template_ports_for_worker(dns_ref, 5353, 1), dns_ref);
        let ups = "dial upstream1:80, mirrors 127.0.0.1:8101";
        assert_eq!(template_ports_for_worker(ups, 8091, 3), ups);
        // Candidate-local lines rewritten with word boundaries.
        let own = "listening on `0.0.0.0:6379`, test `curl localhost:6379`, `-p 6379`, port 6379";
        let got = template_ports_for_worker(own, 6379, 1);
        assert!(got.contains("6479"), "{got}");
        assert!(!got.contains("6379"), "{got}");
        // No coincidental digit-run corruption.
        let tricky = "id 16379 and 63790 stay";
        assert_eq!(template_ports_for_worker(tricky, 6379, 1), tricky);
        // Worker 0 verbatim.
        assert_eq!(template_ports_for_worker(own, 6379, 0), own);
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
}
