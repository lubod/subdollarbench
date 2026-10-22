use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use colored::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use subdollarbench::config::{Cli, Commands, Harness, TaskType};
use subdollarbench::pipeline::{
    BenchmarkConfig, BenchmarkPipeline, PipelineLogger, PipelineOutput,
};
use subdollarbench::remote;
use subdollarbench::report::{
    assess_stability, BenchmarkRunResult, LeaderboardManager, PortalExporter, RunManifest,
    Stability,
};
use subdollarbench::sandbox::docker::{RunGuard, SandboxManager, PROXY_UPSTREAM_PORTS};
use subdollarbench::verifier::{
    BinprotoVerifier, HttpVerifier, PdfVerifier, ProxyVerifier, PubsubVerifier, QueueVerifier,
    RedisVerifier, WalVerifier,
};
use subdollarbench::web;
use subdollarbench::web::server::compute_pass_at_k;

mod main_campaign;
mod main_queue;
mod review_cmd;

struct CliPipelineLogger;

impl PipelineLogger for CliPipelineLogger {
    fn log(&self, msg: &str) {
        println!("{}", msg);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let cli = Cli::parse();
    run_cli(cli).await
}

pub(crate) fn resolve_run_id(runs_dir: &str, prefix: &str) -> Result<String> {
    let path = Path::new(runs_dir);
    if path.join(prefix).is_dir() {
        return Ok(prefix.to_string());
    }
    let mut hits: Vec<String> = std::fs::read_dir(path)
        .with_context(|| format!("read runs dir {runs_dir}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|id| !id.ends_with(".tmp") && !id.starts_with('.'))
        .filter(|id| id.starts_with(prefix))
        .collect();
    hits.sort();
    match hits.len() {
        0 => anyhow::bail!("no run matches prefix '{prefix}' in {runs_dir}"),
        1 => Ok(hits.remove(0)),
        _ => {
            eprintln!("ambiguous prefix '{prefix}':");
            for h in &hits {
                eprintln!("  {h}");
            }
            anyhow::bail!("{} runs match; give a longer prefix", hits.len())
        }
    }
}

fn load_run_manifest(runs_dir: &str, run_id: &str) -> Result<RunManifest> {
    let text = std::fs::read_to_string(Path::new(runs_dir).join(run_id).join("manifest.json"))
        .with_context(|| format!("read manifest for {run_id}"))?;
    Ok(serde_json::from_str(&text)?)
}

fn apply_trial_suffix(run_id: &str, trial: Option<u32>) -> String {
    match trial {
        Some(t) if !run_id.contains("_trial") => format!("{run_id}_trial{t}"),
        _ => run_id.to_string(),
    }
}

/// Exit code for `verdict`: 0 pass / 1 test failure / 2 infra-error.
fn verdict_code(m: &RunManifest) -> i32 {
    match m.status.as_str() {
        "completed" => 0,
        "failed_tests" | "disqualified_cheat" => 1,
        _ => 2,
    }
}

/// How a finished trial counts toward pass@k, as
/// `(is_infrastructure_fault, counts_as_passing)`.
///
/// A harness fault is a non-result in *both* directions: it must not inflate
/// the denominator, and it must not be able to contribute a passing trial
/// either. The second half is the subtle one — a run that returned `Ok` with
/// `infra_error` set can still carry `pass_rate == 100.0` if an earlier stage
/// happened to pass, and counting that as a model success credits the model for
/// a run the harness corrupted.
fn trial_accounting(output: &PipelineOutput) -> (bool, bool) {
    if output.infra_error {
        (true, false)
    } else {
        (false, output.benchmark_result.pass_rate == 100.0)
    }
}

/// The differential SQL stages the verifier implements today, run
/// synchronously for the `eval` and `replay` commands: one script per
/// stage, fresh per-(seed tag, stage) seeds, shrink on failure.
fn run_sql_stages_sync(port: u16, seed_tag: &str) -> (f64, u32, u32) {
    use subdollarbench::verifier::sql::driver;
    let addr = format!("127.0.0.1:{port}");
    let total = 4u32;
    let mut passed = 0u32;
    for stage in 0..total {
        let ids: Vec<u32> = (0..=stage).collect();
        let seed = driver::stage_seed(seed_tag, stage);
        let title = driver::stage_title(stage);
        match driver::run_stage(
            &addr,
            std::time::Duration::from_secs(10),
            seed,
            &ids,
            driver::DEFAULT_STEPS,
            None,
            None,
        ) {
            Ok(out) if out.passed => {
                passed += 1;
                println!("  Stage {stage}: {title} -> PASS");
            }
            Ok(out) => println!("  Stage {stage}: {title} -> FAIL {}", out.detail),
            Err(e) => println!("  Stage {stage}: {title} -> VERIFIER FAULT {e}"),
        }
    }
    (passed as f64 / total as f64 * 100.0, passed, total)
}

fn parse_task(s: &str) -> Result<TaskType> {
    // Single source of truth: the queue engine resolves task names too, and
    // when the two had separate match blocks any gate here was bypassed there.
    TaskType::parse(s)
}

pub(crate) fn resolve_parent_id(results_dir: &str, prefix: &str) -> Result<String> {
    let path = Path::new(results_dir);
    if path.join(format!("{prefix}.json")).exists() {
        return Ok(prefix.to_string());
    }
    let mut hits: Vec<String> = std::fs::read_dir(path)
        .with_context(|| format!("read results dir {results_dir}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_suffix(".json").map(|s| s.to_string())
        })
        .filter(|id| id.starts_with(prefix))
        .collect();
    hits.sort();
    match hits.len() {
        0 => anyhow::bail!("no consolidated run matches prefix '{prefix}' in {results_dir}"),
        1 => Ok(hits.remove(0)),
        _ => {
            eprintln!("ambiguous prefix '{prefix}':");
            for h in &hits {
                eprintln!("  {h}");
            }
            anyhow::bail!("{} runs match; give a longer prefix", hits.len())
        }
    }
}

fn load_parent_result(results_dir: &str, parent_id: &str) -> Result<BenchmarkRunResult> {
    let text = std::fs::read_to_string(Path::new(results_dir).join(format!("{parent_id}.json")))
        .with_context(|| format!("read parent result {parent_id}"))?;
    Ok(serde_json::from_str(&text)?)
}

struct TrialBatchParams {
    model: String,
    task: TaskType,
    effort: String,
    harness: Harness,
    max_turns: u32,
    budget_usd: f64,
    timeout_min: u64,
    api_key: Option<String>,
    workdir: PathBuf,
    eval_only: bool,
    review_model: Option<String>,
    parent_run_id: String,
    start_idx: u32,
    count: u32,
    feedback_rounds: u32,
}

struct TrialBatchOutput {
    results: Vec<BenchmarkRunResult>,
    passing: u32,
    /// Trials that errored (non-infra Err arm): attempted but unscored.
    /// Lets callers report "N errored" honestly instead of blanket
    /// "all failed".
    errors: u32,
    drift: Option<bool>,
}

/// Run one extension batch (fresh batch reference, suffix-numbered trial dirs).
async fn run_trial_batch(
    p: TrialBatchParams,
    logger: Arc<CliPipelineLogger>,
) -> Result<TrialBatchOutput> {
    let batch_ref = if p.count > 1 {
        match subdollarbench::bench::batch_ref::BatchRef::start_async(
            &p.task,
            &p.parent_run_id,
            &|m| println!("{}", m),
            0,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  [BATCH-REF] setup failed ({}); trials use live refs.", e);
                None
            }
        }
    } else {
        None
    };
    let mut results = Vec::new();
    let mut passing = 0;
    let mut errors = 0u32;
    for i in 0..p.count {
        let idx = p.start_idx + i;
        println!(
            "{}",
            format!("=== Starting Extension Subrun {} ===", idx)
                .bold()
                .cyan()
        );
        let run_id = format!("{}_trial{}", p.parent_run_id, idx);
        // Per-trial workdir under the shared root (never shared between
        // concurrent trials: pipeline wipes workdir at trial start).
        let trial_workdir = if p.workdir.is_absolute() {
            p.workdir.join(&run_id)
        } else {
            p.workdir.clone()
        };
        let _ = std::fs::create_dir_all(&trial_workdir);
        let pipe_config = BenchmarkConfig {
            model: p.model.clone(),
            task: p.task,
            effort: p.effort.clone(),
            harness: p.harness,
            max_turns: p.max_turns,
            budget_usd: p.budget_usd,
            timeout_min: p.timeout_min,
            api_key: p.api_key.clone(),
            workdir: trial_workdir,
            eval_only: p.eval_only,
            run_id: Some(run_id.clone()),
            save_results: true,
            review_model: p.review_model.clone(),
            batch_reference: batch_ref.as_ref().map(|b| b.reference()),
            worker: 0,
            feedback_rounds: p.feedback_rounds,
            campaign_stage: None,
            baseline_rps: None,
        };
        match BenchmarkPipeline::execute(pipe_config, logger.clone(), None).await {
            Ok(output) => {
                // Same rule as the foreground loop: a harness fault is a
                // non-result and must not become a scored extension trial.
                if output.infra_error {
                    eprintln!(
                        "  [INFRA] Extension subrun {idx} degraded by a harness fault \
                         (status: {}); excluded",
                        output.manifest.status
                    );
                    continue;
                }
                if output.benchmark_result.pass_rate == 100.0 {
                    passing += 1;
                }
                results.push(output.benchmark_result);
            }
            Err(e) => {
                eprintln!("  [ERROR] Extension subrun {} failed: {}", idx, e);
                errors += 1;
            }
        }
    }
    let drift = match batch_ref {
        Some(b) => Some(b.finish(&|m| println!("{}", m)).await.drift),
        None => None,
    };
    Ok(TrialBatchOutput {
        results,
        passing,
        errors,
        drift,
    })
}

/// Resolve `--server` for remote commands: flag > SDB_SERVER env > default.
pub(crate) fn resolve_server_flag(server: &Option<String>) -> String {
    remote::resolve_server(server.as_deref())
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    tracing::info!(
        "repo root: {}",
        subdollarbench::config::get_repo_root().display()
    );
    match cli.command {
        Commands::Run {
            model,
            task,
            effort,
            harness,
            max_turns,
            budget_usd,
            timeout_min,
            api_key,
            workdir,
            eval_only,
            trials,
            no_save,
            queue,
            server,
            review_model,
            extend_if_unstable,
            max_trials,
            stop_on_pass,
            feedback,
            feedback_rounds,
        } => {
            let feedback_rounds = if feedback && feedback_rounds == 0 {
                2
            } else {
                feedback_rounds
            };
            if queue {
                let server = resolve_server_flag(&server);
                // Engine path: identical payload to `enqueue`, so matrix
                // output stays uniform. Foreground stays the default.
                let body = remote::run_request(
                    &model,
                    &task.to_string(),
                    &effort,
                    harness.as_str(),
                    max_turns,
                    budget_usd,
                    timeout_min,
                    api_key.as_deref(),
                    eval_only,
                    trials,
                    extend_if_unstable,
                    max_trials,
                    no_save,
                    review_model.as_deref(),
                    stop_on_pass,
                    feedback_rounds,
                );
                let resp = remote::post_json(&server, "/api/queue", &body).await?;
                let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                if resp
                    .get("started")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    println!("enqueued {id}: engine idle, started immediately");
                } else {
                    let pos = resp
                        .get("position")
                        .and_then(|v| v.as_u64())
                        .map(|p| p.to_string())
                        .unwrap_or("-".into());
                    println!("enqueued {id} at position {pos}");
                }
                return Ok(());
            }
            let mut total_trials = if stop_on_pass {
                subdollarbench::web::queue::screen_initial_trials(trials)
            } else {
                trials.max(1)
            };
            // Trials that failed for infrastructure reasons, not model reasons.
            // Excluded from the pass@k denominator: counting them as failures
            // is how a flaky host turns into a published model score.
            let mut infra_failed: u32 = 0;
            let mut passing_trials = 0;
            let logger = Arc::new(CliPipelineLogger);

            let work_path_buf = if Path::new(&workdir).is_absolute() {
                PathBuf::from(&workdir)
            } else {
                subdollarbench::config::get_repo_root().join(&workdir)
            };

            let run_timestamp = Utc::now().format("%Y%m%d_%H%M%S%.3f").to_string();
            let parent_run_id = format!(
                "{}_{}_{}_w0",
                task,
                model.replace(['/', ':'], "_"),
                run_timestamp
            );
            let mut subrun_results = Vec::new();

            // Batch reference: one shared denominator for all trials.
            // Single-trial runs keep live per-trial measurement.
            let batch_ref = if total_trials > 1 {
                match subdollarbench::bench::batch_ref::BatchRef::start_async(
                    &task,
                    &parent_run_id,
                    &|m| println!("{}", m),
                    0,
                )
                .await
                {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("  [BATCH-REF] setup failed ({}); trials use live refs.", e);
                        None
                    }
                }
            } else {
                None
            };

            let mut trial_idx = 1;
            while trial_idx <= total_trials {
                if total_trials > 1 || stop_on_pass {
                    println!(
                        "{}",
                        format!("=== Starting Subrun {}/{} ===", trial_idx, total_trials)
                            .bold()
                            .cyan()
                    );
                }

                let run_id = if total_trials > 1 || stop_on_pass {
                    format!("{}_trial{}", parent_run_id, trial_idx)
                } else {
                    parent_run_id.clone()
                };

                let pipe_config = BenchmarkConfig {
                    model: model.clone(),
                    task,
                    effort: effort.clone(),
                    harness,
                    max_turns,
                    budget_usd,
                    timeout_min,
                    api_key: api_key.clone(),
                    workdir: work_path_buf.clone(),
                    eval_only,
                    run_id: Some(run_id.clone()),
                    save_results: !no_save,
                    review_model: review_model.clone(),
                    batch_reference: batch_ref.as_ref().map(|b| b.reference()),
                    worker: 0,
                    feedback_rounds,
                    campaign_stage: None,
                    baseline_rps: None,
                };

                let pipe_res = BenchmarkPipeline::execute(pipe_config, logger.clone(), None).await;

                match pipe_res {
                    Ok(output) => {
                        let (infra, passing) = trial_accounting(&output);
                        if infra {
                            // The run completed but a harness fault degraded
                            // it - the workspace could not be inspected, or the
                            // candidate container would not start. It still
                            // returns Ok, so without this it lands in
                            // `subrun_results` as a 0% trial and is counted in
                            // the pass@k denominator: the same defect as the
                            // Err arm below.
                            eprintln!(
                                "  [INFRA] Subrun {}/{} degraded by a harness fault; \
                                 excluded from pass@k (status: {})",
                                trial_idx, total_trials, output.manifest.status
                            );
                            infra_failed += 1;
                            trial_idx += 1;
                            continue;
                        }
                        if passing {
                            passing_trials += 1;
                        }
                        if total_trials > 1 || stop_on_pass {
                            println!(
                                "{}",
                                format!(
                                    "=== Subrun {}/{} Completed (Pass Rate: {:.1}%) ===",
                                    trial_idx, total_trials, output.benchmark_result.pass_rate
                                )
                                .bold()
                                .cyan()
                            );
                        }
                        subrun_results.push(output.benchmark_result);

                        if stop_on_pass && passing {
                            println!(
                                "{}",
                                format!(
                                    "  [CAPABILITY] Solved at trial {} (100.0% stages passed)! Stopping cell (stop-on-pass protocol).",
                                    trial_idx
                                )
                                .bold()
                                .green()
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        // An infrastructure failure is not evidence about the
                        // model. It used to be logged and dropped while
                        // `total_trials` stayed the pass@k denominator, so a
                        // Docker outage or an expired key scored as a model
                        // that failed. Counted apart and excluded below.
                        eprintln!(
                            "  [ERROR] Subrun {}/{} INFRASTRUCTURE failure (not a model result): {}",
                            trial_idx, total_trials, e
                        );
                        infra_failed += 1;
                    }
                }

                if stop_on_pass
                    && passing_trials == 0
                    && subrun_results.is_empty()
                    && infra_failed > 0
                {
                    // Foreground is interactive: no auto-fire (the operator
                    // is watching). Engine waves auto-extend this shape
                    // within SDB_AUTO_EXTEND_TRIALS; here, say so.
                    println!(
                        "{}",
                        format!(
                            "  [CAPABILITY] 0 passing, all {} harness-fault — no model evidence. Re-run, extend manually, or enqueue (engine auto-extends within its envelope).",
                            infra_failed
                        )
                        .bold()
                        .yellow()
                    );
                }

                trial_idx += 1;
            }

            let batch_drift = match batch_ref {
                Some(b) => Some(b.finish(&|m| println!("{}", m)).await.drift),
                None => None,
            };

            let mut merged_opt = if (total_trials > 1 || stop_on_pass || subrun_results.len() > 1)
                && !subrun_results.is_empty()
            {
                // Protocol is set BEFORE the merge/save so the JSON export
                // and the DB row agree (the old post-save upsert wrote only
                // the DB; it also ignored `no_save`).
                let protocol = if stop_on_pass {
                    Some("sequential_cap".to_string())
                } else {
                    None
                };
                LeaderboardManager::consolidate_multi_trial_run(
                    &parent_run_id,
                    &subrun_results,
                    !no_save,
                    batch_drift,
                    protocol,
                )
            } else {
                None
            };

            if extend_if_unstable && !stop_on_pass {
                if total_trials < 2 {
                    println!("[EXTEND] single-trial run: nothing to assess; use --trials 2+ for auto-extend.");
                } else if total_trials <= 3
                    && subrun_results
                        .iter()
                        .filter(|r| r.pass_rate >= 100.0)
                        .count()
                        <= 1
                {
                    // Dead-cell early stop (wave-v2: 0-1/3 never recovered).
                    println!(
                        "[EXTEND] dead cell ({}/{} passing at {}); stopping, no extend.",
                        subrun_results
                            .iter()
                            .filter(|r| r.pass_rate >= 100.0)
                            .count(),
                        subrun_results.len(),
                        total_trials
                    );
                } else {
                    let max_total = max_trials.max(total_trials);
                    while total_trials < max_total {
                        let merged = match merged_opt.as_ref() {
                            Some(m) => m,
                            None => break,
                        };
                        let (stab, reason) = assess_stability(&merged.subruns);
                        if stab == Stability::Stable {
                            println!("[EXTEND] stable: {}; done.", reason);
                            break;
                        }
                        let add = (max_total - total_trials).min(3);
                        println!(
                            "[EXTEND] unstable ({}); running {} more trial(s)...",
                            reason, add
                        );
                        let batch = run_trial_batch(
                            TrialBatchParams {
                                model: model.clone(),
                                task,
                                effort: effort.clone(),
                                harness: Harness::Omp,
                                max_turns,
                                budget_usd,
                                timeout_min,
                                api_key: api_key.clone(),
                                workdir: work_path_buf.clone(),
                                eval_only,
                                review_model: review_model.clone(),
                                parent_run_id: parent_run_id.clone(),
                                start_idx: subdollarbench::web::queue::next_trial_idx(
                                    &subrun_results,
                                ),
                                count: add,
                                feedback_rounds,
                            },
                            logger.clone(),
                        )
                        .await?;
                        if batch.results.is_empty() {
                            if batch.errors > 0 {
                                eprintln!(
                                    "[EXTEND] batch produced no scored trials ({} errored); keeping current merge.",
                                    batch.errors
                                );
                            } else {
                                eprintln!(
                                    "[EXTEND] batch produced no scored trials and no errors (setup refused before any trial); keeping current merge."
                                );
                            }
                            break;
                        }
                        total_trials += batch.results.len() as u32;
                        passing_trials += batch.passing as usize;
                        subrun_results.extend(batch.results);
                        // Quarantine is sticky (web path already does this):
                        // a drifted parent stays drifted even when the
                        // extension bracket is clean. Passing only
                        // batch.drift would erase it and re-score a bad
                        // denominator.
                        let kept_drift = merged_opt
                            .as_ref()
                            .and_then(|m| m.ref_drift)
                            .or(batch.drift);
                        // Protocol rides the same way as drift: the parent's
                        // stopping rule survives an extension re-merge.
                        let kept_protocol = merged_opt.as_ref().and_then(|m| m.protocol.clone());
                        // Take ownership back for the reassignment below
                        // (borrow ends here; NLL releases it).
                        let _ = &kept_drift;
                        merged_opt = LeaderboardManager::consolidate_multi_trial_run(
                            &parent_run_id,
                            &subrun_results,
                            !no_save,
                            kept_drift,
                            kept_protocol,
                        );
                    }
                }
            }

            // The denominator counts only trials that produced a verdict.
            let scored_trials = total_trials.saturating_sub(infra_failed);
            if infra_failed > 0 {
                eprintln!(
                    "  [INFRA] {infra_failed} of {total_trials} trial(s) failed for \
                     infrastructure reasons and are excluded from pass@k; \
                     {scored_trials} scored."
                );
            }
            if scored_trials > 1 {
                let pass_at_1 =
                    compute_pass_at_k(scored_trials as usize, passing_trials, 1) * 100.0;
                let pass_at_k = compute_pass_at_k(
                    scored_trials as usize,
                    passing_trials,
                    scored_trials as usize,
                ) * 100.0;
                println!(
                    "\n{}",
                    "=== Multi-Trial Pass@k Summary ===".bold().magenta()
                );
                println!("  Total Trials:   {total_trials}");
                println!("  Passing Trials: {}", passing_trials);
                if infra_failed > 0 {
                    println!("  Infra Failures: {infra_failed} (excluded)");
                }
                println!("  Scored Trials:  {scored_trials}");
                println!("  Pass@1:         {:.1}%", pass_at_1);
                println!("  Pass@{scored_trials}:      {:.1}%", pass_at_k);
                println!("{}", "=================================".bold().magenta());
            }

            let results_dir_buf = subdollarbench::config::get_repo_root().join("results");
            let results_dir_str = results_dir_buf.to_str().unwrap_or("./results");
            let all = LeaderboardManager::load_all(results_dir_str);
            LeaderboardManager::print_table(&all);
        }

        Commands::Eval { task, port } => {
            // Fresh random seed per eval: static nonces (seed=None) make
            // standalone eval a stub-passable pre-flight. Same rule as runs.
            let eval_seed = format!(
                "eval-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let p = port.unwrap_or(match task {
                TaskType::Redis => 6379,
                TaskType::Http => 8080,
                TaskType::Pdf => 8090,
                TaskType::Wal => 6390,
                TaskType::Proxy => 8091,
                TaskType::Pubsub => 8092,
                TaskType::Binproto => 6395,
                TaskType::Queue => 6385,
                TaskType::Sql => 5480,
            });
            println!("Evaluating {} on port {}...", task, p);
            match task {
                TaskType::Redis => {
                    let v = RedisVerifier::new(p, Some(&eval_seed));
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Http => {
                    let v = HttpVerifier::new(p, Some(&eval_seed));
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Pdf => {
                    let v = PdfVerifier::new(p, Some(&eval_seed));
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Wal => {
                    let v = WalVerifier::new(p, Some(&eval_seed));
                    let res = v.run_basic().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Queue => {
                    let v = QueueVerifier::new(p, Some(&eval_seed));
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Proxy => {
                    let ups: Vec<(String, u16)> = (1u8..=3u8)
                        .map(|i| {
                            (
                                format!("subdollar-upstream{}-manual", i),
                                subdollarbench::sandbox::docker::PROXY_UPSTREAM_PORTS
                                    [(i - 1) as usize],
                            )
                        })
                        .collect();
                    let v = ProxyVerifier::new(p, None, ups);
                    let res = v.run_routing().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Pubsub => {
                    let v = PubsubVerifier::new(p, Some(&eval_seed));
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Binproto => {
                    let v = BinprotoVerifier::new(p, Some(&eval_seed));
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Sql => {
                    let (_rate, passed, total) = run_sql_stages_sync(p, &eval_seed);
                    println!("Pass rate: {:.1}% ({}/{})", _rate, passed, total);
                }
            }
        }

        Commands::Leaderboard { results_dir, task } => {
            let all = LeaderboardManager::load_all(&results_dir);
            if all.is_empty() {
                println!("No benchmark results found in {}", results_dir);
            } else {
                LeaderboardManager::print_leaderboard_views(&all, &task);
            }
        }

        Commands::Backfill { results_dir, yes } => {
            use std::path::Path;
            let path = Path::new(&results_dir);
            let mut entries: Vec<String> = std::fs::read_dir(path)
                .with_context(|| format!("read results dir {results_dir}"))?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".json"))
                .collect();
            entries.sort();
            let (mut rewritten, mut stamped, mut skipped) = (0, 0, 0);
            // Cutover backfill: every stored parent is re-merged with
            // current scoring and re-stamped with the current fingerprint
            // (see `fresh_stamp`). The old guard that left foreign
            // rulesets alone is gone on purpose: leaving old maths next
            // to new maths under comparable-looking labels is exactly
            // the inconsistency this cutover fixes.
            for name in &entries {
                let fp = path.join(name);
                let Ok(text) = std::fs::read_to_string(&fp) else {
                    skipped += 1;
                    continue;
                };
                let Ok(run) =
                    serde_json::from_str::<subdollarbench::report::BenchmarkRunResult>(&text)
                else {
                    skipped += 1;
                    continue;
                };
                if run.subruns.is_empty() {
                    // Single-trial row: no merge math to redo, but the
                    // score stamp still moves to the current fingerprint.
                    // Grader provenance is preserved, never derived: None
                    // stays None (honest unknown). Deriving it from
                    // rules_version here would stamp the CURRENT fingerprint
                    // as the grader on the next regime bump — the exact
                    // false-provenance bug this split exists to prevent.
                    let fresh = subdollarbench::report::rules::fingerprint();
                    if run.rules_version.as_deref() != Some(fresh.as_str()) {
                        let mut single = run.clone();
                        single.rules_version = Some(fresh.clone());
                        if yes {
                            let data = serde_json::to_string_pretty(&single)?;
                            std::fs::write(&fp, data)?;
                            if let Some(store) = LeaderboardManager::open_store_if_exists() {
                                if let Err(e) = store.upsert_parent(&single) {
                                    tracing::warn!(
                                        "backfill DB upsert {} failed: {:#}",
                                        single.id,
                                        e
                                    );
                                }
                            }
                        }
                        stamped += usize::from(yes);
                        rewritten += usize::from(!yes);
                    } else {
                        skipped += 1;
                    }
                    continue;
                }
                // Regime cutover: a stored run graded under an older ruleset
                // IS re-scored here and re-stamped with the current
                // fingerprint (see `fresh_stamp` below). Publishing old and
                // new maths side by side under one label is the worse
                // outcome; the re-stamp makes the cutover explicit and the
                // matrix gate refuses cross-regime ranks either way.
                // Legacy stamp when the run predates fingerprinting.
                let want_stamp = run.rules_version.is_none();
                let stamp = want_stamp.then(|| {
                    subdollarbench::report::legacy_rules_version(&run.timestamp).to_string()
                });
                let fresh_stamp = subdollarbench::report::rules::fingerprint();
                // Re-merge with current scoring from stored subruns.
                // Stored subruns also predate fingerprinting: stamp them first
                // so the merge inherits a unanimous (non-mixed) version.
                let mut subs = run.subruns.clone();
                for sub in &mut subs {
                    if sub.rules_version.is_none() {
                        sub.rules_version = stamp.clone();
                    }
                    // Proportional rescore (2026-09-24.1): stored subruns
                    // predate the cliff removal; recompute their scores from
                    // pass_rate + effective cost so trial rows match parents.
                    let eff_cost = sub.effective_cost_usd.unwrap_or(sub.total_cost_usd);
                    sub.efficiency_score =
                        subdollarbench::cost::calculator::ModelPricing::calculate_efficiency_score(
                            sub.pass_rate,
                            eff_cost,
                        );
                    sub.throughput_score = subdollarbench::cost::calculator::ModelPricing::calculate_throughput_score_for_task(
                        sub.pass_rate,
                        eff_cost,
                        sub.throughput_req_sec,
                        sub.reference_throughput_req_sec,
                        Some(&sub.task),
                    );
                }
                let Some(merged) = subdollarbench::report::BenchmarkRunResult::merge_subruns(
                    &run.id,
                    subs.clone(),
                    run.ref_drift,
                    // Pass-through, never overwrite: the stored grader is
                    // the witness value (or honest None). Merge takes it as
                    // a param precisely so a re-merge cannot launder trial
                    // stamps onto the parent (420/420 incident, .3 cutover).
                    run.graded_under.clone(),
                ) else {
                    skipped += 1;
                    continue;
                };
                // Preserve the recorded batch bracket; only scoring fields move.
                // Carry the rescored subruns (proportional eff/tp) so the
                // stored trial rows match the new rule-set.
                let mut out = merged;
                out.ref_drift = run.ref_drift;
                out.subruns = out
                    .subruns
                    .into_iter()
                    .map(|m| match subs.iter().find(|r| r.id == m.id) {
                        Some(r) => r.clone(),
                        None => m,
                    })
                    .collect();
                // Cutover re-stamp: `rules_version` carries the current
                // scoring fingerprint; `graded_under` is NEVER derived here
                // — it arrives on the row from the witness backfill (or a
                // fresh run's own stamp) and is preserved untouched. A
                // backfill recomputes cost, it does not re-grade. If the
                // row has no grader, that is honest None (inferred legacy
                // downstream), never the fresh stamp laundered as history.
                let grader = run.graded_under.clone();
                out.rules_version = Some(fresh_stamp.clone());
                for sub in &mut out.subruns {
                    sub.rules_version = out.rules_version.clone();
                }
                out.graded_under = grader;
                fn opt_eq(a: Option<f64>, b: Option<f64>) -> bool {
                    match (a, b) {
                        (Some(x), Some(y)) => (x - y).abs() < 1e-9,
                        (None, None) => true,
                        _ => false,
                    }
                }
                let subs_same = run.subruns.len() == out.subruns.len()
                    && run.subruns.iter().zip(out.subruns.iter()).all(|(a, b)| {
                        (a.efficiency_score - b.efficiency_score).abs() < 1e-9
                            && opt_eq(a.throughput_score, b.throughput_score)
                            // Subrun stat backfills (billing telemetry:
                            // turns/tokens/cost patched post-hoc) must
                            // trigger the parent fair-cost recompute below.
                            && (a.total_cost_usd - b.total_cost_usd).abs() < 1e-9
                            && a.total_tokens == b.total_tokens
                            && a.turns == b.turns
                    });
                let same = (run.pass_rate - out.pass_rate).abs() < 1e-9
                    && opt_eq(run.pass_at_1_lo, out.pass_at_1_lo)
                    && opt_eq(run.pass_at_1_hi, out.pass_at_1_hi)
                    && opt_eq(run.throughput_score, out.throughput_score)
                    && (run.efficiency_score - out.efficiency_score).abs() < 1e-9
                    && (run.total_cost_usd - out.total_cost_usd).abs() < 1e-9
                    && subs_same;
                // NOTE: stamp excluded from `same` on purpose — a
                // cutover re-stamp (old label → current fingerprint)
                // with identical numbers is still a rewrite, not a skip.
                let stamp_changed = run.rules_version != out.rules_version;
                if same && !stamp_changed {
                    skipped += 1;
                    continue;
                }
                if same && stamp_changed {
                    // Cutover re-stamp with identical numbers: rewrite
                    // the file + DB + manifest (not a skip), so no old
                    // label survives next to new maths.
                    if yes {
                        let data = serde_json::to_string_pretty(&out)?;
                        std::fs::write(&fp, data)?;
                        if let Some(store) = LeaderboardManager::open_store_if_exists() {
                            if let Err(e) = store.upsert_parent(&out) {
                                tracing::warn!("backfill DB upsert {} failed: {:#}", out.id, e);
                            }
                        }
                        let mpath = subdollarbench::report::RunArchiver::resolve_runs_dir()
                            .join(&run.id)
                            .join("manifest.json");
                        if let Ok(mtext) = std::fs::read_to_string(&mpath) {
                            if let Ok(mut mval) = serde_json::from_str::<serde_json::Value>(&mtext)
                            {
                                mval["rules_version"] = serde_json::Value::String(
                                    out.rules_version.clone().unwrap_or_default(),
                                );
                                let _ =
                                    std::fs::write(&mpath, serde_json::to_string_pretty(&mval)?);
                            }
                        }
                    }
                    stamped += 1;
                    continue;
                }
                println!(
                    "  {}: pass {:.0} -> {:.0} | P@1 [{:.0}-{:.0}] | tp {} -> {} | rules {} -> {}",
                    run.id,
                    run.pass_rate,
                    out.pass_rate,
                    out.pass_at_1_lo.unwrap_or(-1.0),
                    out.pass_at_1_hi.unwrap_or(-1.0),
                    match run.throughput_score {
                        Some(v) => format!("{v:.1}"),
                        None => "-".to_string(),
                    },
                    match out.throughput_score {
                        Some(v) => format!("{v:.1}"),
                        None => "-".to_string(),
                    },
                    run.rules_version.as_deref().unwrap_or("-"),
                    out.rules_version.as_deref().unwrap_or("-"),
                );
                if yes {
                    let data = serde_json::to_string_pretty(&out)?;
                    std::fs::write(&fp, data)?;
                    // Best-effort DB mirror (same gating as consolidate); never fatal.
                    if let Some(store) = LeaderboardManager::open_store_if_exists() {
                        if let Err(e) = store.upsert_parent(&out) {
                            tracing::warn!("backfill DB upsert {} failed: {:#}", out.id, e);
                        }
                    }
                    // Mirror headline + Wilson + rules into the parent manifest.
                    let mpath = subdollarbench::report::RunArchiver::resolve_runs_dir()
                        .join(&run.id)
                        .join("manifest.json");
                    if let Ok(mtext) = std::fs::read_to_string(&mpath) {
                        if let Ok(mut mval) = serde_json::from_str::<serde_json::Value>(&mtext) {
                            mval["pass_rate"] = serde_json::Value::from(out.pass_rate);
                            mval["passed_stages"] = serde_json::Value::from(out.passed_stages);
                            mval["total_trials"] =
                                serde_json::Value::from(out.total_trials.unwrap_or(0));
                            mval["pass_at_1"] = out
                                .pass_at_1
                                .map(serde_json::Value::from)
                                .unwrap_or(serde_json::Value::Null);
                            mval["pass_at_1_lo"] = out
                                .pass_at_1_lo
                                .map(serde_json::Value::from)
                                .unwrap_or(serde_json::Value::Null);
                            mval["pass_at_1_hi"] = out
                                .pass_at_1_hi
                                .map(serde_json::Value::from)
                                .unwrap_or(serde_json::Value::Null);
                            mval["stability"] = out
                                .stability
                                .clone()
                                .map(serde_json::Value::String)
                                .unwrap_or(serde_json::Value::Null);
                            mval["trial_pass_rates"] = serde_json::Value::Array(
                                out.subruns
                                    .iter()
                                    .map(|r| serde_json::Value::from(r.pass_rate))
                                    .collect(),
                            );
                            mval["throughput_req_sec"] = out
                                .throughput_req_sec
                                .map(serde_json::Value::from)
                                .unwrap_or(serde_json::Value::Null);
                            mval["throughput_score"] = out
                                .throughput_score
                                .map(serde_json::Value::from)
                                .unwrap_or(serde_json::Value::Null);
                            mval["rules_version"] = out
                                .rules_version
                                .clone()
                                .map(serde_json::Value::String)
                                .unwrap_or(serde_json::Value::Null);
                            if out.avg_pass_rate.unwrap_or(out.pass_rate) >= 100.0 {
                                mval["status"] = serde_json::Value::String("completed".to_string());
                            } else if mval.get("status").and_then(|v| v.as_str())
                                == Some("completed")
                            {
                                mval["status"] =
                                    serde_json::Value::String("failed_tests".to_string());
                            }
                            let _ = std::fs::write(&mpath, serde_json::to_string_pretty(&mval)?);
                        }
                    }
                    if yes {
                        rewritten += 1;
                    }
                }
            }
            if yes {
                println!(
                    "backfill: rewrote {rewritten}, re-stamped {stamped}, skipped {skipped} \
                     (all rescored rows carry the current fingerprint)"
                );
            } else {
                println!(
                    "backfill plan: {rewritten} to rewrite, {stamped} re-stamp-only, {skipped} unchanged \
                     (re-run with --yes)"
                );
            }
        }

        Commands::Migrate { results_dir, yes } => {
            use std::path::Path;
            let path = Path::new(&results_dir);
            let mut entries: Vec<String> = std::fs::read_dir(path)
                .with_context(|| format!("read results dir {results_dir}"))?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".json"))
                .collect();
            entries.sort();
            let mut runs = Vec::new();
            let mut skipped = 0usize;
            for name in &entries {
                let fp = path.join(name);
                let Ok(text) = std::fs::read_to_string(&fp) else {
                    skipped += 1;
                    continue;
                };
                match serde_json::from_str::<subdollarbench::report::BenchmarkRunResult>(&text) {
                    Ok(run) => runs.push(run),
                    Err(_) => skipped += 1,
                }
            }
            if !yes {
                println!(
                    "migrate plan: {} to import, {} skipped (re-run with --yes)",
                    runs.len(),
                    skipped
                );
                return Ok(());
            }
            let db_path = subdollarbench::report::Store::resolve_db_path();
            let store = subdollarbench::report::Store::open(&db_path)?;
            let count_before = store.parent_count();
            let mut imported = 0usize;
            let mut added = 0usize;
            for run in &runs {
                // Upserts replace same-id rows, so only genuinely new ids
                // grow the table; count those to keep re-runs idempotent.
                if !store.parent_exists(&run.id) {
                    added += 1;
                }
                store.upsert_parent(run)?;
                imported += 1;
            }
            let count_after = store.parent_count();
            let delta = count_after.saturating_sub(count_before);
            println!("migrate: imported {imported} ({added} new), skipped {skipped}, parents {count_after}");
            if delta != added {
                anyhow::bail!(
                    "migrate delta {delta} (before {count_before}, after {count_after}) != newly added {added} of {imported} files upserted; files stay authoritative"
                );
            }
            let runs_dir = subdollarbench::report::RunArchiver::resolve_runs_dir();
            subdollarbench::report::RunArchiver::cleanup_stale_tmp_dirs(&runs_dir);
            let mut run_imported = 0usize;
            let mut run_skipped = 0usize;
            if let Ok(entries) = std::fs::read_dir(&runs_dir) {
                for entry in entries.flatten() {
                    // Never import staging leftovers from a crashed archive.
                    if entry.file_name().to_string_lossy().ends_with(".tmp") {
                        continue;
                    }
                    let manifest_file = entry.path().join("manifest.json");
                    if !manifest_file.exists() {
                        continue;
                    }
                    match std::fs::read_to_string(&manifest_file).ok().and_then(|t| {
                        serde_json::from_str::<subdollarbench::report::RunManifest>(&t).ok()
                    }) {
                        Some(m) => {
                            if store.upsert_run_manifest(&m).is_ok() {
                                run_imported += 1;
                            } else {
                                run_skipped += 1;
                            }
                        }
                        None => run_skipped += 1,
                    }
                }
            }
            let run_count = store.run_manifest_count();
            println!(
                "migrate: runs {run_imported} imported, {run_skipped} skipped, total {run_count}"
            );
        }

        Commands::CostAnnotate {
            results_dir,
            runs_dir,
            yes,
        } => {
            let rep = subdollarbench::report::run_cost_annotate(
                std::path::Path::new(&results_dir),
                std::path::Path::new(&runs_dir),
                yes,
            )
            .await?;
            println!(
                "cost-annotate{}: scanned {} manifests; annotate {} manifest row(s), {} result row(s), {} db run row(s), {} db parent row(s); {} row(s) price zero or unconfirmable; {} warning(s)",
                if yes { "" } else { " (dry-run)" },
                rep.manifests_scanned,
                rep.annotated.len(),
                rep.results_annotated,
                rep.db_annotated,
                rep.db_parents_annotated,
                rep.already_free,
                rep.warnings.len(),
            );
            for line in &rep.annotated {
                println!("  + {line}");
            }
            for line in &rep.warnings {
                println!("  ! {line}");
            }
            if !yes {
                println!("  (dry run: nothing written; re-run with --yes to apply)");
            }
        }

        Commands::AnnotateCutoff { runs_dir, yes } => {
            let rep =
                subdollarbench::report::run_cutoff_annotate(std::path::Path::new(&runs_dir), yes)?;
            println!(
                "annotate-cutoff{}: scanned {} manifests; flag {} trial(s), {} db row(s); {} billing-shaped near-miss(es) excluded; {} warning(s)",
                if yes { "" } else { " (dry-run)" },
                rep.manifests_scanned,
                rep.annotated.len(),
                rep.db_annotated,
                rep.skipped_billing,
                rep.warnings.len(),
            );
            for line in &rep.annotated {
                println!("  + {line}");
            }
            for line in &rep.warnings {
                println!("  ! {line}");
            }
            if !yes {
                println!("  (dry run: nothing written; re-run with --yes to apply)");
            }
        }

        Commands::Repair {
            results_dir,
            runs_dir,
            yes,
        } => {
            let rep = subdollarbench::report::run_repair(
                std::path::Path::new(&results_dir),
                std::path::Path::new(&runs_dir),
                yes,
            )?;
            println!(
                "repair{}: scanned {} trial manifests; cured {} DQ + {} agent_killed; still-DQ {}; loose {}/{} groups/trials; consolidated {}; re-merged {}; pruned {} stale runs; {} warning(s)",
                if yes { "" } else { " (dry-run)" },
                rep.manifests_scanned,
                rep.cured_dq.len(),
                rep.cured_agent_killed.len(),
                rep.still_dq.len(),
                rep.loose_groups,
                rep.loose_trials,
                rep.consolidated.len(),
                rep.parents_remerged.len(),
                rep.pruned_runs.len(),
                rep.warnings.len(),
            );
            for line in rep
                .cured_dq
                .iter()
                .chain(rep.cured_agent_killed.iter())
                .chain(rep.still_dq.iter().take(5))
                .chain(rep.consolidated.iter())
                .chain(rep.parents_remerged.iter())
                .chain(rep.pruned_runs.iter().take(5))
                .chain(rep.skipped_existing_parent.iter())
                .chain(rep.warnings.iter())
            {
                println!("  {line}");
            }
        }

        Commands::Replay {
            run,
            port,
            runs_dir,
        } => {
            // Resolve runs/<id> by exact id or unambiguous prefix.
            let root = Path::new(&runs_dir);
            let mut hits: Vec<String> = std::fs::read_dir(root)
                .with_context(|| format!("read runs dir {runs_dir}"))?
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|id| id == &run || id.starts_with(&run))
                .collect();
            hits.sort();
            let run_id = match hits.len() {
                0 => anyhow::bail!("no run matches '{run}' in {runs_dir}"),
                1 => hits.remove(0),
                _ => {
                    eprintln!("ambiguous prefix '{run}':");
                    for h in &hits {
                        eprintln!("  {h}");
                    }
                    anyhow::bail!("{} runs match; give a longer prefix", hits.len())
                }
            };
            let run_path = root.join(&run_id);
            let manifest: RunManifest =
                serde_json::from_str(&std::fs::read_to_string(run_path.join("manifest.json"))?)
                    .with_context(|| format!("read manifest for {run_id}"))?;
            let task = parse_task(&manifest.task)?;
            let p = port.unwrap_or(match task {
                TaskType::Redis => 6379,
                TaskType::Http => 8080,
                TaskType::Pdf => 8090,
                TaskType::Wal => 6390,
                TaskType::Proxy => 8091,
                TaskType::Pubsub => 8092,
                TaskType::Binproto => 6395,
                TaskType::Queue => 6385,
                TaskType::Sql => 5480,
            });
            // Port safety: never fight a live engine over task ports.
            // Best-effort check — an unreachable engine means idle-or-down,
            // which is exactly when replay is safe.
            let server = resolve_server_flag(&None);
            if let Ok(st) = remote::get_json(&server, "/api/status").await {
                if st
                    .get("is_running")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    anyhow::bail!(
                        "engine is live; replay needs its task port ({p}) free — stop the wave or wait for idle"
                    );
                }
            }
            println!("replaying {run_id} ({task} on port {p})...");
            // Copy the archived workspace aside: the archive is evidence and
            // must never be mutated (perms relax + build artifacts stay out).
            let work_src = run_path.join("workspace");
            let work_tmp =
                std::env::temp_dir().join(format!("sdb-replay-{run_id}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&work_tmp);
            fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
                std::fs::create_dir_all(dst)?;
                for e in std::fs::read_dir(src)? {
                    let e = e?;
                    let d = dst.join(e.file_name());
                    if e.file_type()?.is_dir() {
                        copy_tree(&e.path(), &d)?;
                    } else if !e.file_type()?.is_symlink() {
                        std::fs::copy(e.path(), d)?;
                    }
                }
                Ok(())
            }
            copy_tree(&work_src, &work_tmp)
                .with_context(|| format!("copy workspace for {run_id}"))?;
            let sandbox =
                SandboxManager::with_id(&format!("replay-{}", run_id.replace(['/', ':'], "_")));
            let _guard = RunGuard::new(SandboxManager::with_id(&format!(
                "replay-{}",
                run_id.replace(['/', ':'], "_")
            )));
            if !sandbox.ensure_runnable_candidate(&work_tmp)? {
                sandbox.cleanup();
                let _ = std::fs::remove_dir_all(&work_tmp);
                anyhow::bail!("{run_id}: no Dockerfile or start.sh in archived workspace");
            }
            // Compliance re-check (reported, never gates): the archived
            // verdict stands; this tells the auditor whether it reproduces.
            match subdollarbench::verifier::compliance::ComplianceChecker::check_no_frameworks(
                &work_tmp,
            ) {
                Ok(()) => println!("compliance re-check: PASS"),
                Err(v) => println!("compliance re-check: FAIL ({v})"),
            }
            if task == TaskType::Proxy {
                sandbox
                    .start_upstream_pool(&PROXY_UPSTREAM_PORTS)
                    .context("start replay upstream pool")?;
            }
            sandbox
                .start_candidate_in_docker(&work_tmp, p)
                .context("start replay candidate")?;
            // Differential oracle needs the official reference too (redis
            // only, like the pipeline). Best-effort: a missing reference
            // degrades to a reported advisory skip, never a failure.
            if task == TaskType::Redis {
                if let Err(e) = sandbox.start_reference_redis(6380) {
                    println!("replay reference unavailable: {e:#} (differential skipped)");
                }
            }
            if !sandbox.wait_for_port(p, 25).await {
                sandbox.cleanup();
                let _ = std::fs::remove_dir_all(&work_tmp);
                anyhow::bail!(
                    "{run_id}: candidate never opened port {p}\n{}",
                    sandbox.get_candidate_logs()
                );
            }
            let replay_seed = format!(
                "replay-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let mut replay_robust = Vec::new();
            let (pass_rate, passed, total) = match task {
                TaskType::Redis => {
                    let v = RedisVerifier::new(p, Some(&replay_seed));
                    let r = v.run_all().await;
                    replay_robust = v.run_robustness().await;
                    replay_robust.extend(v.run_differential(6380).await);
                    (r.pass_rate, r.passed_count, r.total_stages)
                }
                TaskType::Http => {
                    let v = HttpVerifier::new(p, Some(&replay_seed));
                    let r = v.run_all().await;
                    replay_robust = v.run_robustness().await;
                    (r.pass_rate, r.passed_count, r.total_stages)
                }
                TaskType::Pdf => {
                    let v = PdfVerifier::new(p, Some(&replay_seed));
                    let r = v.run_all().await;
                    (r.pass_rate, r.passed_count, r.total_stages)
                }
                TaskType::Wal => {
                    let v = WalVerifier::new(p, Some(&replay_seed));
                    let r = v.run_basic().await;
                    (r.pass_rate, r.passed_count, r.total_stages)
                }
                TaskType::Queue => {
                    let v = QueueVerifier::new(p, Some(&replay_seed));
                    let r = v.run_all().await;
                    for s in &r.stages {
                        println!(
                            "  Stage {}: {} -> {} {}",
                            s.stage,
                            s.name,
                            if s.passed { "PASS" } else { "FAIL" },
                            s.error.as_deref().unwrap_or("")
                        );
                    }
                    (r.pass_rate, r.passed_count, r.total_stages)
                }
                TaskType::Proxy => {
                    let ups: Vec<(String, u16)> = (1u8..=3u8)
                        .map(|i| {
                            (
                                sandbox.upstream_name(i),
                                PROXY_UPSTREAM_PORTS[(i - 1) as usize],
                            )
                        })
                        .collect();
                    // Mirror the pipeline exactly (routing + failover +
                    // throughput stage) so the replayed pass rate is
                    // comparable with the archived one.
                    let v = ProxyVerifier::new(p, Some(&replay_seed), ups);
                    let routing = v.run_routing().await;
                    let mut stages = routing.stages.clone();
                    for s in &routing.stages {
                        println!(
                            "  Stage {}: {} -> {} {}",
                            s.stage,
                            s.name,
                            if s.passed { "PASS" } else { "FAIL" },
                            s.error.as_deref().unwrap_or("")
                        );
                    }
                    let fo = v.run_failover().await;
                    println!(
                        "  Stage {}: {} -> {} {}",
                        fo.stage,
                        fo.name,
                        if fo.passed { "PASS" } else { "FAIL" },
                        fo.error.as_deref().unwrap_or("")
                    );
                    stages.push(fo);
                    let tp = v.measure_throughput().await;
                    match tp {
                        Some(x) => println!("  Candidate load: {x:.0} req/sec"),
                        None => println!("  Candidate load: no measurement"),
                    }
                    stages.push(subdollarbench::verifier::StageResult {
                        stage: 5,
                        name: "Stage 5: Concurrent GET throughput".to_string(),
                        passed: tp.is_some(),
                        error: None,
                    });
                    let ps = stages.iter().filter(|s| s.passed).count() as u32;
                    let ts = stages.len() as u32;
                    (ps as f64 / ts as f64 * 100.0, ps, ts)
                }
                TaskType::Pubsub => {
                    let v = PubsubVerifier::new(p, Some(&replay_seed));
                    let r = v.run_all().await;
                    (r.pass_rate, r.passed_count, r.total_stages)
                }
                TaskType::Binproto => {
                    let v = BinprotoVerifier::new(p, Some(&replay_seed));
                    let r = v.run_all().await;
                    (r.pass_rate, r.passed_count, r.total_stages)
                }
                TaskType::Sql => run_sql_stages_sync(p, &replay_seed),
            };
            for s in &replay_robust {
                println!(
                    "robustness (advisory): {} -> {} {}",
                    s.name,
                    if s.passed { "PASS" } else { "FAIL" },
                    s.error.as_deref().unwrap_or("")
                );
            }
            sandbox.cleanup();
            let _ = std::fs::remove_dir_all(&work_tmp);
            println!(
                "replay: {pass_rate:.1}% ({passed}/{total}) vs archived {:.1}% ({}/{})",
                manifest.pass_rate, manifest.passed_stages, manifest.total_stages
            );
            if (pass_rate - manifest.pass_rate).abs() > 0.5 {
                println!("MISMATCH: replay disagrees with the archived verdict — investigate before citing");
            } else {
                println!("MATCH: replay reproduces the archived verdict");
            }
        }

        Commands::MigrateCheck => {
            // Report-only integrity audit: orphans, junk rows, row-file drift.
            // Runs on every engine boot (warn-only); exit 1 on findings.
            let db_path = subdollarbench::report::Store::resolve_db_path();
            let store = subdollarbench::report::Store::open(&db_path)?;
            let mut bad = 0usize;
            let orphans = store.orphan_trial_count();
            if orphans > 0 {
                println!("check: {orphans} orphan trial(s) (parent row missing)");
                bad += orphans;
            }
            let junk = store.junk_parent_count();
            if junk > 0 {
                println!("check: {junk} junk parent row(s) (test_/0-trial)");
                bad += junk;
            }
            let (db_parents, files) =
                store.parent_file_drift(&subdollarbench::config::get_repo_root().join("results"));
            if db_parents != files {
                println!("check: row-file drift (db parents {db_parents} vs files {files})");
                bad += 1;
            }
            if bad == 0 {
                println!("check: clean (0 orphans, 0 junk, rows match files)");
            }
            if bad > 0 {
                anyhow::bail!("integrity check found {bad} issue(s)");
            }
        }

        Commands::MatrixReport { results_dir } => {
            let all = LeaderboardManager::load_all(&results_dir);
            if all.is_empty() {
                println!("No benchmark results found in {}", results_dir);
            } else {
                subdollarbench::report::validate_comparison_wave(&all)?;
                LeaderboardManager::print_effort_matrix(&all);
            }
        }

        Commands::RegenReviews {
            runs_dir,
            run_id,
            model,
            api_key,
            force,
            results_dir,
            out,
            base_url,
            keep,
            rollup_only,
        } => {
            review_cmd::handle_regen_reviews(
                runs_dir,
                run_id,
                model,
                api_key,
                force,
                results_dir,
                out,
                base_url,
                keep,
                rollup_only,
            )
            .await?
        }
        Commands::ReviewRun {
            run_id,
            runs_dir,
            model,
            api_key,
            force,
        } => review_cmd::handle_review_run(run_id, runs_dir, model, api_key, force).await?,

        Commands::Portal { runs_dir, out, .. } => {
            // Round-12 P1: the legacy pre-sdb-sql page is retired; the
            // public surface is the sdb-sql board (same as the engine's
            // rebuild endpoint and the campaign finish).
            println!("{}", "Exporting the sdb-sql board...".bold().cyan());
            let summary = PortalExporter::export_sql_board(&runs_dir, &out)?;
            println!("{}", "✅ Portal exported!".bold().green());
            println!("  Output: {}", out);
            println!("  Cells:  {}", summary.run_count);
            println!("  Deploy: rsync -az --delete {}/ <vps>:/srv/bench/", out);
        }

        Commands::Sandbox { command } => match command {
            subdollarbench::config::SandboxCommands::Check => {
                println!("{}", "Checking OMP Sandbox Status...".bold().cyan());
                let status = subdollarbench::sandbox::omp_updater::check_status(true).await;
                let installed = status
                    .installed_version
                    .as_deref()
                    .unwrap_or("Not installed");
                let latest = status.latest_version.as_deref().unwrap_or("Unknown");
                println!("--------------------------------------------------");
                println!(
                    "  Docker Image:    {}",
                    if status.image_exists {
                        "Found (subdollar-sandbox)".green()
                    } else {
                        "Missing".red()
                    }
                );
                println!("  Installed OMP:   {}", installed.cyan());
                println!("  Latest on npm:   {}", latest.yellow());
                println!(
                    "  Status:          {}",
                    if status.up_to_date {
                        "Up to date ✓".green()
                    } else {
                        "Update available ⚠️".yellow()
                    }
                );
                if let Some(err) = status.last_error {
                    println!("  Registry Notice: {}", err.dimmed());
                }
                println!("--------------------------------------------------");
            }
            subdollarbench::config::SandboxCommands::Update { version } => {
                println!(
                    "{}",
                    "Updating subdollar-sandbox image with OMP...".bold().cyan()
                );
                let new_status =
                    subdollarbench::sandbox::omp_updater::update_sandbox_image(version.as_deref())
                        .await?;
                println!(
                    "{} Installed OMP: {}",
                    "✅ Sandbox successfully updated!".bold().green(),
                    new_status
                        .installed_version
                        .as_deref()
                        .unwrap_or("Unknown")
                        .green()
                );
            }
        },

        Commands::Watch {
            run,
            no_follow,
            stages_only,
            runs_dir,
        } => {
            let id = resolve_run_id(&runs_dir, &run)?;
            let dir = Path::new(&runs_dir).join(&id);
            let log_path = dir.join("console.log");
            let manifest_path = dir.join("manifest.json");
            println!("watching {id} (Ctrl-C to stop)...");
            let show = |line: &str| {
                if stages_only
                    && !(line.contains("Stage") || line.contains("PASS") || line.contains("FAIL"))
                {
                    return;
                }
                if line.contains("FAIL") || line.contains("CHEAT") {
                    println!("{}", line.red());
                } else if line.contains("Stage") && line.contains("PASS") {
                    println!("{}", line.green());
                } else {
                    println!("{line}");
                }
            };
            let mut pos: usize = 0;
            loop {
                if let Ok(text) = std::fs::read_to_string(&log_path) {
                    let from = pos.min(text.len());
                    for line in text[from..].lines() {
                        show(line);
                    }
                    pos = text.len();
                }
                if no_follow {
                    break;
                }
                if manifest_path.exists() {
                    if let Ok(text) = std::fs::read_to_string(&log_path) {
                        let from = pos.min(text.len());
                        for line in text[from..].lines() {
                            show(line);
                        }
                    }
                    println!("run archived: {id}");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }

        Commands::Runs {
            task,
            model,
            effort,
            status,
            limit,
            runs_dir,
            all,
        } => {
            let path = Path::new(&runs_dir);
            let mut rows: Vec<RunManifest> = std::fs::read_dir(path)
                .with_context(|| format!("read runs dir {runs_dir}"))?
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter(|e| all || !e.path().join(".portal_hidden").exists())
                .filter_map(|e| {
                    std::fs::read_to_string(e.path().join("manifest.json"))
                        .ok()
                        .and_then(|t| serde_json::from_str(&t).ok())
                })
                .filter(|m: &RunManifest| {
                    if let Some(t) = task.as_ref() {
                        if &m.task != t {
                            return false;
                        }
                    }
                    if let Some(pat) = model.as_ref() {
                        if !m.model.to_lowercase().contains(&pat.to_lowercase()) {
                            return false;
                        }
                    }
                    if let Some(pat) = effort.as_ref() {
                        if m.effort.as_deref().unwrap_or("").to_lowercase() != pat.to_lowercase() {
                            return false;
                        }
                    }
                    if let Some(pat) = status.as_ref() {
                        if !m.status.eq_ignore_ascii_case(pat) {
                            return false;
                        }
                    }
                    true
                })
                .collect();
            rows.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            println!(
                "{:<64} {:<22} {:<7} {:<8} {:<16} {:>6}",
                "run", "model", "effort", "task", "status", "pass%"
            );
            for m in rows.iter().take(limit) {
                println!(
                    "{:<64} {:<22} {:<7} {:<8} {:<16} {:>5.1}%",
                    m.run_id,
                    m.model.rsplit('/').next().unwrap_or(&m.model),
                    m.effort.as_deref().unwrap_or("-"),
                    m.task,
                    m.status,
                    m.pass_rate,
                );
            }
        }

        Commands::Verdict {
            run,
            json,
            runs_dir,
        } => {
            let id = resolve_run_id(&runs_dir, &run)?;
            let m = load_run_manifest(&runs_dir, &id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&m)?);
            } else {
                println!("run:     {}", id);
                println!("model:   {}", m.model);
                println!("task:    {}", m.task);
                println!("status:  {}", m.status);
                println!(
                    "stages:  {}/{} ({:.1}%)",
                    m.passed_stages, m.total_stages, m.pass_rate
                );
                if let Some(n) = m.total_trials {
                    if n > 1 {
                        let p1 = m
                            .pass_at_1
                            .map(|v| format!(" P@1 {v:.0}%"))
                            .unwrap_or_default();
                        println!("trials:  {n} merged{p1}");
                    }
                }
                if let Some(st) = m.stability.as_deref() {
                    println!("stability: {st}");
                }
                if let Some(tp) = m.throughput_req_sec {
                    match m.reference_throughput_req_sec {
                        Some(r) => println!("tput:    {tp:.0} req/s vs ref {r:.0}"),
                        None => println!("tput:    {tp:.0} req/s (no reference)"),
                    }
                }
                println!(
                    "turns:   {}",
                    m.turns.map(|t| t.to_string()).unwrap_or("-".into())
                );
                println!("spent:   {:.1}s", m.duration_seconds);
            }
            std::process::exit(verdict_code(&m));
        }

        Commands::Diff { a, b, runs_dir } => {
            let ida = resolve_run_id(&runs_dir, &a)?;
            let idb = resolve_run_id(&runs_dir, &b)?;
            let ma = load_run_manifest(&runs_dir, &ida)?;
            let mb = load_run_manifest(&runs_dir, &idb)?;
            println!("a: {ida}  ({:.1}%)", ma.pass_rate);
            println!("b: {idb}  ({:.1}%)", mb.pass_rate);
            println!("{:<5} {:<46} {:<6} {:<6}", "n", "stage", "a", "b");
            let n = ma.stages.len().max(mb.stages.len());
            for i in 0..n {
                let sa = ma.stages.get(i);
                let sb = mb.stages.get(i);
                let name = sa
                    .map(|s| s.name.as_str())
                    .or_else(|| sb.map(|s| s.name.as_str()))
                    .unwrap_or("?");
                let fa = sa
                    .map(|s| if s.passed { "PASS" } else { "FAIL" })
                    .unwrap_or("-");
                let fb = sb
                    .map(|s| if s.passed { "PASS" } else { "FAIL" })
                    .unwrap_or("-");
                println!("{:<5} {:<46} {:<6} {:<6}", i + 1, name, fa, fb);
            }
        }

        Commands::Stages {
            run,
            trial,
            json,
            runs_dir,
        } => {
            let prefix_id = resolve_run_id(&runs_dir, &run)?;
            let id = apply_trial_suffix(&prefix_id, trial);
            let m = load_run_manifest(&runs_dir, &id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&m.stages)?);
            } else {
                println!("{id}: {} {}/{}", m.status, m.passed_stages, m.total_stages);
                for s in &m.stages {
                    let mark = if s.passed { "PASS" } else { "FAIL" };
                    println!("  {} -> {}", s.name, mark);
                    if !s.passed {
                        if let Some(e) = s.error.as_deref() {
                            if !e.is_empty() {
                                println!("      {e}");
                            }
                        }
                    }
                }
            }
        }

        Commands::Prune { yes } => {
            let list = |filter: &str, fmt: &str| -> Result<Vec<String>> {
                let out = std::process::Command::new("docker")
                    .args(["ps", "-a", "--filter", filter, "--format", fmt])
                    .output()?;
                Ok(String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
                    .collect())
            };
            let containers = list("name=subdollar-", "{{.Names}}\t{{.Status}}")?;
            let networks = {
                let out = std::process::Command::new("docker")
                    .args([
                        "network",
                        "ls",
                        "--filter",
                        "name=subdollar-net-",
                        "--format",
                        "{{.Name}}",
                    ])
                    .output()?;
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
                    .collect::<Vec<_>>()
            };
            let live: Vec<&str> = containers
                .iter()
                .filter(|c| c.contains("omp-agent") && c.contains("\tUp"))
                .map(|c| c.split('\t').next().unwrap_or(""))
                .collect();
            if !live.is_empty() {
                anyhow::bail!(
                    "refusing: live agent containers running ({})",
                    live.join(", ")
                );
            }
            if !yes {
                println!(
                    "would remove {} containers, {} networks:",
                    containers.len(),
                    networks.len()
                );
                for c in &containers {
                    println!("  container {c}");
                }
                for net in &networks {
                    println!("  network   {net}");
                }
                println!("re-run with --yes to delete.");
                return Ok(());
            }
            let mut gone_c = 0;
            for c in &containers {
                let name = c.split('\t').next().unwrap_or(c);
                let st = std::process::Command::new("docker")
                    .args(["rm", "-f", name])
                    .status()?;
                if st.success() {
                    gone_c += 1;
                }
            }
            let mut gone_n = 0;
            for net in &networks {
                let st = std::process::Command::new("docker")
                    .args(["network", "rm", net])
                    .status()?;
                if st.success() {
                    gone_n += 1;
                }
            }
            println!("pruned {gone_c} containers, {gone_n} networks");
        }

        Commands::Campaign { action } => match action {
            subdollarbench::config::CampaignAction::List => main_campaign::list()?,
            subdollarbench::config::CampaignAction::DryRun { name, model } => {
                main_campaign::dry_run(&name, &model)?
            }
            subdollarbench::config::CampaignAction::Run {
                name,
                model,
                effort,
                harness,
                budget_usd,
                wall_min,
                timeout_min,
                api_key,
                eval_only,
                workspace,
                phase,
                chain,
                refork,
            } => {
                main_campaign::run(
                    name,
                    model,
                    effort,
                    harness,
                    budget_usd,
                    wall_min,
                    timeout_min,
                    api_key,
                    eval_only,
                    workspace,
                    phase,
                    chain,
                    refork,
                )
                .await?
            }
        },

        Commands::Matrix {
            models,
            efforts,
            tasks,
            trials,
            extra,
            per_effort_columns,
            enqueue,
            extend_if_unstable,
            max_trials,
            server,
            harness,
            stop_on_pass,
            feedback,
            feedback_rounds,
        } => {
            let feedback_rounds = if feedback && feedback_rounds == 0 {
                2
            } else {
                feedback_rounds
            };
            let models: Vec<String> = models
                .split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect();
            let efforts: Vec<String> = efforts
                .split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect();
            let tasks: Vec<String> = tasks
                .split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect();
            for t in &tasks {
                parse_task(t)?;
            }
            subdollarbench::report::validate_matrix_plan(&efforts, per_effort_columns)?;
            // Low-effort-first ordering: cheap capability probe before
            // expensive runs (enqueue mode; print mode keeps model-major order).
            let mut efforts_sorted = efforts.clone();
            efforts_sorted.sort_by_key(|e| match e.to_lowercase().as_str() {
                "minimal" | "none" | "off" => 0,
                "low" => 1,
                "medium" => 2,
                "high" => 3,
                "xhigh" => 4,
                "max" => 5,
                _ => 9,
            });
            if enqueue {
                if !extra.trim().is_empty() {
                    eprintln!("matrix --enqueue: --extra is print-mode only and is ignored (cells use engine defaults: 100 turns, $1.00, 30min)");
                }
                let server = resolve_server_flag(&server);
                // Pre-flight: refuse to launch into a paused engine or dead credit.
                // (Wave-launch script wraps this with tree+suite+spot-check gates.)
                let st = remote::get_json(&server, "/api/status").await.ok();
                if let Some(v) = st
                    .as_ref()
                    .and_then(|x| x.get("queue_paused"))
                    .and_then(|x| x.as_bool())
                {
                    if v {
                        anyhow::bail!("engine queue is paused (billing); resume before launching");
                    }
                }
                let bal = remote::get_json(&server, "/api/balance/check").await.ok();
                if let Some(b) = bal {
                    if b.get("ok").and_then(|v| v.as_bool()) == Some(false)
                        && b.get("unknown").is_none()
                    {
                        anyhow::bail!("engine balance gate: credit exhausted; top up to launch");
                    }
                }
                let mut ok = 0usize;
                let total = models.len() * efforts_sorted.len() * tasks.len();
                for e in &efforts_sorted {
                    for m in &models {
                        for t in &tasks {
                            let body = remote::run_request(
                                m,
                                t,
                                e,
                                harness.as_str(),
                                100,
                                1.0,
                                120,
                                None,
                                false,
                                trials,
                                extend_if_unstable,
                                max_trials,
                                false,
                                None,
                                stop_on_pass,
                                feedback_rounds,
                            );
                            match remote::post_json(&server, "/api/queue", &body).await {
                                Ok(resp) => {
                                    let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                    // Say what will actually run: stop-on-pass
                                    // clamps the dispatched batch to SCREEN_CAP_TRIALS,
                                    // so "x9" alone reads as nine trials per cell.
                                    let trial_label = if stop_on_pass {
                                        format!(
                                            "x{trials} (screening <= {})",
                                            trials.min(subdollarbench::config::SCREEN_CAP_TRIALS)
                                        )
                                    } else {
                                        format!("x{trials}")
                                    };
                                    println!("enqueued {id}: {m} {t} {e} {trial_label}");
                                    ok += 1;
                                }
                                Err(err) => eprintln!("enqueue failed {m} {t} {e}: {err:#}"),
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }
                println!("matrix enqueue: {ok}/{total} cells queued (low-effort-first)");
                return Ok(());
            }
            println!(
                "# effort x model matrix: {} models x {} efforts x {} tasks x {} trials ({} runs)",
                models.len(),
                efforts_sorted.len(),
                tasks.len(),
                trials,
                models.len() * efforts_sorted.len() * tasks.len()
            );
            println!(
                "# analysis after the wave: subdollarbench matrix-report --results-dir ./results"
            );
            let stop_flag = if stop_on_pass { " --stop-on-pass" } else { "" };
            for m in &models {
                for e in &efforts {
                    for t in &tasks {
                        let ex = if extra.trim().is_empty() {
                            String::new()
                        } else {
                            format!(" {}", extra.trim())
                        };
                        println!(
                            "subdollarbench run --model {} --task {} --effort {} --harness {} --trials {}{}{}",
                            m, t, e, harness, trials, ex, stop_flag
                        );
                    }
                }
            }
        }

        Commands::Doctor => {
            let mut fail = 0;
            match std::process::Command::new("docker").arg("info").output() {
                Ok(o) if o.status.success() => println!("docker:      ok"),
                _ => {
                    println!("docker:      FAIL (daemon unreachable)");
                    fail += 1;
                }
            }
            if fail == 0 {
                let out = std::process::Command::new("docker")
                    .args(["images", "--format", "{{.Repository}}"])
                    .output()?;
                let repos = String::from_utf8_lossy(&out.stdout);
                for img in ["subdollar-sandbox", "nginx", "redis"] {
                    if repos.lines().any(|l| l.trim() == img) {
                        println!("image {img:<18} ok");
                    } else {
                        println!("image {img:<18} MISSING");
                        fail += 1;
                    }
                }
            }
            match std::env::var("OPENROUTER_API_KEY") {
                Ok(k) if !k.trim().is_empty() => println!("api key:     set"),
                _ => {
                    println!("api key:     MISSING (OPENROUTER_API_KEY)");
                    fail += 1;
                }
            }
            for port in [6379u16, 8080, 8090, 8091, 8092, 3000] {
                match std::net::TcpListener::bind(("127.0.0.1", port)) {
                    Ok(_) => {}
                    Err(_) => println!("port {port}:     in use (may collide with a live run)"),
                }
            }
            println!("ports:       scanned (in-use lines are warnings only)");
            if fail > 0 {
                anyhow::bail!("doctor: {fail} hard check(s) failed");
            }
            println!("doctor: all green");
        }

        Commands::Stability {
            parent,
            results_dir,
        } => {
            let pid = resolve_parent_id(&results_dir, &parent)?;
            let merged = load_parent_result(&results_dir, &pid)?;
            println!("{pid}: {} trials", merged.subruns.len());
            for s in &merged.subruns {
                match s.throughput_req_sec {
                    Some(tp) => println!("  {:<52} {:>6.1}%  {:.0} req/s", s.id, s.pass_rate, tp),
                    None => println!("  {:<52} {:>6.1}%", s.id, s.pass_rate),
                }
            }
            let (stab, reason) = assess_stability(&merged.subruns);
            match stab {
                Stability::Stable => println!("stability: stable ({reason})"),
                Stability::Watch => {
                    println!("stability: watch ({reason})");
                    std::process::exit(1);
                }
                Stability::Unstable => {
                    println!("stability: unstable ({reason})");
                    std::process::exit(1);
                }
            }
        }

        Commands::Extend {
            parent,
            trials,
            model,
            task,
            effort,
            max_turns,
            budget_usd,
            timeout_min,
            api_key,
            workdir,
            eval_only,
            review_model,
            results_dir,
        } => {
            let pid = resolve_parent_id(&results_dir, &parent)?;
            let merged = load_parent_result(&results_dir, &pid)?;
            if merged.subruns.len() < 2 {
                anyhow::bail!(
                    "extend needs a multi-trial parent ({} has {} subrun(s)); re-run with --trials 2+",
                    pid,
                    merged.subruns.len()
                );
            }
            let (stab, reason) = assess_stability(&merged.subruns);
            if stab == Stability::Stable {
                println!("{pid} already stable ({reason}); nothing to do.");
                return Ok(());
            }
            println!("[EXTEND] {pid}: {reason}; running {trials} more trial(s)...");
            let task = match task {
                Some(t) => t,
                None => parse_task(&merged.task)?,
            };
            let work_path_buf = if Path::new(&workdir).is_absolute() {
                PathBuf::from(&workdir)
            } else {
                subdollarbench::config::get_repo_root().join(&workdir)
            };
            let logger = Arc::new(CliPipelineLogger);
            // Pre-absorb BEFORE numbering: stranded scored trials rejoin
            // first so max-N+1 skips their numbers (no workdir reuse), and
            // the merge below sees every trial whether or not this batch ran.
            let mut base_subruns = merged.subruns.clone();
            {
                use std::collections::HashSet;
                let known: HashSet<String> = base_subruns.iter().map(|s| s.id.clone()).collect();
                let pre = subdollarbench::web::queue::load_orphans(
                    pid.as_str(),
                    &merged.model,
                    &merged.task,
                    &known,
                    None,
                );
                if !pre.absorb.is_empty() {
                    eprintln!(
                        "  [EXTEND] pre-absorbed {} orphan scored trial(s) into the base.",
                        pre.absorb.len()
                    );
                    base_subruns.extend(pre.absorb);
                    base_subruns.sort_by_key(|r| {
                        subdollarbench::web::queue::trial_no_of(&r.id).unwrap_or(u32::MAX)
                    });
                }
            }
            let batch_start = subdollarbench::web::queue::next_trial_idx(&base_subruns);
            let batch = run_trial_batch(
                TrialBatchParams {
                    model: model.unwrap_or_else(|| merged.model.clone()),
                    harness: Harness::parse(merged.harness.as_deref().unwrap_or("omp")),
                    task,
                    effort: effort.unwrap_or_else(|| {
                        merged.effort.clone().unwrap_or_else(|| "high".to_string())
                    }),
                    max_turns,
                    budget_usd,
                    timeout_min,
                    api_key,
                    workdir: work_path_buf,
                    eval_only,
                    review_model,
                    parent_run_id: pid.clone(),
                    start_idx: batch_start,
                    count: trials.max(1),
                    feedback_rounds: merged.feedback_rounds.unwrap_or(0),
                },
                logger,
            )
            .await?;
            // Post-batch absorb (race belt) + honest empty-branch: the batch
            // may have scored nothing while stranded trials exist, and an
            // empty batch with zero errors means setup refused — all three
            // read differently from blanket "all failed".
            let attempted =
                batch_start..=(batch_start.saturating_add(trials.max(1)).saturating_sub(1));
            let known: std::collections::HashSet<String> = base_subruns
                .iter()
                .map(|s| s.id.clone())
                .chain(batch.results.iter().map(|r| r.id.clone()))
                .collect();
            let post = subdollarbench::web::queue::load_orphans(
                pid.as_str(),
                &merged.model,
                &merged.task,
                &known,
                Some(attempted),
            );
            let n_new = (batch.results.len() + post.absorb.len()) as u32;
            let mut combined = base_subruns.clone();
            combined.extend(batch.results);
            combined.extend(post.absorb);
            combined.sort_by_key(|r| {
                subdollarbench::web::queue::trial_no_of(&r.id).unwrap_or(u32::MAX)
            });
            if n_new == 0 {
                if batch.errors > 0 {
                    anyhow::bail!(
                        "extend produced no scored trials ({} errored); parent merge untouched",
                        batch.errors
                    );
                }
                anyhow::bail!(
                    "extend produced no scored trials and no errors (setup refused before any trial); parent merge untouched"
                );
            }
            let total = combined.len();
            let passing = combined.iter().filter(|r| r.pass_rate >= 100.0).count();
            let final_merged = LeaderboardManager::consolidate_multi_trial_run(
                &pid,
                &combined,
                true,
                batch.drift,
                merged.protocol.clone(),
            );
            let pass_at_1 = compute_pass_at_k(total, passing, 1) * 100.0;
            let pass_at_k = compute_pass_at_k(total, passing, total) * 100.0;
            println!("extended {pid}: {total} trials, {passing} passing, Pass@1 {pass_at_1:.1}% Pass@{total} {pass_at_k:.1}%");
            if let Some(m) = final_merged {
                let (stab2, reason2) = assess_stability(&m.subruns);
                println!("stability now: {stab2:?} ({reason2})");
            }
        }

        Commands::Status { server, json } => main_queue::handle_status(server, json).await?,

        Commands::Queue { server, json } => main_queue::handle_queue(server, json).await?,

        Commands::Ps {
            server,
            model,
            task,
            effort,
            status,
            limit,
            runs_dir,
        } => main_queue::handle_ps(server, model, task, effort, status, limit, runs_dir).await?,

        Commands::Enqueue {
            model,
            task,
            effort,
            harness,
            max_turns,
            budget_usd,
            timeout_min,
            api_key,
            eval_only,
            trials,
            extend_if_unstable,
            max_trials,
            no_save,
            review_model,
            server,
            stop_on_pass,
            feedback,
            feedback_rounds,
        } => {
            let feedback_rounds = if feedback && feedback_rounds == 0 {
                2
            } else {
                feedback_rounds
            };
            main_queue::handle_enqueue(
                model,
                task,
                effort,
                harness,
                max_turns,
                budget_usd,
                timeout_min,
                api_key,
                eval_only,
                trials,
                extend_if_unstable,
                max_trials,
                no_save,
                review_model,
                server,
                stop_on_pass,
                feedback_rounds,
            )
            .await?
        }

        Commands::QueueExtend {
            parent,
            trials,
            max_turns,
            budget_usd,
            timeout_min,
            eval_only,
            results_dir,
            server,
        } => {
            main_queue::handle_queue_extend(
                parent,
                trials,
                max_turns,
                budget_usd,
                timeout_min,
                eval_only,
                results_dir,
                server,
            )
            .await?
        }

        Commands::Dequeue { id, server } => main_queue::handle_dequeue(id, server).await?,

        Commands::QueueClear { server } => main_queue::handle_queue_clear(server).await?,

        Commands::Stop { run, server } => main_queue::handle_stop(run, server).await?,

        Commands::Log {
            run: _,
            tail,
            stages_only,
            follow,
            server,
        } => {
            let server = resolve_server_flag(&server);
            // Poll the in-memory buffer (pre-flush live tail); --follow ends
            // when the engine goes idle. Read-only against the engine.
            let show = |line: &str| {
                if stages_only
                    && !(line.contains("Stage") || line.contains("PASS") || line.contains("FAIL"))
                {
                    return;
                }
                if line.contains("FAIL") || line.contains("CHEAT") {
                    println!("{}", line.red());
                } else if line.contains("Stage") && line.contains("PASS") {
                    println!("{}", line.green());
                } else {
                    println!("{line}");
                }
            };
            let mut since: Option<usize> = None;
            loop {
                let (total, body) = remote::get_console(&server, tail, since).await?;
                for line in body.lines() {
                    show(line);
                }
                let status = remote::get_json(&server, "/api/status").await?;
                let idle = !status
                    .get("is_running")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                since = Some(total);
                if !follow || idle {
                    if follow && idle {
                        println!("engine idle.");
                    }
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
        }

        Commands::Detail {
            run,
            json,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let id = resolve_run_id(&runs_dir, &run)?;
            let v = remote::get_json(&server, &format!("/api/runs/{id}")).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", remote::format_detail(&v));
            }
        }

        Commands::Sdiff {
            a,
            b,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let ida = resolve_run_id(&runs_dir, &a)?;
            let idb = resolve_run_id(&runs_dir, &b)?;
            let v =
                remote::get_json(&server, &format!("/api/compare?run_a={ida}&run_b={idb}")).await?;
            print!("{}", remote::format_compare(&v));
        }

        Commands::Ls {
            run,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let id = resolve_run_id(&runs_dir, &run)?;
            let v = remote::get_json(&server, &format!("/api/runs/{id}/files")).await?;
            let files = v.as_array().cloned().unwrap_or_default();
            for f in &files {
                let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                let size = f.get("size_bytes").and_then(|n| n.as_u64()).unwrap_or(0);
                println!("{size:>10}  {name}");
            }
        }

        Commands::Cat {
            run,
            file,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let id = resolve_run_id(&runs_dir, &run)?;
            print!("{}", remote::get_file_text(&server, &id, &file).await?);
        }

        Commands::Review {
            run,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let id = resolve_run_id(&runs_dir, &run)?;
            let v = remote::get_json(&server, &format!("/api/runs/{id}/review")).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }

        Commands::Lboard { task, json, server } => {
            let server = resolve_server_flag(&server);
            let v = remote::get_json(&server, &format!("/api/leaderboard?task={task}")).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let rows = v.as_array().cloned().unwrap_or_default();
                println!("{:<64} {:<8} {:>6}", "run", "task", "pass%");
                for r in &rows {
                    let id = r.get("id").and_then(|s| s.as_str()).unwrap_or("?");
                    let t = r.get("task").and_then(|s| s.as_str()).unwrap_or("?");
                    let p = r.get("pass_rate").and_then(|n| n.as_f64()).unwrap_or(0.0);
                    println!("{id:<64} {t:<8} {p:>5.1}%");
                }
            }
        }

        Commands::Prompt { task, save, server } => {
            let server = resolve_server_flag(&server);
            if save {
                let mut body = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)?;
                println!(
                    "{}",
                    remote::post_text(&server, &format!("/api/prompt/{task}"), body).await?
                );
            } else {
                print!(
                    "{}",
                    remote::get_text(&server, &format!("/api/prompt/{task}")).await?
                );
            }
        }

        Commands::Env { server } => {
            let server = resolve_server_flag(&server);
            let v = remote::get_json(&server, "/api/env").await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }

        Commands::Models {
            family,
            search,
            sort,
            limit,
            json,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let v = remote::get_json(&server, "/api/models").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!(
                    "{}",
                    remote::format_models_filtered(
                        &v,
                        family.as_deref(),
                        search.as_deref(),
                        &sort,
                        limit,
                    )
                );
            }
        }

        Commands::Omp { server } => {
            let server = resolve_server_flag(&server);
            let v = remote::get_json(&server, "/api/sandbox/omp").await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }

        Commands::OmpUpdate { version, server } => {
            let server = resolve_server_flag(&server);
            let body = serde_json::json!({ "version": version });
            let v = remote::post_json(&server, "/api/sandbox/omp/update", &body).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }

        Commands::PortalRebuild { server } => {
            let server = resolve_server_flag(&server);
            println!(
                "{}",
                remote::post_empty(&server, "/api/portal/rebuild").await?
            );
        }

        Commands::Hide {
            run,
            show,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let id = resolve_run_id(&runs_dir, &run)?;
            let body = serde_json::json!({ "hidden": !show });
            let v = remote::post_json(&server, &format!("/api/runs/{id}/portal"), &body).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }

        Commands::Poison {
            run,
            cure,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let id = resolve_run_id(&runs_dir, &run)?;
            let body = serde_json::json!({ "poisoned": !cure });
            let v = remote::post_json(&server, &format!("/api/runs/{id}/poison"), &body).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }

        Commands::Rm {
            run,
            yes,
            runs_dir,
            server,
        } => {
            let server = resolve_server_flag(&server);
            let id = resolve_run_id(&runs_dir, &run)?;
            if !yes {
                anyhow::bail!("refusing to delete {id} without --yes");
            }
            println!(
                "{}",
                remote::delete_text(&server, &format!("/api/runs/{id}")).await?
            );
        }

        Commands::Ui {
            port,
            host,
            engine_url,
        } => {
            web::UiServer::start(&host, port, &engine_url).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use subdollarbench::report::{BenchmarkRunResult, RunManifest};

    #[tokio::test]
    async fn test_main_cli_leaderboard() {
        let temp_dir = std::env::temp_dir().join(format!("test_main_lb_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        let cli_empty = Cli {
            command: Commands::Leaderboard {
                results_dir: temp_dir.to_string_lossy().to_string(),
                task: "all".to_string(),
            },
        };
        assert!(run_cli(cli_empty).await.is_ok());

        let result = BenchmarkRunResult {
            id: "test_run_main_1".to_string(),
            model: "test_model".to_string(),
            task: "redis".to_string(),
            language: "Rust".to_string(),
            effort: Some("low".to_string()),
            effort_effective: None,
            status: None,
            harness: None,
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            throughput_req_sec: Some(1000.0),
            reference_throughput_req_sec: Some(71000.0),
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            prompt_tokens: 100,
            cached_tokens: 50,
            completion_tokens: 20,
            total_tokens: 170,
            total_cost_usd: 0.01,
            cost_source: None,
            cost_formula_usd: None,
            effective_cost_usd: Some(0.015),
            duration_seconds: Some(60.0),
            turns: Some(4),
            savings_percent: 10.0,
            efficiency_score: 100.0,
            throughput_score: Some(102.8),
            timestamp: "2026-09-04T12:00:00Z".to_string(),
            method_version: Some("0.1.0".to_string()),
            rules_version: None,
            graded_under: None,
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            pass_at_k: None,
            avg_pass_rate: None,
            stability: None,
            frontier_fraction: None,
            protocol: None,
            solved: None,
            solved_at: None,
            best_stage_score: None,
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        };
        let _ = LeaderboardManager::save_result(&temp_dir.to_string_lossy(), &result);

        let cli_with_res = Cli {
            command: Commands::Leaderboard {
                results_dir: temp_dir.to_string_lossy().to_string(),
                task: "redis".to_string(),
            },
        };
        assert!(run_cli(cli_with_res).await.is_ok());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_main_cli_eval() {
        let cli_redis = Cli {
            command: Commands::Eval {
                task: TaskType::Redis,
                port: Some(59997),
            },
        };
        assert!(run_cli(cli_redis).await.is_ok());

        let cli_http = Cli {
            command: Commands::Eval {
                task: TaskType::Http,
                port: Some(59998),
            },
        };
        assert!(run_cli(cli_http).await.is_ok());
    }

    /// Round-8 finding 2: the argv the console launcher spawns must be
    /// accepted by the real clap definition (a stale argv once reached a
    /// spawn and died in libtest's parser instead).
    #[test]
    fn launcher_argv_parses_as_the_campaign_run_command() {
        let args = subdollarbench::campaign::launcher_args(
            "openrouter/meta/muse-spark-1.3-contributor",
            "high",
            0.75,
            120,
        );
        let cli = Cli::try_parse_from(
            std::iter::once("subdollarbench").chain(args.iter().map(String::as_str)),
        )
        .expect("the launcher argv must parse");
        match cli.command {
            Commands::Campaign {
                action:
                    subdollarbench::config::CampaignAction::Run {
                        name,
                        model,
                        effort,
                        budget_usd,
                        wall_min,
                        timeout_min,
                        ..
                    },
            } => {
                assert_eq!(name, "sdb-sql");
                assert_eq!(model, "openrouter/meta/muse-spark-1.3-contributor");
                assert_eq!(effort, "high");
                assert_eq!(budget_usd, 0.75);
                assert_eq!(wall_min, Some(120));
                assert_eq!(timeout_min, 20);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // P1: the phase/chain form parses too.
        let args = subdollarbench::campaign::launcher_args_with_phase(
            "openrouter/meta/muse-spark-1.3-contributor",
            "high",
            0.99,
            150,
            Some(2),
            Some("campaign_prev"),
            None,
        );
        let cli = Cli::try_parse_from(
            std::iter::once("subdollarbench").chain(args.iter().map(String::as_str)),
        )
        .expect("the phase/chain launcher argv must parse");
        match cli.command {
            Commands::Campaign {
                action: subdollarbench::config::CampaignAction::Run { phase, chain, .. },
            } => {
                assert_eq!(phase, Some(2));
                assert_eq!(chain.as_deref(), Some("campaign_prev"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    /// Round-12 P1: `portal` exports the sdb-sql board. The legacy pre-
    /// sdb-sql page (with its per-run pages, console logs and
    /// results.json) can no longer be written anywhere.
    #[tokio::test]
    async fn test_main_cli_portal_export() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_main_portal_{}", std::process::id()));
        let runs_dir = temp_dir.join("runs");
        let results_dir = temp_dir.join("results");
        let out_dir = temp_dir.join("portal");
        // A legacy redis run must never publish...
        let legacy = runs_dir.join("redis_test_model_20260910_000000");
        fs::create_dir_all(legacy.join("workspace")).unwrap();
        fs::write(legacy.join("workspace/main.py"), "print(1)").unwrap();
        fs::write(legacy.join("console.log"), "key=sk-or-v1-SECRET123 done").unwrap();
        // ...a campaign cell must.
        let cell = runs_dir.join("campaign_sdb-sql_test_model_20261018_000000");
        fs::create_dir_all(&cell).unwrap();
        fs::write(
            cell.join("campaign.json"),
            serde_json::json!({
                "campaign_id": "campaign_sdb-sql_test_model_20261018_000000",
                "name": "sdb-sql",
                "model": "test-model",
                "harness": "omp",
                "effort": "high",
                "task": "sql",
                "started_at": "2026-10-18T00:00:00Z",
                "finished_at": "2026-10-18T00:30:00Z",
                "stages": [],
                "reached": 2,
                "regression_free_tickets": 2,
                "points": 3.0,
                "max_points": 36.0,
                "rules_version": subdollarbench::report::rules::RULES_VERSION,
                "total_cost_usd": 0.05,
                "over_budget": false,
                "over_wall": false
            })
            .to_string(),
        )
        .unwrap();

        let cli = Cli {
            command: Commands::Portal {
                runs_dir: runs_dir.to_string_lossy().to_string(),
                results_dir: results_dir.to_string_lossy().to_string(),
                out: out_dir.to_string_lossy().to_string(),
                base_url: "".to_string(),
                keep: 50,
            },
        };
        assert!(run_cli(cli).await.is_ok());

        let index = fs::read_to_string(out_dir.join("index.html")).unwrap();
        assert!(index.contains("sdb-sql campaign"), "{index}");
        assert!(index.contains("test-model"), "{index}");
        assert!(
            !index.contains("redis_test_model"),
            "legacy runs never publish: {index}"
        );
        assert!(!out_dir.join("runs").exists(), "no per-run pages any more");
        assert!(!out_dir.join("results.json").exists());
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_main_cli_run_eval_only_redis() {
        // Replay resolution errors need no docker and no engine: unknown id
        // and ambiguous prefixes fail before any container or HTTP call.
        let empty_runs =
            std::env::temp_dir().join(format!("test_replay_empty_{}", std::process::id()));
        let _ = fs::create_dir_all(&empty_runs);
        let cli_none = Cli {
            command: Commands::Replay {
                run: "nope".to_string(),
                port: None,
                runs_dir: empty_runs.to_string_lossy().to_string(),
            },
        };
        assert!(run_cli(cli_none).await.is_err());
        let amb_runs = std::env::temp_dir().join(format!("test_replay_amb_{}", std::process::id()));
        let _ = fs::create_dir_all(amb_runs.join("redis_aaa_w0"));
        let _ = fs::create_dir_all(amb_runs.join("redis_aab_w0"));
        let cli_amb = Cli {
            command: Commands::Replay {
                run: "redis_aa".to_string(),
                port: None,
                runs_dir: amb_runs.to_string_lossy().to_string(),
            },
        };
        assert!(run_cli(cli_amb).await.is_err());
        let _ = fs::remove_dir_all(&empty_runs);
        let _ = fs::remove_dir_all(&amb_runs);

        let temp_workdir =
            std::env::temp_dir().join(format!("test_main_run_redis_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_workdir);

        let cli_run = Cli {
            command: Commands::Run {
                model: "openrouter/meta/muse-spark-1.3-contributor".to_string(),
                task: TaskType::Redis,
                effort: "low".to_string(),
                harness: Harness::Omp,
                max_turns: 1,
                budget_usd: 0.10,
                timeout_min: 1,
                api_key: None,
                workdir: temp_workdir.to_string_lossy().to_string(),
                eval_only: true,
                trials: 2,
                no_save: true,
                queue: false,
                server: Some("http://127.0.0.1:3000".to_string()),
                review_model: None,
                extend_if_unstable: false,
                max_trials: 9,
                stop_on_pass: false,
                feedback: false,
                feedback_rounds: 0,
            },
        };
        assert!(run_cli(cli_run).await.is_ok());

        let _ = fs::remove_dir_all(&temp_workdir);
    }

    #[tokio::test]
    async fn test_main_cli_run_eval_only_http_with_candidate() {
        let temp_workdir =
            std::env::temp_dir().join(format!("test_main_run_http_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_workdir);
        let start_sh = temp_workdir.join("start.sh");
        fs::write(&start_sh, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&start_sh, fs::Permissions::from_mode(0o755));
        }

        let cli_run = Cli {
            command: Commands::Run {
                model: "openrouter/meta/muse-spark-1.3-contributor".to_string(),
                task: TaskType::Http,
                effort: "low".to_string(),
                harness: Harness::Omp,
                max_turns: 1,
                budget_usd: 0.10,
                timeout_min: 1,
                api_key: None,
                workdir: temp_workdir.to_string_lossy().to_string(),
                eval_only: true,
                trials: 1,
                no_save: true,
                queue: false,
                server: Some("http://127.0.0.1:3000".to_string()),
                review_model: None,
                extend_if_unstable: false,
                max_trials: 9,
                stop_on_pass: false,
                feedback: false,
                feedback_rounds: 0,
            },
        };
        assert!(run_cli(cli_run).await.is_ok());

        let _ = fs::remove_dir_all(&temp_workdir);
    }

    #[tokio::test]
    async fn test_main_cli_compliance_disqualification() {
        let temp_workdir =
            std::env::temp_dir().join(format!("test_main_run_cheat_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_workdir);
        let start_sh = temp_workdir.join("start.sh");
        fs::write(&start_sh, "#!/bin/sh\nredis-server --port 6379\n").unwrap();

        let cli_run = Cli {
            command: Commands::Run {
                model: "openrouter/meta/muse-spark-1.3-contributor".to_string(),
                task: TaskType::Redis,
                effort: "low".to_string(),
                harness: Harness::Omp,
                max_turns: 1,
                budget_usd: 0.10,
                timeout_min: 1,
                api_key: None,
                workdir: temp_workdir.to_string_lossy().to_string(),
                eval_only: true,
                trials: 1,
                no_save: true,
                queue: false,
                server: Some("http://127.0.0.1:3000".to_string()),
                review_model: None,
                extend_if_unstable: false,
                max_trials: 9,
                stop_on_pass: false,
                feedback: false,
                feedback_rounds: 0,
            },
        };
        assert!(run_cli(cli_run).await.is_ok());

        let _ = fs::remove_dir_all(&temp_workdir);
    }

    #[tokio::test]
    async fn test_main_cli_ui_cancelled() {
        let cli_ui = Cli {
            command: Commands::Ui {
                host: "127.0.0.1".to_string(),
                port: 0,
                engine_url: String::new(),
            },
        };
        let _ = tokio::time::timeout(Duration::from_millis(150), run_cli(cli_ui)).await;
    }

    fn test_manifest_with_status(status: &str) -> RunManifest {
        serde_json::from_value(serde_json::json!({
            "run_id": "redis_m_20260917_000000_trial1",
            "model": "m",
            "task": "redis",
            "status": status,
            "language": "Go",
            "started_at": "2026-09-17T00:00:00Z",
            "completed_at": "2026-09-17T00:01:00Z",
            "duration_seconds": 60.0,
            "pass_rate": 100.0,
            "passed_stages": 6,
            "total_stages": 6,
            "stages": [],
            "throughput_req_sec": 35000.0,
            "tokens": {"prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1, "total_tokens": 2},
            "cost_usd": 0.01,
            "savings_percent": 0.0,
            "efficiency_score": 1.0,
            "files": []
        }))
        .unwrap()
    }

    #[test]
    fn test_verdict_code_mapping() {
        assert_eq!(verdict_code(&test_manifest_with_status("completed")), 0);
        assert_eq!(verdict_code(&test_manifest_with_status("failed_tests")), 1);
        assert_eq!(
            verdict_code(&test_manifest_with_status("disqualified_cheat")),
            1
        );
        assert_eq!(verdict_code(&test_manifest_with_status("agent_failed")), 2);
        assert_eq!(verdict_code(&test_manifest_with_status("agent_killed")), 2);
        assert_eq!(verdict_code(&test_manifest_with_status("mystery")), 2);
    }

    fn test_result_with_pass_rate(pass: f64) -> subdollarbench::report::BenchmarkRunResult {
        serde_json::from_value(serde_json::json!({
            "id": "t", "model": "m", "task": "redis", "language": "Go",
            "effort": null, "pass_rate": pass, "passed_stages": 1, "total_stages": 1,
            "throughput_req_sec": null, "reference_throughput_req_sec": null,
            "baseline_source": null, "ref_drift": null, "ref_image": null,
            "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0,
            "total_tokens": 0, "total_cost_usd": 0.0, "effective_cost_usd": null,
            "duration_seconds": null, "turns": null, "savings_percent": 0.0,
            "efficiency_score": 0.0, "throughput_score": null,
            "timestamp": "2026-09-30T00:00:00Z", "method_version": null,
            "rules_version": null, "subruns": [], "total_trials": null,
            "pass_at_1": null, "pass_at_k": null, "avg_pass_rate": null,
            "pass_at_1_lo": null, "pass_at_1_hi": null, "stability": null,
            "frontier_fraction": null
        }))
        .unwrap()
    }

    /// A harness fault is a non-result in both directions. This fails if the
    /// `infra_error` arm is dropped from `trial_accounting` - the case that
    /// motivated it is a run returning `Ok` with a harness fault logged, which
    /// used to be scored as a 0% trial inside the pass@k denominator.
    #[test]
    fn test_infra_fault_is_a_non_result_in_both_directions() {
        let mk = |pass: f64, infra: bool| PipelineOutput {
            manifest: test_manifest_with_status(if infra { "infra_error" } else { "completed" }),
            benchmark_result: test_result_with_pass_rate(pass),
            agent_failed: false,
            disqualified: false,
            infra_error: infra,
            harness_fault: None,
            max_call_usd: 0.0,
            stop_reason: None,
        };

        assert_eq!(trial_accounting(&mk(100.0, false)), (false, true));
        assert_eq!(trial_accounting(&mk(0.0, false)), (false, false));

        // A harness fault is never passing, whatever pass_rate it carries.
        assert_eq!(trial_accounting(&mk(0.0, true)), (true, false));
        assert_eq!(
            trial_accounting(&mk(100.0, true)),
            (true, false),
            "an infra fault must not be able to contribute a passing trial"
        );
    }

    /// The archived status must distinguish a harness fault from a model
    /// verdict, or a stored 0% is indistinguishable from a real failure.
    #[test]
    fn test_infra_error_status_maps_to_infra_exit_code() {
        assert_eq!(verdict_code(&test_manifest_with_status("infra_error")), 2);
        assert_eq!(verdict_code(&test_manifest_with_status("failed_tests")), 1);
    }

    #[test]
    fn test_apply_trial_suffix() {
        assert_eq!(
            apply_trial_suffix("redis_m_20260917", Some(2)),
            "redis_m_20260917_trial2"
        );
        assert_eq!(
            apply_trial_suffix("redis_m_20260917_trial1", Some(2)),
            "redis_m_20260917_trial1"
        );
        assert_eq!(
            apply_trial_suffix("redis_m_20260917", None),
            "redis_m_20260917"
        );
    }

    #[test]
    fn test_resolve_run_id_exact_prefix_ambiguous() {
        let dir = std::env::temp_dir().join(format!("test_runs_{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join("redis_m_aaa_trial1"));
        let _ = fs::create_dir_all(dir.join("redis_m_aaa_trial2"));
        let _ = fs::create_dir_all(dir.join("http_m_bbb"));
        let s = dir.to_string_lossy().to_string();
        assert_eq!(
            resolve_run_id(&s, "redis_m_aaa_trial1").unwrap(),
            "redis_m_aaa_trial1"
        );
        assert_eq!(resolve_run_id(&s, "http_m").unwrap(), "http_m_bbb");
        assert!(resolve_run_id(&s, "redis_m_aaa").is_err());
        assert!(resolve_run_id(&s, "wal_").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Backfill grader provenance: the stored parent grader rides the
    /// re-merge untouched (never a trial stamp, never the fresh score
    /// stamp). Guards the 420/420 false-provenance incident.
    #[test]
    fn test_backfill_grader_round_trip() {
        // Multi-trial: explicit param wins verbatim.
        let merged = subdollarbench::report::BenchmarkRunResult::merge_subruns(
            "qp",
            vec![
                test_result_with_pass_rate(100.0),
                test_result_with_pass_rate(0.0),
            ],
            None,
            Some("fp-witness".to_string()),
        )
        .unwrap();
        assert_eq!(merged.graded_under.as_deref(), Some("fp-witness"));
        // Honest None stays None (never derived from rules_version).
        let merged2 = subdollarbench::report::BenchmarkRunResult::merge_subruns(
            "qp2",
            vec![
                test_result_with_pass_rate(100.0),
                test_result_with_pass_rate(0.0),
            ],
            None,
            None,
        )
        .unwrap();
        assert_eq!(merged2.graded_under, None);
    }

    #[tokio::test]
    async fn test_main_cli_sandbox_check() {
        let cli = Cli {
            command: Commands::Sandbox {
                command: subdollarbench::config::SandboxCommands::Check,
            },
        };
        assert!(run_cli(cli).await.is_ok());
    }

    // ---- Offline CLI coverage ------------------------------------------
    //
    // Every test below uses its own pid-tagged temp dir and only offline or
    // explicitly closed-port (`http://127.0.0.1:9`) command variants: none
    // of them may talk to the live engine on 127.0.0.1:3000, mutate the
    // repo runs/results dirs, or write bench.db rows.

    /// Shadow of the parent `run_cli`: the dispatch future is huge (the
    /// whole command match lives inside it), and a test coroutine that
    /// awaits many inline copies overflows the default test-thread stack.
    /// Boxing keeps one future on the heap per call instead.
    async fn run_cli(cli: Cli) -> Result<()> {
        Box::pin(super::run_cli(cli)).await
    }

    /// Fresh uniquely named temp dir (removed first in case a prior run of
    /// the same test crashed mid-way).
    fn test_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("test_main_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Serializes every test that mutates `SDB_HOME`/`SDB_BENCH_DB`: the env
    /// is process-wide, so a racing test would resolve repo dirs under the
    /// wrong root mid-command. Async mutex so the guard can be held across
    /// the command await without tripping `await_holding_lock`.
    pub(crate) static SDB_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Points `SDB_HOME` and `SDB_BENCH_DB` at a fresh temp tree for the
    /// test's lifetime and restores the previous values on drop. Everything
    /// that resolves repo dirs (canonical results/runs, bench.db, campaigns)
    /// then reads/writes only the temp tree.
    pub(crate) struct SdbEnv {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        pub(crate) root: PathBuf,
        prev_home: Option<std::ffi::OsString>,
        prev_db: Option<std::ffi::OsString>,
    }

    impl SdbEnv {
        pub(crate) async fn new(tag: &str) -> Self {
            let lock = SDB_ENV_LOCK.lock().await;
            // Resolve the real repo root before the env flip so the read-only
            // graded inputs stay reachable: a concurrently running test may
            // call rules::fingerprint() while our temp root is active.
            let real_root = subdollarbench::config::get_repo_root();
            let root = test_tmp_dir(tag);
            #[cfg(unix)]
            for name in ["tasks", "src"] {
                let target = real_root.join(name);
                if target.exists() {
                    let _ = std::os::unix::fs::symlink(&target, root.join(name));
                }
            }
            #[cfg(not(unix))]
            let _ = &real_root;
            let prev_home = std::env::var_os("SDB_HOME");
            let prev_db = std::env::var_os("SDB_BENCH_DB");
            std::env::set_var("SDB_HOME", &root);
            // Blank, not absent: Store falls back to `$SDB_HOME/bench.db`,
            // while LeaderboardManager::use_db stays false for foreign temp
            // dirs. A non-blank override would flip concurrent tests onto our
            // temp DB and change what their results dirs load.
            std::env::set_var("SDB_BENCH_DB", "");
            Self {
                _lock: lock,
                root,
                prev_home,
                prev_db,
            }
        }
    }

    impl Drop for SdbEnv {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("SDB_HOME", v),
                None => std::env::remove_var("SDB_HOME"),
            }
            match self.prev_db.take() {
                Some(v) => std::env::set_var("SDB_BENCH_DB", v),
                None => std::env::remove_var("SDB_BENCH_DB"),
            }
        }
    }

    /// Write a result row as the export copy the CLI scanners read.
    fn write_result_row(dir: &Path, result: &BenchmarkRunResult) {
        fs::write(
            dir.join(format!("{}.json", result.id)),
            serde_json::to_string_pretty(result).unwrap(),
        )
        .unwrap();
    }

    /// Write a full RunManifest fixture into runs/<id>/manifest.json.
    fn write_manifest(runs_dir: &Path, id: &str, mutate: impl FnOnce(&mut RunManifest)) -> PathBuf {
        let mut m = test_manifest_with_status("completed");
        m.run_id = id.to_string();
        mutate(&mut m);
        let dir = runs_dir.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();
        dir
    }

    /// Manifest with two stages (one failing when pass_rate < 100).
    fn manifest_fixture(id: &str, pass_rate: f64) -> RunManifest {
        let mut m = test_manifest_with_status(if pass_rate >= 100.0 {
            "completed"
        } else {
            "failed_tests"
        });
        m.run_id = id.to_string();
        m.model = "openrouter/meta/muse-spark".to_string();
        m.task = "redis".to_string();
        m.effort = Some("low".to_string());
        m.pass_rate = pass_rate;
        m.passed_stages = if pass_rate >= 100.0 { 2 } else { 1 };
        m.total_stages = 2;
        m.stages = vec![
            subdollarbench::verifier::StageResult {
                stage: 1,
                name: "Stage 1: basic protocol".to_string(),
                passed: true,
                error: None,
            },
            subdollarbench::verifier::StageResult {
                stage: 2,
                name: "Stage 2: dbsize".to_string(),
                passed: pass_rate >= 100.0,
                error: Some("expected 1 key".to_string()),
            },
        ];
        m
    }

    /// Consolidated parent result with one subrun per pass rate. Equal pass
    /// rates and no throughput make it `Stability::Stable` by construction
    /// (so `stability`/`extend` never hit their `process::exit` arms).
    fn parent_result(id: &str, pass_rates: &[f64]) -> BenchmarkRunResult {
        let mut parent = test_result_with_pass_rate(pass_rates[0]);
        parent.id = id.to_string();
        parent.model = "openrouter/meta/muse-spark".to_string();
        parent.task = "redis".to_string();
        parent.effort = Some("low".to_string());
        parent.subruns = pass_rates
            .iter()
            .enumerate()
            .map(|(i, pass)| {
                let mut sub = test_result_with_pass_rate(*pass);
                sub.id = format!("{id}_trial{}", i + 1);
                sub.model = parent.model.clone();
                sub.task = parent.task.clone();
                sub
            })
            .collect();
        parent
    }

    #[tokio::test]
    async fn test_main_cli_runs_views() {
        let runs = test_tmp_dir("runs_views");
        write_manifest(&runs, "redis_alpha_20261001", |m| {
            m.model = "openrouter/meta/muse-spark".to_string();
            m.task = "redis".to_string();
            m.effort = Some("low".to_string());
            m.status = "completed".to_string();
            m.pass_rate = 100.0;
            m.started_at = "2026-10-01T00:00:00Z".to_string();
        });
        write_manifest(&runs, "http_beta_20261002", |m| {
            m.model = "openrouter/z-ai/glm".to_string();
            m.task = "http".to_string();
            m.effort = Some("high".to_string());
            m.status = "failed_tests".to_string();
            m.pass_rate = 50.0;
            m.started_at = "2026-10-02T00:00:00Z".to_string();
        });
        let hidden = write_manifest(&runs, "redis_gamma_20261003", |m| {
            m.started_at = "2026-10-03T00:00:00Z".to_string();
        });
        fs::write(hidden.join(".portal_hidden"), "").unwrap();
        let dir = runs.to_string_lossy().to_string();

        // Unfiltered view: portal-hidden runs are dropped without --all.
        let cli = Cli {
            command: Commands::Runs {
                task: None,
                model: None,
                effort: None,
                status: None,
                limit: 20,
                all: false,
                runs_dir: dir.clone(),
            },
        };
        assert!(run_cli(cli).await.is_ok());

        // Every filter arm at once, plus --limit/--all.
        let cli = Cli {
            command: Commands::Runs {
                task: Some("redis".to_string()),
                model: Some("muse".to_string()),
                effort: Some("high".to_string()),
                status: Some("failed_tests".to_string()),
                limit: 1,
                all: true,
                runs_dir: dir,
            },
        };
        assert!(run_cli(cli).await.is_ok());

        // A missing runs dir is a clean error, never a panic.
        let cli = Cli {
            command: Commands::Runs {
                task: None,
                model: None,
                effort: None,
                status: None,
                limit: 20,
                all: false,
                runs_dir: runs.join("missing").to_string_lossy().to_string(),
            },
        };
        assert!(run_cli(cli).await.is_err());
        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_watch_fixture() {
        let runs = test_tmp_dir("watch");
        let dir = write_manifest(&runs, "redis_watch_20261004", |m| {
            m.started_at = "2026-10-04T00:00:00Z".to_string();
        });
        fs::write(
            dir.join("console.log"),
            "Stage 1: PASS ok\nStage 2: FAIL bad\nplain line\n",
        )
        .unwrap();
        let runs_s = runs.to_string_lossy().to_string();

        // --no-follow prints the current log once and returns.
        let cli = Cli {
            command: Commands::Watch {
                run: "redis_watch".to_string(),
                no_follow: true,
                stages_only: false,
                runs_dir: runs_s.clone(),
            },
        };
        assert!(run_cli(cli).await.is_ok());

        // An archived manifest ends the follow loop without waiting on sleep.
        let cli = Cli {
            command: Commands::Watch {
                run: "redis_watch_20261004".to_string(),
                no_follow: false,
                stages_only: true,
                runs_dir: runs_s.clone(),
            },
        };
        assert!(run_cli(cli).await.is_ok());

        // Unknown run id is an error.
        let cli = Cli {
            command: Commands::Watch {
                run: "nope".to_string(),
                no_follow: true,
                stages_only: false,
                runs_dir: runs_s,
            },
        };
        assert!(run_cli(cli).await.is_err());
        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_diff_stages_fixture_and_missing_ids() {
        let runs = test_tmp_dir("diff_stages");
        for m in [
            manifest_fixture("redis_a_20261001", 100.0),
            manifest_fixture("redis_b_20261002", 50.0),
        ] {
            let d = runs.join(&m.run_id);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("manifest.json"), serde_json::to_string(&m).unwrap()).unwrap();
        }
        let runs_s = runs.to_string_lossy().to_string();

        // Diff: exact id on one side, unambiguous prefix on the other.
        let cli = Cli {
            command: Commands::Diff {
                a: "redis_a".to_string(),
                b: "redis_b_20261002".to_string(),
                runs_dir: runs_s.clone(),
            },
        };
        assert!(run_cli(cli).await.is_ok());

        // Stages: human-readable and JSON.
        for json in [false, true] {
            let cli = Cli {
                command: Commands::Stages {
                    run: "redis_a".to_string(),
                    trial: None,
                    json,
                    runs_dir: runs_s.clone(),
                },
            };
            assert!(run_cli(cli).await.is_ok());
        }

        // --trial appends _trialN when the id has no trial suffix; that dir
        // does not exist, so this is a clean error.
        let cli = Cli {
            command: Commands::Stages {
                run: "redis_a_20261001".to_string(),
                trial: Some(2),
                json: false,
                runs_dir: runs_s.clone(),
            },
        };
        assert!(run_cli(cli).await.is_err());

        // Unknown ids fail during resolution (before verdict's exit/format).
        let cli = Cli {
            command: Commands::Verdict {
                run: "missing".to_string(),
                json: false,
                runs_dir: runs_s.clone(),
            },
        };
        assert!(run_cli(cli).await.is_err());
        let cli = Cli {
            command: Commands::Diff {
                a: "redis_a".to_string(),
                b: "missing".to_string(),
                runs_dir: runs_s.clone(),
            },
        };
        assert!(run_cli(cli).await.is_err());
        let cli = Cli {
            command: Commands::Stages {
                run: "redis_a".to_string(),
                trial: None,
                json: false,
                runs_dir: runs.join("missing").to_string_lossy().to_string(),
            },
        };
        assert!(run_cli(cli).await.is_err());
        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_stability_and_extend_error_paths() {
        let results = test_tmp_dir("stability_extend");
        for parent in [
            parent_result("stable_parent", &[100.0, 100.0]),
            parent_result("single_trial_parent", &[100.0]),
        ] {
            fs::write(
                results.join(format!("{}.json", parent.id)),
                serde_json::to_string(&parent).unwrap(),
            )
            .unwrap();
        }
        let dir = results.to_string_lossy().to_string();

        // Exact id and prefix both stay on the stable (never-transient)
        // branch of assess_stability.
        for prefix in ["stable_parent", "stable_par"] {
            let cli = Cli {
                command: Commands::Stability {
                    parent: prefix.to_string(),
                    results_dir: dir.clone(),
                },
            };
            assert!(run_cli(cli).await.is_ok());
        }
        let cli = Cli {
            command: Commands::Stability {
                parent: "nope".to_string(),
                results_dir: dir.clone(),
            },
        };
        assert!(run_cli(cli).await.is_err());

        let extend_cli = |parent: &str| Cli {
            command: Commands::Extend {
                parent: parent.to_string(),
                trials: 3,
                model: None,
                task: None,
                effort: None,
                max_turns: 100,
                budget_usd: 1.0,
                timeout_min: 120,
                api_key: None,
                workdir: "./workspace".to_string(),
                eval_only: false,
                review_model: None,
                results_dir: dir.clone(),
            },
        };

        // Stable parent: extend is a no-op success (no pipeline, no docker).
        assert!(run_cli(extend_cli("stable_par")).await.is_ok());
        // Single-trial parent: refused before any batch is attempted.
        let err = run_cli(extend_cli("single_trial_parent"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a multi-trial parent"), "{err}");
        // Unknown parent fails during resolution.
        assert!(run_cli(extend_cli("missing")).await.is_err());
        let _ = fs::remove_dir_all(&results);
    }

    #[tokio::test]
    async fn test_main_cli_matrix_plan_and_offline_enqueue() {
        let matrix_cli = |efforts: &str, tasks: &str, enqueue: bool, server: Option<String>| Cli {
            command: Commands::Matrix {
                models: "openrouter/meta/muse-spark".to_string(),
                efforts: efforts.to_string(),
                tasks: tasks.to_string(),
                trials: 2,
                extra: "--no-save".to_string(),
                per_effort_columns: false,
                enqueue,
                extend_if_unstable: false,
                harness: Harness::Omp,
                max_trials: 9,
                server,
                stop_on_pass: false,
                feedback: false,
                feedback_rounds: 0,
            },
        };

        // Print mode is pure formatting: no network, no enqueue.
        assert!(run_cli(matrix_cli("low", "redis", false, None))
            .await
            .is_ok());
        // Mixed efforts without --per-effort-columns are refused up front.
        assert!(run_cli(matrix_cli("low,high", "redis", false, None))
            .await
            .is_err());
        // Unknown task names fail before any dispatch.
        assert!(run_cli(matrix_cli("low", "dns", false, None))
            .await
            .is_err());
        // Enqueue mode against a closed port: status/balance probes degrade
        // to None, every cell POST fails, and the loop still exits Ok having
        // queued nothing.
        assert!(run_cli(matrix_cli(
            "low",
            "redis",
            true,
            Some("http://127.0.0.1:9".to_string())
        ))
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn test_main_cli_campaign_list_and_dry_run() {
        // Serialize with the SDB_HOME fixture tests: this resolves the repo
        // campaigns dir and must not observe a temp campaigns tree.
        let _env_lock = SDB_ENV_LOCK.lock().await;
        // Bundled campaigns are listed read-only.
        assert!(run_cli(Cli {
            command: Commands::Campaign {
                action: subdollarbench::config::CampaignAction::List,
            },
        })
        .await
        .is_ok());

        // Missing campaign fails to load.
        let missing = format!("no-such-campaign-{}", std::process::id());
        assert!(run_cli(Cli {
            command: Commands::Campaign {
                action: subdollarbench::config::CampaignAction::DryRun {
                    name: missing,
                    model: "test-model".to_string(),
                },
            },
        })
        .await
        .is_err());

        // Existing campaign: dry-run only reads campaign.json + prints the
        // stage plan (no spend, no engine, no workspace writes).
        if let Some(name) = subdollarbench::campaign::list_campaigns()
            .into_iter()
            .next()
        {
            assert!(run_cli(Cli {
                command: Commands::Campaign {
                    action: subdollarbench::config::CampaignAction::DryRun {
                        name,
                        model: "test-model".to_string(),
                    },
                },
            })
            .await
            .is_ok());
        }
    }

    #[tokio::test]
    async fn test_main_cli_maintenance_dry_runs_on_temp_dirs() {
        let results = test_tmp_dir("maint_results");
        let runs = test_tmp_dir("maint_runs");
        // One single-trial row and one two-trial parent cover both backfill
        // arms without --yes.
        let single = test_result_with_pass_rate(100.0);
        fs::write(
            results.join("single_trial.json"),
            serde_json::to_string_pretty(&single).unwrap(),
        )
        .unwrap();
        let parent = parent_result("multi_trial", &[100.0, 0.0]);
        fs::write(
            results.join("multi_trial.json"),
            serde_json::to_string_pretty(&parent).unwrap(),
        )
        .unwrap();
        let before = fs::read_to_string(results.join("multi_trial.json")).unwrap();
        let results_s = results.to_string_lossy().to_string();
        let runs_s = runs.to_string_lossy().to_string();

        assert!(run_cli(Cli {
            command: Commands::Backfill {
                results_dir: results_s.clone(),
                yes: false,
            },
        })
        .await
        .is_ok());
        assert!(run_cli(Cli {
            command: Commands::Migrate {
                results_dir: results_s.clone(),
                yes: false,
            },
        })
        .await
        .is_ok());
        // Dry runs must not rewrite the rows they describe.
        assert_eq!(
            fs::read_to_string(results.join("multi_trial.json")).unwrap(),
            before
        );

        // Canonical-dir guards refuse foreign dirs before touching live data.
        let err = run_cli(Cli {
            command: Commands::CostAnnotate {
                results_dir: results_s.clone(),
                runs_dir: runs_s.clone(),
                yes: false,
            },
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("canonical"), "{err}");
        let err = run_cli(Cli {
            command: Commands::Repair {
                results_dir: results_s.clone(),
                runs_dir: runs_s.clone(),
                yes: false,
            },
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("canonical"), "{err}");
        let err = run_cli(Cli {
            command: Commands::AnnotateCutoff {
                runs_dir: runs_s,
                yes: false,
            },
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("canonical"), "{err}");

        let _ = fs::remove_dir_all(&results);
        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_migrate_check_audit() {
        // Serialize with the SDB_HOME/SDB_BENCH_DB tests: this one opens the
        // resolved bench.db, and an env test's temp store may be mid-write.
        let _env_lock = SDB_ENV_LOCK.lock().await;
        // Read-only integrity audit against the canonical bench.db. The repo
        // may legitimately have findings, in which case the handler must fail
        // with its own audit error rather than something unexpected.
        match run_cli(Cli {
            command: Commands::MigrateCheck,
        })
        .await
        {
            Ok(()) => {}
            Err(e) => assert!(
                e.to_string().starts_with("integrity check found"),
                "migrate-check failed for a non-audit reason: {e:#}"
            ),
        }
    }

    #[tokio::test]
    async fn test_main_cli_regen_reviews_offline() {
        let root = test_tmp_dir("regen_reviews");
        let runs = root.join("runs");
        let results = root.join("results");
        let out = root.join("portal");
        let out_rollup = root.join("portal-rollup");
        fs::create_dir_all(&results).unwrap();
        let id = format!("redis_regen_{}", std::process::id());
        let rdir = runs.join(&id);
        fs::create_dir_all(&rdir).unwrap();
        // A cached review.json + force=false means the judge is never called.
        fs::write(
            rdir.join("review.json"),
            r#"{"good":[],"bad":[],"conclusion":"cached"}"#,
        )
        .unwrap();

        let regen_cli = |out: &Path, rollup_only: bool| Cli {
            command: Commands::RegenReviews {
                runs_dir: runs.to_string_lossy().to_string(),
                run_id: Some(id.clone()),
                model: "openrouter/meta/muse-spark".to_string(),
                api_key: None,
                force: false,
                results_dir: results.to_string_lossy().to_string(),
                out: out.to_string_lossy().to_string(),
                base_url: String::new(),
                keep: 10,
                rollup_only,
            },
        };

        // Full portal export path.
        assert!(run_cli(regen_cli(&out, false)).await.is_ok());
        assert!(out.join("index.html").exists());
        // Rollup-only export path.
        assert!(run_cli(regen_cli(&out_rollup, true)).await.is_ok());
        assert!(out_rollup.join("index.html").exists());
        // The cached review file was not rewritten.
        assert!(rdir.join("review.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_main_cli_review_run_error_and_early_paths() {
        let runs = test_tmp_dir("review_run");

        // Unknown id fails during resolution.
        assert!(run_cli(Cli {
            command: Commands::ReviewRun {
                run_id: "missing".to_string(),
                runs_dir: runs.to_string_lossy().to_string(),
                model: "openrouter/meta/muse-spark".to_string(),
                api_key: None,
                force: false,
            },
        })
        .await
        .is_err());

        // Existing run dir but no merged parent in the canonical results
        // dir: fails before any judge call (the id is pid-unique).
        let id = format!("zzz_offline_review_fixture_{}", std::process::id());
        let rdir = runs.join(&id);
        fs::create_dir_all(&rdir).unwrap();
        let err = run_cli(Cli {
            command: Commands::ReviewRun {
                run_id: id.clone(),
                runs_dir: runs.to_string_lossy().to_string(),
                model: "openrouter/meta/muse-spark".to_string(),
                api_key: None,
                force: false,
            },
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("no results/"), "{err}");

        // An existing run-review.md short-circuits without --force.
        fs::write(rdir.join("run-review.md"), "already reviewed\n").unwrap();
        assert!(run_cli(Cli {
            command: Commands::ReviewRun {
                run_id: id,
                runs_dir: runs.to_string_lossy().to_string(),
                model: "openrouter/meta/muse-spark".to_string(),
                api_key: None,
                force: false,
            },
        })
        .await
        .is_ok());
        assert_eq!(
            fs::read_to_string(rdir.join("run-review.md")).unwrap(),
            "already reviewed\n"
        );
        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_engine_commands_closed_port() {
        // Engine-backed variants are exercised only against a closed port:
        // each one must observe a connection failure, queue/stop nothing,
        // and leave local fixtures untouched.
        let dead = || Some("http://127.0.0.1:9".to_string());

        assert!(run_cli(Cli {
            command: Commands::Status {
                server: dead(),
                json: false,
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Status {
                server: dead(),
                json: true,
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Queue {
                server: dead(),
                json: false,
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Ps {
                server: dead(),
                model: None,
                task: None,
                effort: None,
                status: None,
                limit: 20,
                runs_dir: "./runs".to_string(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Enqueue {
                model: "openrouter/meta/muse-spark".to_string(),
                task: TaskType::Redis,
                effort: "low".to_string(),
                harness: Harness::Omp,
                max_turns: 1,
                budget_usd: 0.1,
                timeout_min: 1,
                api_key: None,
                eval_only: true,
                trials: 1,
                extend_if_unstable: false,
                max_trials: 9,
                no_save: true,
                review_model: None,
                server: dead(),
                stop_on_pass: false,
                feedback: false,
                feedback_rounds: 0,
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Dequeue {
                id: "q1".to_string(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::QueueClear { server: dead() },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Stop {
                run: String::new(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Log {
                run: String::new(),
                tail: 10,
                stages_only: false,
                follow: false,
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Prompt {
                task: "redis".to_string(),
                save: false,
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Env { server: dead() },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Models {
                family: Some("glm".to_string()),
                search: None,
                sort: "cheapest".to_string(),
                limit: 5,
                json: false,
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Omp { server: dead() },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::OmpUpdate {
                version: None,
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::PortalRebuild { server: dead() },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Lboard {
                task: "redis".to_string(),
                json: true,
                server: dead(),
            },
        })
        .await
        .is_err());

        // QueueExtend resolves the parent locally, then fails on POST.
        let results = test_tmp_dir("engine_queue_extend");
        let parent = parent_result("queue_extend_parent", &[100.0]);
        fs::write(
            results.join("queue_extend_parent.json"),
            serde_json::to_string(&parent).unwrap(),
        )
        .unwrap();
        assert!(run_cli(Cli {
            command: Commands::QueueExtend {
                parent: "queue_extend".to_string(),
                trials: 3,
                max_turns: None,
                budget_usd: None,
                timeout_min: None,
                eval_only: false,
                results_dir: results.to_string_lossy().to_string(),
                server: dead(),
            },
        })
        .await
        .is_err());

        // Run-inspection variants resolve the id against a temp runs dir,
        // then fail on the closed-port request.
        let runs = test_tmp_dir("engine_runs");
        let m = manifest_fixture("redis_engine_offline", 100.0);
        let rdir = runs.join(&m.run_id);
        fs::create_dir_all(&rdir).unwrap();
        fs::write(
            rdir.join("manifest.json"),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();
        let runs_s = runs.to_string_lossy().to_string();
        let lookup = || "redis_engine".to_string();

        assert!(run_cli(Cli {
            command: Commands::Detail {
                run: lookup(),
                json: false,
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Ls {
                run: lookup(),
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Cat {
                run: lookup(),
                file: "main.py".to_string(),
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Review {
                run: lookup(),
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Sdiff {
                a: lookup(),
                b: lookup(),
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Poison {
                run: lookup(),
                cure: false,
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Hide {
                run: lookup(),
                show: false,
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        // Rm refuses without --yes before any network call; with --yes the
        // DELETE hits the closed port, so the archived dir survives.
        assert!(run_cli(Cli {
            command: Commands::Rm {
                run: lookup(),
                yes: false,
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(run_cli(Cli {
            command: Commands::Rm {
                run: lookup(),
                yes: true,
                runs_dir: runs_s.clone(),
                server: dead(),
            },
        })
        .await
        .is_err());
        assert!(rdir.join("manifest.json").exists());

        let _ = fs::remove_dir_all(&runs);
        let _ = fs::remove_dir_all(&results);
    }

    #[tokio::test]
    async fn test_main_cli_matrix_report_temp_dirs() {
        let results = test_tmp_dir("matrix_report");
        // Empty dir: the "no results" arm.
        assert!(run_cli(Cli {
            command: Commands::MatrixReport {
                results_dir: results.to_string_lossy().to_string(),
            },
        })
        .await
        .is_ok());

        // One well-formed row: validates the wave, then prints the matrix.
        let mut row = test_result_with_pass_rate(100.0);
        row.id = "matrix_fixture_1".to_string();
        row.model = "openrouter/meta/muse-spark".to_string();
        row.task = "redis".to_string();
        row.effort = Some("low".to_string());
        fs::write(
            results.join("matrix_fixture_1.json"),
            serde_json::to_string(&row).unwrap(),
        )
        .unwrap();
        assert!(run_cli(Cli {
            command: Commands::MatrixReport {
                results_dir: results.to_string_lossy().to_string(),
            },
        })
        .await
        .is_ok());
        let _ = fs::remove_dir_all(&results);
    }

    #[tokio::test]
    async fn test_main_cli_doctor_readonly() {
        // Read-only pre-flight (docker info/images, env, TCP bind probes).
        // Either everything is green or the handler's own hard-check gate
        // fires; any other error means the arm fell over unexpectedly.
        match run_cli(Cli {
            command: Commands::Doctor,
        })
        .await
        {
            Ok(()) => {}
            Err(e) => assert!(
                e.to_string().starts_with("doctor:"),
                "unexpected doctor error: {e:#}"
            ),
        }
    }

    #[test]
    fn test_cli_parse_subcommand_paths() {
        use clap::Parser;

        let cli = Cli::try_parse_from(["subdollarbench", "runs", "--limit", "5", "--all"]).unwrap();
        match cli.command {
            Commands::Runs { limit, all, .. } => {
                assert_eq!(limit, 5);
                assert!(all);
            }
            other => panic!("expected Runs, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["subdollarbench", "campaign", "dry-run", "--name", "demo"])
            .unwrap();
        match cli.command {
            Commands::Campaign {
                action: subdollarbench::config::CampaignAction::DryRun { name, model },
            } => {
                assert_eq!(name, "demo");
                assert_eq!(model, "(unset)");
            }
            other => panic!("expected Campaign::DryRun, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["subdollarbench", "matrix", "--tasks", "redis"]).unwrap();
        match cli.command {
            Commands::Matrix {
                tasks,
                efforts,
                enqueue,
                ..
            } => {
                assert_eq!(tasks, "redis");
                // Defaults documented on the clap args.
                assert_eq!(efforts, "minimal,max");
                assert!(!enqueue);
            }
            other => panic!("expected Matrix, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["subdollarbench", "sandbox", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Sandbox {
                command: subdollarbench::config::SandboxCommands::Check
            }
        ));

        let cli = Cli::try_parse_from(["subdollarbench", "verdict", "abc", "--json"]).unwrap();
        match cli.command {
            Commands::Verdict { run, json, .. } => {
                assert_eq!(run, "abc");
                assert!(json);
            }
            other => panic!("expected Verdict, got {other:?}"),
        }

        // Invalid ValueEnum values and unknown subcommands are clap errors.
        assert!(Cli::try_parse_from(["subdollarbench", "eval", "--task", "dns"]).is_err());
        assert!(Cli::try_parse_from(["subdollarbench", "no-such-command"]).is_err());
    }

    // ---- Synthetic-fixture coverage (offline) ---------------------------

    #[tokio::test]
    async fn test_main_cli_leaderboard_populated_and_matrix_protocols() {
        let results = test_tmp_dir("lb_matrix_pop");
        let row = |id: &str, model: &str, pass: f64| {
            let mut r = test_result_with_pass_rate(pass);
            r.id = id.to_string();
            r.model = model.to_string();
            r.task = "redis".to_string();
            r.effort = Some("low".to_string());
            r.timestamp = "2026-10-01T00:00:00Z".to_string();
            // Single-trial legs need an evidence count or the board filters
            // them as 0-trial junk.
            r.total_trials = Some(1);
            r
        };
        let a = row("redis_pop_a_20261001", "openrouter/meta/muse-spark", 100.0);
        let b = row("redis_pop_b_20261001", "openrouter/z-ai/glm", 50.0);
        write_result_row(&results, &a);
        write_result_row(&results, &b);
        let dir = results.to_string_lossy().to_string();

        // Non-empty boards: the task-filtered and consolidated views both print.
        for task in ["redis", "all"] {
            assert!(run_cli(Cli {
                command: Commands::Leaderboard {
                    results_dir: dir.clone(),
                    task: task.to_string(),
                },
            })
            .await
            .is_ok());
        }

        // One uniform-protocol wave: the matrix prints.
        assert!(run_cli(Cli {
            command: Commands::MatrixReport {
                results_dir: dir.clone(),
            },
        })
        .await
        .is_ok());

        // Mixed evaluation protocols must be refused, never pooled.
        let mut a2 = a.clone();
        a2.protocol = Some("fixed_n".to_string());
        let mut b2 = b.clone();
        b2.protocol = Some("sequential_cap".to_string());
        write_result_row(&results, &a2);
        write_result_row(&results, &b2);
        let err = run_cli(Cli {
            command: Commands::MatrixReport {
                results_dir: dir.clone(),
            },
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("protocol"), "{err}");

        // Same-stamped wave compares again (two rows, three cells incl. this one).
        b2.protocol = Some("fixed_n".to_string());
        write_result_row(&results, &b2);
        let mut c = row("redis_pop_c_20261001", "openrouter/meta/muse-spark", 0.0);
        c.protocol = Some("fixed_n".to_string());
        write_result_row(&results, &c);
        assert!(run_cli(Cli {
            command: Commands::MatrixReport {
                results_dir: dir.clone(),
            },
        })
        .await
        .is_ok());

        let _ = fs::remove_dir_all(&results);
    }

    #[tokio::test]
    async fn test_main_cli_stages_failed_stage_and_trial_suffix() {
        let runs = test_tmp_dir("stages_more");
        for m in [
            manifest_fixture("redis_fail_fixture", 50.0),
            manifest_fixture("redis_trial_target_trial2", 100.0),
        ] {
            let d = runs.join(&m.run_id);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("manifest.json"), serde_json::to_string(&m).unwrap()).unwrap();
        }
        let runs_s = runs.to_string_lossy().to_string();

        // Failed stage with a verifier error message.
        assert!(run_cli(Cli {
            command: Commands::Stages {
                run: "redis_fail".to_string(),
                trial: None,
                json: false,
                runs_dir: runs_s.clone(),
            },
        })
        .await
        .is_ok());

        // --trial appends _trialN only when the resolved id has no suffix.
        assert!(run_cli(Cli {
            command: Commands::Stages {
                run: "redis_trial_target".to_string(),
                trial: Some(2),
                json: false,
                runs_dir: runs_s.clone(),
            },
        })
        .await
        .is_ok());

        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_stability_throughput_subruns() {
        let results = test_tmp_dir("stability_tp");
        let mut parent = parent_result("stability_tp_parent", &[100.0, 100.0]);
        for sub in &mut parent.subruns {
            sub.throughput_req_sec = Some(1234.0);
            sub.reference_throughput_req_sec = Some(71000.0);
        }
        write_result_row(&results, &parent);
        assert!(run_cli(Cli {
            command: Commands::Stability {
                parent: "stability_tp".to_string(),
                results_dir: results.to_string_lossy().to_string(),
            },
        })
        .await
        .is_ok());
        let _ = fs::remove_dir_all(&results);
    }

    #[tokio::test]
    async fn test_main_cli_watch_follow_loop_without_manifest() {
        let runs = test_tmp_dir("watch_follow");
        let rdir = runs.join("redis_watch_live");
        fs::create_dir_all(&rdir).unwrap();
        fs::write(rdir.join("console.log"), "booting\n").unwrap();
        let cli = Cli {
            command: Commands::Watch {
                run: "redis_watch_live".to_string(),
                no_follow: false,
                stages_only: false,
                runs_dir: runs.to_string_lossy().to_string(),
            },
        };
        // No manifest: the loop tails, sleeps, and tails again; the timeout
        // proves it stayed in follow mode without waiting on a real run.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), run_cli(cli))
                .await
                .is_err()
        );
        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_eval_offline_verifier_arms() {
        // Closed ports only: every verifier reports 0% without touching a
        // live server (the engine's own task ports are never probed).
        for (task, port) in [
            (TaskType::Pdf, 59990),
            (TaskType::Wal, 59991),
            (TaskType::Pubsub, 59992),
            (TaskType::Binproto, 59993),
            (TaskType::Proxy, 59994),
        ] {
            assert!(run_cli(Cli {
                command: Commands::Eval {
                    task,
                    port: Some(port),
                },
            })
            .await
            .is_ok());
        }
    }

    #[tokio::test]
    async fn test_main_cli_replay_manifest_read_failure_offline() {
        // Exact one-dir resolution then a manifest read failure: both happen
        // before the engine status probe, so this stays fully offline even
        // though a live engine may hold 127.0.0.1:3000.
        let runs = test_tmp_dir("replay_bad_manifest");
        let rdir = runs.join("redis_replay_fixture_bad");
        fs::create_dir_all(&rdir).unwrap();
        fs::write(rdir.join("manifest.json"), "{not json").unwrap();
        let err = run_cli(Cli {
            command: Commands::Replay {
                run: "redis_replay_fixture".to_string(),
                port: Some(59980),
                runs_dir: runs.to_string_lossy().to_string(),
            },
        })
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("manifest"), "{msg}");

        // Valid manifest, unsupported task: parse_task refuses before the
        // engine probe and before any docker work.
        let mut m = test_manifest_with_status("completed");
        m.run_id = "redis_replay_badtask".to_string();
        m.task = "dns".to_string();
        let d2 = runs.join(&m.run_id);
        fs::create_dir_all(&d2).unwrap();
        fs::write(d2.join("manifest.json"), serde_json::to_string(&m).unwrap()).unwrap();
        let err = run_cli(Cli {
            command: Commands::Replay {
                run: "redis_replay_badtask".to_string(),
                port: None,
                runs_dir: runs.to_string_lossy().to_string(),
            },
        })
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("task"), "{msg}");
        let _ = fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_main_cli_queue_feedback_defaults_and_empty_extra() {
        let dead = || Some("http://127.0.0.1:9".to_string());

        // Run --queue --feedback: the default rounds (2) resolve before the
        // POST fails on the closed port; no pipeline, no docker.
        assert!(run_cli(Cli {
            command: Commands::Run {
                model: "openrouter/meta/muse-spark".to_string(),
                task: TaskType::Redis,
                effort: "low".to_string(),
                harness: Harness::Omp,
                max_turns: 1,
                budget_usd: 0.1,
                timeout_min: 1,
                api_key: None,
                workdir: "./workspace".to_string(),
                eval_only: true,
                trials: 1,
                no_save: true,
                queue: true,
                server: dead(),
                review_model: None,
                extend_if_unstable: false,
                max_trials: 9,
                stop_on_pass: false,
                feedback: true,
                feedback_rounds: 0,
            },
        })
        .await
        .is_err());

        // Enqueue --feedback: same default resolution.
        assert!(run_cli(Cli {
            command: Commands::Enqueue {
                model: "openrouter/meta/muse-spark".to_string(),
                task: TaskType::Redis,
                effort: "low".to_string(),
                harness: Harness::Omp,
                max_turns: 1,
                budget_usd: 0.1,
                timeout_min: 1,
                api_key: None,
                eval_only: true,
                trials: 1,
                extend_if_unstable: false,
                max_trials: 9,
                no_save: true,
                review_model: None,
                server: dead(),
                stop_on_pass: false,
                feedback: true,
                feedback_rounds: 0,
            },
        })
        .await
        .is_err());

        // Matrix print mode with no extra flags: command lines carry no
        // trailing suffix and nothing is dispatched.
        assert!(run_cli(Cli {
            command: Commands::Matrix {
                models: "openrouter/meta/muse-spark".to_string(),
                efforts: "low".to_string(),
                tasks: "redis".to_string(),
                trials: 1,
                extra: String::new(),
                per_effort_columns: true,
                enqueue: false,
                extend_if_unstable: false,
                harness: Harness::Omp,
                max_trials: 9,
                server: None,
                stop_on_pass: false,
                feedback: true,
                feedback_rounds: 0,
            },
        })
        .await
        .is_ok());
    }

    /// Minimal valid campaign spec; `mutate` tweaks it for validation cases.
    fn campaign_spec_json(
        name: &str,
        mutate: impl FnOnce(&mut serde_json::Value),
    ) -> serde_json::Value {
        let mut v = serde_json::json!({
            "name": name,
            "version": 1,
            "task": "redis",
            "title": "Fixture campaign",
            "description": "offline fixture",
            "stages": [{
                "id": 0,
                "type": "build",
                "title": "Stage zero",
                "prompt": "p0.md",
                "acceptance": {"redis_stages": [1, 2]},
                "budget_usd": 0.05,
                "max_turns": 3
            }]
        });
        mutate(&mut v);
        v
    }

    fn write_campaign_fixture(
        root: &Path,
        name: &str,
        spec: &serde_json::Value,
        with_prompt: bool,
    ) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("campaign.json"),
            serde_json::to_string(spec).unwrap(),
        )
        .unwrap();
        if with_prompt {
            fs::write(dir.join("p0.md"), "build something\n").unwrap();
        }
    }

    #[tokio::test]
    async fn test_main_cli_campaign_fixture_validation() {
        let env = SdbEnv::new("campaign_fixture").await;

        // No campaigns dir at all: the empty-list arm.
        assert!(run_cli(Cli {
            command: Commands::Campaign {
                action: subdollarbench::config::CampaignAction::List,
            },
        })
        .await
        .is_ok());

        let croot = env.root.join("campaigns");
        write_campaign_fixture(&croot, "good", &campaign_spec_json("good", |_| {}), true);
        // Invalid: acceptance id outside 1..=16.
        write_campaign_fixture(
            &croot,
            "bad_stage_id",
            &campaign_spec_json("bad_stage_id", |v| {
                v["stages"][0]["acceptance"]["redis_stages"] = serde_json::json!([99]);
            }),
            true,
        );
        // Invalid: empty stage title.
        write_campaign_fixture(
            &croot,
            "empty_title",
            &campaign_spec_json("empty_title", |v| {
                v["stages"][0]["title"] = serde_json::json!("  ");
            }),
            true,
        );
        // Invalid: prompt file missing on disk.
        write_campaign_fixture(
            &croot,
            "missing_prompt",
            &campaign_spec_json("missing_prompt", |_| {}),
            false,
        );

        // list() reports the good one and marks the broken ones INVALID.
        assert!(run_cli(Cli {
            command: Commands::Campaign {
                action: subdollarbench::config::CampaignAction::List,
            },
        })
        .await
        .is_ok());

        // dry-run of the valid campaign prints the full stage plan.
        assert!(run_cli(Cli {
            command: Commands::Campaign {
                action: subdollarbench::config::CampaignAction::DryRun {
                    name: "good".to_string(),
                    model: "test-model".to_string(),
                },
            },
        })
        .await
        .is_ok());

        // Each invalid variant fails to load before any engine call.
        for name in ["bad_stage_id", "empty_title", "missing_prompt"] {
            assert!(run_cli(Cli {
                command: Commands::Campaign {
                    action: subdollarbench::config::CampaignAction::DryRun {
                        name: name.to_string(),
                        model: "test-model".to_string(),
                    },
                },
            })
            .await
            .is_err());
        }

        // `campaign run` on a missing name fails in load_campaign, before the
        // engine status probe (no network, no runs/ writes).
        assert!(run_cli(Cli {
            command: Commands::Campaign {
                action: subdollarbench::config::CampaignAction::Run {
                    wall_min: None,
                    name: "no-such-campaign".to_string(),
                    model: "test-model".to_string(),
                    effort: "low".to_string(),
                    harness: Harness::Omp,
                    budget_usd: 1.0,
                    timeout_min: 5,
                    api_key: None,
                    eval_only: true,
                    workspace: None,
                    phase: None,
                    chain: None,
                    refork: None,
                },
            },
        })
        .await
        .is_err());

        drop(env);
    }

    #[tokio::test]
    async fn test_main_cli_backfill_yes_rewrites_temp_tree() {
        let env = SdbEnv::new("backfill_yes_apply").await;
        let results = env.root.join("results");
        let runs = env.root.join("runs");
        fs::create_dir_all(&results).unwrap();
        fs::create_dir_all(&runs).unwrap();
        // Pre-create the store so the DB mirror branches execute.
        let _store = subdollarbench::report::Store::open(&env.root.join("bench.db")).unwrap();
        let fresh_stamp = subdollarbench::report::rules::fingerprint();

        // 1. Single-trial row, unstamped: rewrite + upsert.
        let mut old_single = test_result_with_pass_rate(100.0);
        old_single.id = "sbf_single_old".to_string();
        write_result_row(&results, &old_single);
        // 2. Single-trial row already on the current fingerprint: skip.
        let mut fresh_single = test_result_with_pass_rate(100.0);
        fresh_single.id = "sbf_single_fresh".to_string();
        fresh_single.rules_version = Some(fresh_stamp.clone());
        write_result_row(&results, &fresh_single);
        // 3. Unparseable row: skipped.
        fs::write(results.join("sbf_bad.json"), "{not json").unwrap();
        // 4. Unreadable entry (dangling symlink): skipped.
        #[cfg(unix)]
        std::os::unix::fs::symlink("nowhere.json", results.join("sbf_broken.json")).unwrap();
        // 5. Multi-trial with current numbers but no stamp: re-stamp only.
        let mut current = parent_result("sbf_multi_current", &[100.0, 0.0]);
        current.subruns = current
            .subruns
            .iter()
            .map(|s| {
                let mut sub = s.clone();
                let eff = sub.effective_cost_usd.unwrap_or(sub.total_cost_usd);
                sub.efficiency_score =
                    subdollarbench::cost::calculator::ModelPricing::calculate_efficiency_score(
                        sub.pass_rate,
                        eff,
                    );
                sub.rules_version = None;
                sub
            })
            .collect();
        let mut merged = BenchmarkRunResult::merge_subruns(
            "sbf_multi_current",
            current.subruns.clone(),
            None,
            None,
        )
        .unwrap();
        merged.rules_version = None;
        write_result_row(&results, &merged);
        write_manifest(&runs, "sbf_multi_current", |m| {
            m.status = "completed".to_string();
        });
        // 6. Same shape, already stamped fresh: skip.
        let mut merged_fresh = merged.clone();
        merged_fresh.rules_version = Some(fresh_stamp.clone());
        for s in &mut merged_fresh.subruns {
            s.rules_version = Some(fresh_stamp.clone());
        }
        write_result_row(&results, &merged_fresh);
        // 7. Multi-trial whose stored numbers are stale: printed diff +
        //    rewrite + manifest mirror (status becomes failed_tests at 50%).
        let mut stale = parent_result("sbf_multi_stale", &[100.0, 0.0]);
        stale.throughput_score = Some(55.5);
        for sub in &mut stale.subruns {
            sub.throughput_req_sec = Some(50_000.0);
            sub.reference_throughput_req_sec = Some(71_000.0);
        }
        write_result_row(&results, &stale);
        write_manifest(&runs, "sbf_multi_stale", |m| {
            m.status = "completed".to_string();
            m.pass_rate = 100.0;
        });
        // 8. Multi-trial, all passing, with a stored throughput score: the
        //    printed diff shows both throughput columns and the manifest
        //    mirror restores status completed.
        let mut full = parent_result("sbf_multi_full", &[100.0, 100.0]);
        full.throughput_score = Some(12.5);
        for sub in &mut full.subruns {
            sub.throughput_req_sec = Some(50_000.0);
            sub.reference_throughput_req_sec = Some(71_000.0);
        }
        write_result_row(&results, &full);
        write_manifest(&runs, "sbf_multi_full", |m| {
            m.status = "failed_tests".to_string();
            m.pass_rate = 0.0;
        });

        let results_s = results.to_string_lossy().to_string();
        assert!(run_cli(Cli {
            command: Commands::Backfill {
                results_dir: results_s.clone(),
                yes: true,
            },
        })
        .await
        .is_ok());

        // The rewrites landed and carry the current fingerprint.
        let rewritten = fs::read_to_string(results.join("sbf_single_old.json")).unwrap();
        assert!(rewritten.contains(&fresh_stamp));
        let stale_row = fs::read_to_string(results.join("sbf_multi_stale.json")).unwrap();
        assert!(stale_row.contains(&fresh_stamp));
        // Manifest mirror: 100% stored / 50% merged -> failed_tests.
        let mval: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(runs.join("sbf_multi_stale/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(mval["status"], "failed_tests");
        // All-passing row: mirror restores completed.
        let fval: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(runs.join("sbf_multi_full/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(fval["status"], "completed");
        // Unchanged rows were not rewritten (dry-run assertion equivalent).
        assert_eq!(
            fs::read_to_string(results.join("sbf_single_fresh.json")).unwrap(),
            serde_json::to_string_pretty(&fresh_single).unwrap()
        );

        // Strip the stamps from the merged stale row: identical numbers with
        // a changed label take the re-stamp-only branch (not a rescore).
        let mut stripped: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(results.join("sbf_multi_stale.json")).unwrap(),
        )
        .unwrap();
        stripped["rules_version"] = serde_json::Value::Null;
        for sub in stripped["subruns"].as_array_mut().unwrap() {
            sub["rules_version"] = serde_json::Value::Null;
        }
        fs::write(
            results.join("sbf_multi_stale.json"),
            serde_json::to_string_pretty(&stripped).unwrap(),
        )
        .unwrap();
        assert!(run_cli(Cli {
            command: Commands::Backfill {
                results_dir: results_s.clone(),
                yes: true,
            },
        })
        .await
        .is_ok());
        let restamped = fs::read_to_string(results.join("sbf_multi_stale.json")).unwrap();
        assert!(restamped.contains(&fresh_stamp));
        let restamped_val: serde_json::Value = serde_json::from_str(&restamped).unwrap();
        assert_eq!(
            restamped_val["rules_version"].as_str(),
            Some(fresh_stamp.as_str())
        );
        // The stamp-only path mirrors the rule label into the manifest too.
        let mval: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(runs.join("sbf_multi_stale/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(mval["rules_version"].as_str(), Some(fresh_stamp.as_str()));

        // Second pass: everything is current now, every row skips cleanly.
        assert!(run_cli(Cli {
            command: Commands::Backfill {
                results_dir: results_s.clone(),
                yes: true,
            },
        })
        .await
        .is_ok());

        drop(env);
    }

    #[tokio::test]
    async fn test_main_cli_migrate_yes_imports_temp_db() {
        let env = SdbEnv::new("migrate_yes_apply").await;
        let results = env.root.join("results");
        let runs = env.root.join("runs");
        fs::create_dir_all(&results).unwrap();
        fs::create_dir_all(&runs).unwrap();
        let _store = subdollarbench::report::Store::open(&env.root.join("bench.db")).unwrap();

        let mut row = test_result_with_pass_rate(100.0);
        row.id = "smg_result_row".to_string();
        write_result_row(&results, &row);
        fs::write(results.join("smg_bad.json"), "{not json").unwrap();
        // Unreadable entry: the read-failure skip arm.
        #[cfg(unix)]
        std::os::unix::fs::symlink("nowhere.json", results.join("smg_broken.json")).unwrap();

        write_manifest(&runs, "smg_run_ok", |_| {});
        fs::create_dir_all(runs.join("smg_run_no_manifest")).unwrap();
        fs::create_dir_all(runs.join("smg_staging.tmp")).unwrap();
        let bad_dir = runs.join("smg_run_bad_manifest");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("manifest.json"), "{not json").unwrap();

        let cli = || Cli {
            command: Commands::Migrate {
                results_dir: results.to_string_lossy().to_string(),
                yes: true,
            },
        };
        assert!(run_cli(cli()).await.is_ok());
        // Second run exercises the already-present parent arm and is idempotent.
        assert!(run_cli(cli()).await.is_ok());
        // The staging .tmp dir is skipped, not imported.
        let store = subdollarbench::report::Store::open(&env.root.join("bench.db")).unwrap();
        assert_eq!(store.parent_count(), 1);
        assert_eq!(store.run_manifest_count(), 1);

        drop(env);
    }

    #[tokio::test]
    async fn test_main_cli_migrate_check_findings_and_clean() {
        let env = SdbEnv::new("migrate_check_fixture").await;
        let results = env.root.join("results");
        fs::create_dir_all(&results).unwrap();
        let db = env.root.join("bench.db");

        // A junk parent (test_ prefix, zero trials) and an orphan trial row.
        let store = subdollarbench::report::Store::open(&db).unwrap();
        let mut junk = test_result_with_pass_rate(100.0);
        junk.id = "test_junk_probe".to_string();
        store.upsert_parent(&junk).unwrap();
        drop(store);
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO trials (id, parent_id, trial_no, pass_rate, status, cost_usd, duration_s, turns, doc)
                 VALUES ('orphan_trial1', 'ghost_parent', 1, 0.0, 'Failed', 0.0, NULL, NULL, '{}')",
                [],
            )
            .unwrap();
        }
        let err = run_cli(Cli {
            command: Commands::MigrateCheck,
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("integrity check found"), "{err}");

        // Clearing the debris leaves the same store clean.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("DELETE FROM trials", []).unwrap();
            conn.execute("DELETE FROM parents", []).unwrap();
        }
        assert!(run_cli(Cli {
            command: Commands::MigrateCheck,
        })
        .await
        .is_ok());

        drop(env);
    }

    #[tokio::test]
    async fn test_main_cli_repair_and_cutoff_dry_run_canonical() {
        let env = SdbEnv::new("repair_cutoff_dry").await;
        let results = env.root.join("results");
        let runs = env.root.join("runs");
        fs::create_dir_all(&results).unwrap();
        fs::create_dir_all(&runs).unwrap();

        // Repair cures a false cheat DQ (empty workspace) and a 100% agent_kill.
        for (id, status) in [
            ("redis_dq_fixture_20261001_w0", "disqualified_cheat"),
            ("redis_killed_fixture_20261001_w0", "agent_killed"),
        ] {
            let d = write_manifest(&runs, id, |m| {
                m.status = status.to_string();
                m.pass_rate = 100.0;
            });
            fs::create_dir_all(d.join("workspace")).unwrap();
        }

        // Cutoff fixture: opencode trial that thought but never acted, plus a
        // billing-shaped near-miss that must be excluded.
        let cutoff_dir = runs.join("redis_cutoff_fixture_20261008_000000_trial1");
        fs::create_dir_all(cutoff_dir.join("workspace")).unwrap();
        fs::write(
            cutoff_dir.join("manifest.json"),
            serde_json::json!({
                "run_id": "redis_cutoff_fixture_20261008_000000_trial1",
                "model": "openrouter/meta/muse-spark",
                "task": "redis",
                "harness": "opencode",
                "status": "failed_tests",
                "pass_rate": 0.0,
                "cutoff_turn1": null
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            cutoff_dir.join("console.log"),
            "[12:00:01] Turn 1 started\n[12:00:02] Finished!\n",
        )
        .unwrap();
        let billing_dir = runs.join("redis_billing_fixture_20261008_000000_trial1");
        fs::create_dir_all(billing_dir.join("workspace")).unwrap();
        fs::write(
            billing_dir.join("manifest.json"),
            serde_json::json!({
                "run_id": "redis_billing_fixture_20261008_000000_trial1",
                "model": "openrouter/meta/muse-spark",
                "task": "redis",
                "harness": "opencode",
                "status": "failed_tests",
                "pass_rate": 0.0,
                "cutoff_turn1": null
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            billing_dir.join("console.log"),
            "[12:00:01] Turn 1 started\nHTTP/1.1 402 Payment Required\n",
        )
        .unwrap();

        let results_s = results.to_string_lossy().to_string();
        let runs_s = runs.to_string_lossy().to_string();

        assert!(run_cli(Cli {
            command: Commands::Repair {
                results_dir: results_s.clone(),
                runs_dir: runs_s.clone(),
                yes: false,
            },
        })
        .await
        .is_ok());

        assert!(run_cli(Cli {
            command: Commands::AnnotateCutoff {
                runs_dir: runs_s.clone(),
                yes: false,
            },
        })
        .await
        .is_ok());

        // Dry runs left every fixture byte-identical.
        let untouched = fs::read_to_string(cutoff_dir.join("manifest.json")).unwrap();
        assert!(untouched.contains("\"cutoff_turn1\":null"), "{untouched}");

        drop(env);
    }

    #[tokio::test]
    async fn test_main_cli_cost_annotate_empty_canonical() {
        let env = SdbEnv::new("cost_annotate_empty").await;
        let results = env.root.join("results");
        let runs = env.root.join("runs");
        fs::create_dir_all(&results).unwrap();
        fs::create_dir_all(&runs).unwrap();
        // No manifests and no result rows means no models to price, so the
        // command stays fully offline while printing the zero report.
        assert!(run_cli(Cli {
            command: Commands::CostAnnotate {
                results_dir: results.to_string_lossy().to_string(),
                runs_dir: runs.to_string_lossy().to_string(),
                yes: false,
            },
        })
        .await
        .is_ok());
        drop(env);
    }

    #[test]
    fn test_cli_parse_remaining_subcommand_flags() {
        use clap::Parser;

        // --server can steer engine commands at a closed port; parse each
        // shape so the clap definitions are locked down.
        let cli = Cli::try_parse_from([
            "subdollarbench",
            "run",
            "--model",
            "m",
            "--queue",
            "--server",
            "http://127.0.0.1:9",
            "--stop-on-pass",
            "--feedback",
        ])
        .unwrap();
        match cli.command {
            Commands::Run {
                queue,
                stop_on_pass,
                feedback,
                server,
                harness,
                trials,
                ..
            } => {
                assert!(queue);
                assert!(stop_on_pass);
                assert!(feedback);
                assert_eq!(server.as_deref(), Some("http://127.0.0.1:9"));
                assert_eq!(harness, Harness::Omp);
                assert_eq!(trials, 9);
            }
            other => panic!("expected Run, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "subdollarbench",
            "enqueue",
            "--model",
            "m",
            "--harness",
            "opencode",
            "--feedback-rounds",
            "3",
            "--max-trials",
            "12",
        ])
        .unwrap();
        match cli.command {
            Commands::Enqueue {
                harness,
                feedback_rounds,
                max_trials,
                trials,
                ..
            } => {
                assert_eq!(harness, Harness::Opencode);
                assert_eq!(feedback_rounds, 3);
                assert_eq!(max_trials, 12);
                assert_eq!(trials, 9);
            }
            other => panic!("expected Enqueue, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "subdollarbench",
            "campaign",
            "run",
            "--name",
            "demo",
            "--model",
            "m",
            "--harness",
            "opencode",
            "-b",
            "2.5",
            "--eval-only",
            "--workspace",
            "/tmp/ws",
        ])
        .unwrap();
        match cli.command {
            Commands::Campaign {
                action:
                    subdollarbench::config::CampaignAction::Run {
                        name,
                        harness,
                        budget_usd,
                        eval_only,
                        workspace,
                        timeout_min,
                        ..
                    },
            } => {
                assert_eq!(name, "demo");
                assert_eq!(harness, Harness::Opencode);
                assert_eq!(budget_usd, 2.5);
                assert!(eval_only);
                assert_eq!(workspace.as_deref(), Some("/tmp/ws"));
                assert_eq!(timeout_min, 60);
            }
            other => panic!("expected Campaign::Run, got {other:?}"),
        }

        for args in [
            vec!["subdollarbench", "extend", "parent", "--trials", "3"],
            vec!["subdollarbench", "backfill", "--yes"],
            vec!["subdollarbench", "repair", "--results-dir", "r", "--yes"],
            vec![
                "subdollarbench",
                "cost-annotate",
                "--results-dir",
                "r",
                "--runs-dir",
                "x",
            ],
            vec![
                "subdollarbench",
                "annotate-cutoff",
                "--runs-dir",
                "x",
                "--yes",
            ],
            vec!["subdollarbench", "migrate", "--results-dir", "r"],
            vec!["subdollarbench", "migrate-check"],
            vec!["subdollarbench", "replay", "rid", "--port", "12345"],
            vec!["subdollarbench", "ps", "--model", "glm", "--limit", "3"],
            vec!["subdollarbench", "queue-extend", "parent", "--trials", "2"],
            vec!["subdollarbench", "dequeue", "q3"],
            vec!["subdollarbench", "queue-clear"],
            vec!["subdollarbench", "stop"],
            vec!["subdollarbench", "log", "--follow", "--tail", "5"],
            vec!["subdollarbench", "detail", "rid", "--json"],
            vec!["subdollarbench", "sdiff", "a", "b"],
            vec!["subdollarbench", "ls", "rid"],
            vec!["subdollarbench", "cat", "rid", "main.py"],
            vec!["subdollarbench", "review", "rid"],
            vec!["subdollarbench", "lboard", "--json", "--task", "redis"],
            vec!["subdollarbench", "prompt", "redis", "--save"],
            vec!["subdollarbench", "env"],
            vec![
                "subdollarbench",
                "models",
                "--sort",
                "newest",
                "--limit",
                "4",
            ],
            vec!["subdollarbench", "omp"],
            vec!["subdollarbench", "omp-update", "-v", "9.9.9"],
            vec!["subdollarbench", "portal-rebuild"],
            vec!["subdollarbench", "poison", "rid", "--cure"],
            vec!["subdollarbench", "hide", "rid", "--show"],
            vec!["subdollarbench", "rm", "rid", "--yes"],
            vec![
                "subdollarbench",
                "matrix",
                "--per-effort-columns",
                "--enqueue",
                "--stop-on-pass",
            ],
            vec!["subdollarbench", "sandbox", "update", "--version", "1.2.3"],
            vec!["subdollarbench", "ui", "--engine-url", "http://127.0.0.1:9"],
            vec!["subdollarbench", "portal", "--keep", "7"],
            vec!["subdollarbench", "prune"],
            vec!["subdollarbench", "doctor"],
            vec!["subdollarbench", "status", "--json"],
        ] {
            assert!(
                Cli::try_parse_from(&args).is_ok(),
                "failed to parse {args:?}"
            );
        }

        // Subtle flag validation: a bad harness and a bad effort never parse
        // into a valid command.
        assert!(
            Cli::try_parse_from(["subdollarbench", "run", "-m", "m", "--harness", "nope"]).is_err()
        );
        assert!(Cli::try_parse_from(["subdollarbench", "campaign", "dry-run", "--name"]).is_err());
    }
}
