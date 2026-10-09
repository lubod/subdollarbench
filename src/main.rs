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
    BinprotoVerifier, HttpVerifier, PdfVerifier, ProxyVerifier, PubsubVerifier, RedisVerifier,
    WalVerifier,
};
use subdollarbench::web;
use subdollarbench::web::server::compute_pass_at_k;

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
}

struct TrialBatchOutput {
    results: Vec<BenchmarkRunResult>,
    passing: u32,
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
        } => {
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
            let mut total_trials = trials.max(1);
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

            for trial_idx in 1..=total_trials {
                if total_trials > 1 {
                    println!(
                        "{}",
                        format!("=== Starting Subrun {}/{} ===", trial_idx, total_trials)
                            .bold()
                            .cyan()
                    );
                }

                let run_id = if total_trials > 1 {
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
                            continue;
                        }
                        if passing {
                            passing_trials += 1;
                        }
                        if total_trials > 1 {
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
            }

            let batch_drift = match batch_ref {
                Some(b) => Some(b.finish(&|m| println!("{}", m)).await.drift),
                None => None,
            };

            let mut merged_opt = if total_trials > 1 && !subrun_results.is_empty() {
                LeaderboardManager::consolidate_multi_trial_run(
                    &parent_run_id,
                    &subrun_results,
                    !no_save,
                    batch_drift,
                )
            } else {
                None
            };

            if extend_if_unstable {
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
                                start_idx: total_trials + 1,
                                count: add,
                            },
                            logger.clone(),
                        )
                        .await?;
                        if batch.results.is_empty() {
                            eprintln!(
                                "[EXTEND] all extension trials failed; keeping current merge."
                            );
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
                        // Take ownership back for the reassignment below
                        // (borrow ends here; NLL releases it).
                        let _ = &kept_drift;
                        merged_opt = LeaderboardManager::consolidate_multi_trial_run(
                            &parent_run_id,
                            &subrun_results,
                            !no_save,
                            kept_drift,
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

        Commands::Portal {
            runs_dir,
            results_dir,
            out,
            base_url,
            keep,
        } => {
            println!("{}", "Exporting static portal...".bold().cyan());
            let summary = PortalExporter::export(&runs_dir, &results_dir, &out, &base_url, keep)?;
            println!("{}", "✅ Portal exported!".bold().green());
            println!("  Output: {}", out);
            println!("  Runs:   {}", summary.run_count);
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
        } => {
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
                            );
                            match remote::post_json(&server, "/api/queue", &body).await {
                                Ok(resp) => {
                                    let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                    println!("enqueued {id}: {m} {t} {e} x{trials}");
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
            for m in &models {
                for e in &efforts {
                    for t in &tasks {
                        let ex = if extra.trim().is_empty() {
                            String::new()
                        } else {
                            format!(" {}", extra.trim())
                        };
                        println!(
                            "subdollarbench run --model {} --task {} --effort {} --harness {} --trials {}{}",
                            m, t, e, harness, trials, ex
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
                    start_idx: merged.subruns.len() as u32 + 1,
                    count: trials.max(1),
                },
                logger,
            )
            .await?;
            if batch.results.is_empty() {
                anyhow::bail!("all extension trials failed; parent merge untouched");
            }
            let mut combined = merged.subruns.clone();
            combined.extend(batch.results);
            let total = combined.len();
            let passing = combined.iter().filter(|r| r.pass_rate >= 100.0).count();
            let final_merged =
                LeaderboardManager::consolidate_multi_trial_run(&pid, &combined, true, batch.drift);
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
        } => {
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
    use subdollarbench::report::{BenchmarkRunResult, RunManifest, RunTokenUsage};

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

    #[tokio::test]
    async fn test_main_cli_portal_export() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_main_portal_{}", std::process::id()));
        let runs_dir = temp_dir.join("runs");
        let results_dir = temp_dir.join("results");
        let out_dir = temp_dir.join("portal");
        let run_id = "redis_test_model_20260910_000000";
        let run_dir = runs_dir.join(run_id);
        let _ = fs::create_dir_all(run_dir.join("workspace"));
        fs::write(run_dir.join("workspace/main.py"), "print(1)").unwrap();
        let manifest = RunManifest {
            run_id: run_id.to_string(),
            model: "test_model".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Python".to_string(),
            effort: Some("low".to_string()),
            effort_effective: None,
            harness: None,
            started_at: "2026-09-04T12:00:00Z".to_string(),
            completed_at: "2026-09-04T12:01:00Z".to_string(),
            duration_seconds: 60.0,
            turns: Some(3),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            stages: Vec::new(),
            robustness: Vec::new(),
            throughput_req_sec: None,
            reference_throughput_req_sec: None,
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            egress_enforced: None,
            tokens: RunTokenUsage {
                prompt_tokens: 100,
                cached_tokens: 50,
                completion_tokens: 20,
                total_tokens: 120,
            },
            cost_usd: 0.01,
            cost_source: None,
            cost_formula_usd: None,
            effective_cost_usd: Some(0.015),
            savings_percent: 10.0,
            efficiency_score: 66.6,
            throughput_score: Some(66.6),
            files: Vec::new(),
            canary_echoed: None,
            cutoff_turn1: None,
            env: None,
            git_commit: None,
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            trial_pass_rates: Vec::new(),
            is_published: None,
            method_version: Some("0.1.0".to_string()),
            temperature: None,
            top_p: None,
            seed: None,
            prompt_sha: None,
            omp_version: None,
            harness_version: None,

            rules_version: None,
        };
        fs::write(
            run_dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(run_dir.join("console.log"), "key=sk-or-v1-SECRET123 done").unwrap();
        let _ = fs::create_dir_all(&results_dir);
        let result = serde_json::json!({
            "id": run_id, "model": "test_model", "task": "redis",
            "language": "Python", "effort": "low",
            "pass_rate": 100.0, "passed_stages": 4, "total_stages": 4,
            "throughput_req_sec": null, "reference_throughput_req_sec": null,
            "prompt_tokens": 100, "cached_tokens": 50, "completion_tokens": 20,
            "total_cost_usd": 0.01, "effective_cost_usd": 0.015,
            "duration_seconds": 60.0, "turns": 3, "savings_percent": 10.0,
            "efficiency_score": 66.6, "throughput_score": 66.6,
            // Must be >= portal::PUBLISH_FREEZE_TS or the exporter drops the row
            // by design and the index assertion below cannot pass. Naive ISO,
            // matching the row format the freeze is stored in.
            "timestamp": "2026-09-28T12:01:00", "method_version": "0.1.0"
        });
        fs::write(
            results_dir.join(format!("{}.json", run_id)),
            serde_json::to_string(&result).unwrap(),
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
        assert!(index.contains(run_id));
        let log =
            fs::read_to_string(out_dir.join("runs").join(run_id).join("console.log")).unwrap();
        assert!(!log.contains("SECRET123"));

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
}
