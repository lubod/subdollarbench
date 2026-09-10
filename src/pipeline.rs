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
use crate::report::leaderboard::{BenchmarkRunResult, LeaderboardManager};
use crate::report::portal::PortalExporter;
use crate::report::judge::RunJudge;
use crate::sandbox::docker::SandboxManager;
use crate::sandbox::omp::{AgentExecutionLimits, AgentRunSpec, OmpRunner, OmpSessionStats};
use crate::verifier::compliance::ComplianceChecker;
use crate::verifier::dns::DnsVerifier;
use crate::verifier::http::HttpVerifier;
use crate::verifier::pdf::PdfVerifier;
use crate::verifier::redis::RedisVerifier;

pub trait PipelineLogger: Send + Sync {
    fn log(&self, msg: &str);
}

struct BufferingPipelineLogger {
    inner: Arc<dyn PipelineLogger>,
    buffer: std::sync::Mutex<Vec<String>>,
}

impl BufferingPipelineLogger {
    fn new(inner: Arc<dyn PipelineLogger>) -> Self {
        Self {
            inner,
            buffer: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn get_logs(&self) -> String {
        self.buffer.lock().unwrap().join("\n")
    }
}

impl PipelineLogger for BufferingPipelineLogger {
    fn log(&self, msg: &str) {
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
    pub max_turns: u32,
    pub budget_usd: f64,
    pub timeout_min: u64,
    pub api_key: Option<String>,
    pub workdir: PathBuf,
    pub eval_only: bool,
    pub run_id: Option<String>,
    pub save_results: bool,
    pub review_model: Option<String>,
}

#[derive(Debug)]
pub struct PipelineOutput {
    pub manifest: RunManifest,
    pub benchmark_result: BenchmarkRunResult,
    pub agent_failed: bool,
    pub disqualified: bool,
}

pub struct BenchmarkPipeline;

impl BenchmarkPipeline {
    pub async fn execute(
        config: BenchmarkConfig,
        logger: Arc<dyn PipelineLogger>,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<PipelineOutput> {
        let buffering_logger = Arc::new(BufferingPipelineLogger::new(logger));
        let logger: Arc<dyn PipelineLogger> = buffering_logger.clone();

        let start_time = Instant::now();
        let started_at = Utc::now().to_rfc3339();

        let run_id = config.run_id.clone().unwrap_or_else(|| {
            format!(
                "{}_{}_{}",
                config.task,
                config.model.replace(['/', ':'], "_"),
                Utc::now().format("%Y%m%d_%H%M%S")
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
            Err(e) => logger.log(&format!(
                "[SETUP] Warning: could not create run network: {}. Reference servers may be unavailable.",
                e
            )),
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

        // 3. Start reference containers (attached to the run network under stable aliases)
        logger.log("[SETUP] Starting official reference server in Docker...");
        match config.task {
            TaskType::Redis => {
                if let Err(e) = sandbox.start_reference_redis(6380) {
                    logger.log(&format!(
                        "[SETUP] Warning: reference Redis unavailable: {}",
                        e
                    ));
                }
            }
            TaskType::Http => {
                if let Err(e) = sandbox.start_reference_http(8081) {
                    logger.log(&format!(
                        "[SETUP] Warning: reference HTTP unavailable: {}",
                        e
                    ));
                }
            }
            TaskType::Dns => {
                if let Err(e) = sandbox.start_reference_dns(5354) {
                    logger.log(&format!(
                        "[SETUP] Warning: reference DNS unavailable: {}",
                        e
                    ));
                }
            }
            TaskType::Pdf => {
                logger.log("[SETUP] PDF task uses verifier-generated fixtures; no reference container.");
            }
        }

        // 4. Read prompt
        let prompt_file = get_repo_root()
            .join("tasks")
            .join(config.task.to_string())
            .join("prompt.md");
        let prompt_content = fs::read_to_string(&prompt_file)
            .map_err(|_| anyhow!("Could not read prompt file: {}", prompt_file.display()))?;

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
            let net_name = sandbox.network_name();

            let omp_res = tokio::task::spawn_blocking(move || {
                let spec = AgentRunSpec {
                    model: &model_c,
                    prompt: &prompt_c,
                    workdir: &work_path_buf,
                    api_key: api_key_c.as_deref(),
                    limits,
                    effort: Some(&effort_c),
                    network: Some(net_name.as_str()),
                };
                OmpRunner::run_agent_with_logger(spec, move |line| {
                    logger_sub.log(&line);
                })
            })
            .await;

            match omp_res {
                Ok(Ok(stats)) => {
                    logger.log(&format!(
                        "[OMP] Finished! Tokens: prompt={}, cached={}, completion={}",
                        stats.prompt_tokens, stats.cached_tokens, stats.completion_tokens
                    ));
                    let p = stats.prompt_tokens;
                    let c = stats.cached_tokens;
                    let comp = stats.completion_tokens;
                    (stats, p, c, comp)
                }
                Ok(Err(e)) => {
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
                Err(join_err) => {
                    logger.log(&format!("[OMP ERROR] Task execution error: {}", join_err));
                    agent_failed = true;
                    let partial_stats = OmpSessionStats::default();
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

        // 7. Start candidate and run test suite
        let port = config.task.default_port();
        let has_runnable = sandbox
            .ensure_runnable_candidate(&config.workdir)
            .unwrap_or(false);

        let mut throughput = None;
        let mut reference_throughput = None;
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
            if agent_failed {
                logger
                    .log("[SETUP] Candidate did not produce runnable code. Skipping verification.");
            } else {
                logger.log("[SETUP] No runnable start.sh or Dockerfile found in ./workspace! Skipping verification.");
            }
            (0.0, 0, config.task.total_stages(), Vec::new())
        } else {
            let _ = sandbox.start_candidate_in_docker(&config.workdir, port);

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
                logger.log("[BENCH] 100% tests passed! Running live throughput load test on candidate...");
                match config.task {
                    TaskType::Redis => {
                        if let Ok(tp) = BenchmarkRunner::run_redis_benchmark(port) {
                            logger.log(&format!("  Candidate Throughput: {:.0} req/sec", tp));
                            throughput = Some(tp);
                        }
                        logger.log("[BENCH] Calibrating live official Redis baseline (redis:alpine on port 6380)...");
                        if let Ok(ref_tp) = BenchmarkRunner::run_redis_benchmark(6380) {
                            logger.log(&format!("  Official Reference Redis: {:.0} req/sec", ref_tp));
                            reference_throughput = Some(ref_tp);
                        }
                    }
                    TaskType::Http => {
                        if let Ok(tp) = BenchmarkRunner::run_wrk_benchmark(port) {
                            logger.log(&format!("  Candidate Throughput: {:.0} req/sec", tp));
                            throughput = Some(tp);
                        }
                        logger.log("[BENCH] Calibrating live official Nginx baseline (nginx:alpine on port 8081)...");
                        if let Ok(ref_tp) = BenchmarkRunner::run_wrk_benchmark(8081) {
                            logger.log(&format!("  Official Reference Nginx: {:.0} req/sec", ref_tp));
                            reference_throughput = Some(ref_tp);
                        }
                    }
                    TaskType::Dns => {
                        let verifier = DnsVerifier::new(port, Some(&verifier_seed));
                        if let Some(tp) = verifier.measure_throughput().await {
                            logger.log(&format!("  Candidate Throughput: {:.0} queries/sec", tp));
                            throughput = Some(tp);
                        }
                        logger.log("[BENCH] No external reference for DNS; score uses verifier timing.");
                    }
                    TaskType::Pdf => {
                        let verifier = PdfVerifier::new(port, Some(&verifier_seed));
                        if let Some(tp) = verifier.measure_throughput().await {
                            logger.log(&format!("  Candidate Throughput: {:.0} pages/sec", tp));
                            throughput = Some(tp);
                        }
                        logger.log("[BENCH] No external reference for PDF extraction; score uses verifier timing.");
                    }
                }

                if let (Some(cand_tp), Some(ref_tp)) = (throughput, reference_throughput) {
                    if ref_tp > 0.0 {
                        let ratio = (cand_tp / ref_tp) * 100.0;
                        logger.log(&format!("  ⚡ Relative Performance: {:.1}% of official reference", ratio));
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
            .max(prompt_tokens + completion_tokens);

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
        let effective_cost_usd =
            ModelPricing::calculate_effective_cost(cost_usd, duration_seconds, total_run_tokens);
        let efficiency_score =
            ModelPricing::calculate_efficiency_score(pass_rate, effective_cost_usd);
        let throughput_score = ModelPricing::calculate_throughput_score(
            pass_rate,
            effective_cost_usd,
            throughput,
            reference_throughput,
        );

        let lang = LeaderboardManager::detect_language(&config.workdir);
        let scanned_files = RunArchiver::scan_workspace_files(&config.workdir);

        let manifest = RunManifest {
            run_id: run_id.clone(),
            model: config.model.clone(),
            task: config.task.to_string(),
            status: if disqualified {
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
            tokens: RunTokenUsage {
                prompt_tokens,
                cached_tokens,
                completion_tokens,
                total_tokens: omp_stats
                    .total_tokens
                    .max(prompt_tokens + completion_tokens),
            },
            cost_usd,
            effective_cost_usd: Some(effective_cost_usd),
            savings_percent: breakdown.savings_percent,
            efficiency_score,
            throughput_score: Some(throughput_score),
            files: scanned_files,
            env: None,
            git_commit: None,
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
            prompt_tokens,
            cached_tokens,
            completion_tokens,
            total_cost_usd: cost_usd,
            effective_cost_usd: Some(effective_cost_usd),
            duration_seconds: Some(duration_seconds),
            turns: Some(omp_stats.steps_taken),
            savings_percent: breakdown.savings_percent,
            efficiency_score,
            throughput_score: Some(throughput_score),
            timestamp: completed_at,
            method_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };

        logger.log(&format!(
            "[TELEMETRY] turn={} max_turns={} spent={:.6} tokens={}",
            omp_stats.steps_taken,
            config.max_turns,
            cost_usd,
            total_run_tokens
        ));

        // 11. Optional Archiving and Leaderboard update
        if config.save_results {
            let runs_dir = RunArchiver::resolve_runs_dir();
            let console_log = buffering_logger.get_logs();
            let _ = RunArchiver::archive_run(&runs_dir, &manifest, &config.workdir, &console_log);
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
                    get_repo_root().join("tasks").join(&task_name).join("prompt.md"),
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
                        buffering_logger.log(&format!(
                            "[REVIEW] Judge {} scored {} strengths, {} weaknesses",
                            judge_model,
                            review.good.len(),
                            review.bad.len()
                        ));
                    }
                    Err(e) => {
                        buffering_logger.log(&format!("[REVIEW] Skipped: {}", e));
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

        // Clean up
        let _ = fs::remove_dir_all(&temp_workdir);
        let _ = fs::remove_dir_all(runs_dir.join(&run_id));
        let results_dir = get_repo_root().join("results");
        let _ = fs::remove_file(results_dir.join(format!("{}.json", run_id)));
    }
}
