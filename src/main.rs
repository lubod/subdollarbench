use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use colored::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use subdollarbench::config::{Cli, Commands, TaskType};
use subdollarbench::pipeline::{BenchmarkConfig, BenchmarkPipeline, PipelineLogger};
use subdollarbench::report::{
    BenchmarkRunResult, LeaderboardManager, PortalExporter, RunJudge, RunManifest,
    assess_stability, Stability,
};
use subdollarbench::verifier::{DnsVerifier, HttpVerifier, PdfVerifier, ProxyVerifier, PubsubVerifier, RedisVerifier, WalVerifier};
use subdollarbench::web;
use subdollarbench::web::server::compute_pass_at_k;

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

fn resolve_run_id(runs_dir: &str, prefix: &str) -> Result<String> {
    let path = Path::new(runs_dir);
    if path.join(prefix).is_dir() {
        return Ok(prefix.to_string());
    }
    let mut hits: Vec<String> = std::fs::read_dir(path)
        .with_context(|| format!("read runs dir {runs_dir}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
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

fn parse_task(s: &str) -> Result<TaskType> {
    match s.to_lowercase().as_str() {
        "redis" => Ok(TaskType::Redis),
        "http" => Ok(TaskType::Http),
        "dns" => Ok(TaskType::Dns),
        "pdf" => Ok(TaskType::Pdf),
        "wal" => Ok(TaskType::Wal),
        "proxy" => Ok(TaskType::Proxy),
        "pubsub" => Ok(TaskType::Pubsub),
        _ => anyhow::bail!("unknown task '{s}'"),
    }
}

fn resolve_parent_id(results_dir: &str, prefix: &str) -> Result<String> {
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
async fn run_trial_batch(p: TrialBatchParams, logger: Arc<CliPipelineLogger>) -> Result<TrialBatchOutput> {
    let batch_ref = if p.count > 1 {
        match subdollarbench::bench::batch_ref::BatchRef::start(
            &p.task,
            &p.parent_run_id,
            &|m| println!("{}", m),
        ) {
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
        println!("{}", format!("=== Starting Extension Subrun {} ===", idx).bold().cyan());
        let run_id = format!("{}_trial{}", p.parent_run_id, idx);
        let pipe_config = BenchmarkConfig {
            model: p.model.clone(),
            task: p.task,
            effort: p.effort.clone(),
            max_turns: p.max_turns,
            budget_usd: p.budget_usd,
            timeout_min: p.timeout_min,
            api_key: p.api_key.clone(),
            workdir: p.workdir.clone(),
            eval_only: p.eval_only,
            run_id: Some(run_id.clone()),
            save_results: true,
            review_model: p.review_model.clone(),
            batch_reference: batch_ref.as_ref().map(|b| b.reference()),
        };
        match BenchmarkPipeline::execute(pipe_config, logger.clone(), None).await {
            Ok(output) => {
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
    let drift = batch_ref.map(|b| b.finish(&|m| println!("{}", m)).drift);
    Ok(TrialBatchOutput { results, passing, drift })
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Run {
            model,
            task,
            effort,
            max_turns,
            budget_usd,
            timeout_min,
            api_key,
            workdir,
            eval_only,
            trials,
            no_save,
            review_model,
            extend_if_unstable,
            max_trials,
        } => {
            let mut total_trials = trials.max(1);
            let mut passing_trials = 0;
            let logger = Arc::new(CliPipelineLogger);

            let work_path_buf = if Path::new(&workdir).is_absolute() {
                PathBuf::from(&workdir)
            } else {
                subdollarbench::config::get_repo_root().join(&workdir)
            };

            let run_timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
            let parent_run_id = format!(
                "{}_{}_{}",
                task,
                model.replace(['/', ':'], "_"),
                run_timestamp
            );
            let mut subrun_results = Vec::new();

            // Batch reference: one shared denominator for all trials.
            // Single-trial runs keep live per-trial measurement.
            let batch_ref = if total_trials > 1 {
                match subdollarbench::bench::batch_ref::BatchRef::start(
                    &task,
                    &parent_run_id,
                    &|m| println!("{}", m),
                ) {
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
                };

                let pipe_res = BenchmarkPipeline::execute(pipe_config, logger.clone(), None).await;

                match pipe_res {
                    Ok(output) => {
                        if output.benchmark_result.pass_rate == 100.0 {
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
                        eprintln!("  [ERROR] Subrun {}/{} execution failed: {}", trial_idx, total_trials, e);
                    }
                }
            }

            let batch_drift = batch_ref.map(|b| {
                b.finish(&|m| println!("{}", m)).drift
            });

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
                } else {
                    let max_total = max_trials.max(total_trials);
                    while total_trials < max_total {
                        let merged = match merged_opt {
                            Some(m) => m,
                            None => break,
                        };
                        let (stab, reason) = assess_stability(&merged.subruns);
                        if stab == Stability::Stable {
                            println!("[EXTEND] stable: {}; done.", reason);
                            break;
                        }
                        let add = (max_total - total_trials).min(3);
                        println!("[EXTEND] unstable ({}); running {} more trial(s)...", reason, add);
                        let batch = run_trial_batch(
                            TrialBatchParams {
                                model: model.clone(),
                                task,
                                effort: effort.clone(),
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
                            eprintln!("[EXTEND] all extension trials failed; keeping current merge.");
                            break;
                        }
                        total_trials += batch.results.len() as u32;
                        passing_trials += batch.passing as usize;
                        subrun_results.extend(batch.results);
                        merged_opt = LeaderboardManager::consolidate_multi_trial_run(
                            &parent_run_id,
                            &subrun_results,
                            !no_save,
                            batch.drift,
                        );
                    }
                }
            }

            if total_trials > 1 {
                let pass_at_1 = compute_pass_at_k(total_trials as usize, passing_trials, 1) * 100.0;
                let pass_at_k =
                    compute_pass_at_k(total_trials as usize, passing_trials, total_trials as usize)
                        * 100.0;
                println!(
                    "\n{}",
                    "=== Multi-Trial Pass@k Summary ===".bold().magenta()
                );
                println!("  Total Trials:   {}", total_trials);
                println!("  Passing Trials: {}", passing_trials);
                println!("  Pass@1:         {:.1}%", pass_at_1);
                println!("  Pass@{}:         {:.1}%", total_trials, pass_at_k);
                println!("{}", "=================================".bold().magenta());
            }

            let results_dir_buf = subdollarbench::config::get_repo_root().join("results");
            let results_dir_str = results_dir_buf.to_str().unwrap_or("./results");
            let all = LeaderboardManager::load_all(results_dir_str);
            LeaderboardManager::print_table(&all);
        }

        Commands::Eval { task, port } => {
            let p = port.unwrap_or(match task {
                TaskType::Redis => 6379,
                TaskType::Http => 8080,
                TaskType::Dns => 5353,
                TaskType::Pdf => 8090,
                TaskType::Wal => 6390,
                TaskType::Proxy => 8091,
                TaskType::Pubsub => 8092,
            });
            println!("Evaluating {} on port {}...", task, p);
            match task {
                TaskType::Redis => {
                    let v = RedisVerifier::new(p, None);
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Http => {
                    let v = HttpVerifier::new(p, None);
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Dns => {
                    let v = DnsVerifier::new(p, None);
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Pdf => {
                    let v = PdfVerifier::new(p, None);
                    let res = v.run_all().await;
                    println!(
                        "Pass rate: {:.1}% ({}/{})",
                        res.pass_rate, res.passed_count, res.total_stages
                    );
                }
                TaskType::Wal => {
                    let v = WalVerifier::new(p, None);
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
                                subdollarbench::sandbox::docker::PROXY_UPSTREAM_PORTS[(i - 1) as usize],
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
                    let v = PubsubVerifier::new(p, None);
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
        } => {
            let runs_path = std::path::Path::new(&runs_dir);
            let mut ids: Vec<String> = std::fs::read_dir(runs_path)
                .with_context(|| format!("read runs dir {runs_dir}"))?
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|id| run_id.as_ref().map(|w| w == id).unwrap_or(true))
                .collect();
            ids.sort();
            let (mut done, mut skipped, mut failed) = (0, 0, 0);
            for id in &ids {
                let dir = runs_path.join(id);
                if dir.join("review.json").exists() && !force {
                    skipped += 1;
                    continue;
                }
                let manifest: RunManifest = match std::fs::read_to_string(dir.join("manifest.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                {
                    Some(m) => m,
                    None => {
                        eprintln!("[SKIP] {id}: no readable manifest.json");
                        failed += 1;
                        continue;
                    }
                };
                let task_prompt = std::fs::read_to_string(
                    std::path::Path::new("tasks").join(&manifest.task).join("prompt.md"),
                )
                .unwrap_or_default();
                match RunJudge::judge_run(
                    api_key.as_deref(),
                    &model,
                    &manifest.task,
                    &task_prompt,
                    &dir.join("workspace"),
                    &manifest,
                )
                .await
                {
                    Ok(review) => {
                        let text = serde_json::to_string_pretty(&review)?;
                        std::fs::write(dir.join("review.json"), text)?;
                        println!("[OK] {id}: {}+{} bullets", review.good.len(), review.bad.len());
                        done += 1;
                    }
                    Err(e) => {
                        eprintln!("[FAIL] {id}: {e:#}");
                        failed += 1;
                    }
                }
            }
            let summary = PortalExporter::export(&runs_dir, &results_dir, &out, &base_url, keep)?;
            println!("reviews: {done} regenerated, {skipped} skipped, {failed} failed; portal: {} runs", summary.run_count);
        }
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
                let installed = status.installed_version.as_deref().unwrap_or("Not installed");
                let latest = status.latest_version.as_deref().unwrap_or("Unknown");
                println!("--------------------------------------------------");
                println!("  Docker Image:    {}", if status.image_exists { "Found (subdollar-sandbox)".green() } else { "Missing".red() });
                println!("  Installed OMP:   {}", installed.cyan());
                println!("  Latest on npm:   {}", latest.yellow());
                println!("  Status:          {}", if status.up_to_date { "Up to date ✓".green() } else { "Update available ⚠️".yellow() });
                if let Some(err) = status.last_error {
                    println!("  Registry Notice: {}", err.dimmed());
                }
                println!("--------------------------------------------------");
            }
            subdollarbench::config::SandboxCommands::Update { version } => {
                println!("{}", "Updating subdollar-sandbox image with OMP...".bold().cyan());
                let new_status = subdollarbench::sandbox::omp_updater::update_sandbox_image(version.as_deref()).await?;
                println!(
                    "{} Installed OMP: {}",
                    "✅ Sandbox successfully updated!".bold().green(),
                    new_status.installed_version.as_deref().unwrap_or("Unknown").green()
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
                    && !(line.contains("Stage")
                        || line.contains("PASS")
                        || line.contains("FAIL"))
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
                .filter(|m: &RunManifest| task.as_ref().map(|t| &m.task == t).unwrap_or(true))
                .collect();
            rows.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            println!(
                "{:<64} {:<8} {:<16} {:>6}  {}",
                "run", "task", "status", "pass%", "stages"
            );
            for m in rows.iter().take(limit) {
                println!(
                    "{:<64} {:<8} {:<16} {:>5.1}%  {}/{}",
                    m.run_id, m.task, m.status, m.pass_rate, m.passed_stages, m.total_stages
                );
            }
        }

        Commands::Verdict { run, json, runs_dir } => {
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
                println!("turns:   {}", m.turns.map(|t| t.to_string()).unwrap_or("-".into()));
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
                let fa = sa.map(|s| if s.passed { "PASS" } else { "FAIL" }).unwrap_or("-");
                let fb = sb.map(|s| if s.passed { "PASS" } else { "FAIL" }).unwrap_or("-");
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
                println!("would remove {} containers, {} networks:", containers.len(), networks.len());
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
                for img in ["subdollar-sandbox", "nginx", "coredns/coredns", "redis"] {
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
            for port in [6379u16, 8080, 5353, 8090, 8091, 8092, 3000] {
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

        Commands::Stability { parent, results_dir } => {
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
                    task,
                    effort: effort.unwrap_or_else(|| merged.effort.clone().unwrap_or_else(|| "high".to_string())),
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
            let final_merged = LeaderboardManager::consolidate_multi_trial_run(
                &pid,
                &combined,
                true,
                batch.drift,
            );
            let pass_at_1 = compute_pass_at_k(total, passing, 1) * 100.0;
            let pass_at_k = compute_pass_at_k(total, passing, total) * 100.0;
            println!("extended {pid}: {total} trials, {passing} passing, Pass@1 {pass_at_1:.1}% Pass@{total} {pass_at_k:.1}%");
            if let Some(m) = final_merged {
                let (stab2, reason2) = assess_stability(&m.subruns);
                println!("stability now: {stab2:?} ({reason2})");
            }
        }

        Commands::Ui { port, host } => {
            web::UiServer::start(&host, port).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use subdollarbench::report::{BenchmarkRunResult, RunArchiver, RunManifest, RunTokenUsage};

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
            effective_cost_usd: Some(0.015),
            duration_seconds: Some(60.0),
            turns: Some(4),
            savings_percent: 10.0,
            efficiency_score: 100.0,
            throughput_score: Some(102.8),
            timestamp: "2026-09-04T12:00:00Z".to_string(),
            method_version: Some("0.1.0".to_string()),
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
            avg_pass_rate: None,
            stability: None,
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

        let cli_dns = Cli {
            command: Commands::Eval {
                task: TaskType::Dns,
                port: Some(59999),
            },
        };
        assert!(run_cli(cli_dns).await.is_ok());
    }

    #[tokio::test]
    async fn test_main_cli_portal_export() {

        let temp_dir = std::env::temp_dir().join(format!("test_main_portal_{}", std::process::id()));
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
            started_at: "2026-09-04T12:00:00Z".to_string(),
            completed_at: "2026-09-04T12:01:00Z".to_string(),
            duration_seconds: 60.0,
            turns: Some(3),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            stages: Vec::new(),
            throughput_req_sec: None,
            reference_throughput_req_sec: None,
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            tokens: RunTokenUsage {
                prompt_tokens: 100,
                cached_tokens: 50,
                completion_tokens: 20,
                total_tokens: 120,
            },
            cost_usd: 0.01,
            effective_cost_usd: Some(0.015),
            savings_percent: 10.0,
            efficiency_score: 66.6,
            throughput_score: Some(66.6),
            files: Vec::new(),
            env: None,
            git_commit: None,
            total_trials: None,
            pass_at_1: None,
            stability: None,
            trial_pass_rates: Vec::new(),
            is_published: None,
            method_version: Some("0.1.0".to_string()),
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
            "timestamp": "2026-09-04T12:01:00Z", "method_version": "0.1.0"
        });
        fs::write(results_dir.join(format!("{}.json", run_id)), serde_json::to_string(&result).unwrap()).unwrap();

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
        let log = fs::read_to_string(out_dir.join("runs").join(run_id).join("console.log")).unwrap();
        assert!(!log.contains("SECRET123"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_main_cli_run_eval_only_redis() {
        let temp_workdir =
            std::env::temp_dir().join(format!("test_main_run_redis_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_workdir);

        let cli_run = Cli {
            command: Commands::Run {
                model: "google/gemini-2.5-flash".to_string(),
                task: TaskType::Redis,
                effort: "low".to_string(),
                max_turns: 1,
                budget_usd: 0.10,
                timeout_min: 1,
                api_key: None,
                workdir: temp_workdir.to_string_lossy().to_string(),
                eval_only: true,
                trials: 2,
                no_save: true,
                review_model: None,
                extend_if_unstable: false,
                max_trials: 9,
            },
        };
        assert!(run_cli(cli_run).await.is_ok());

        let _ = fs::remove_dir_all(&temp_workdir);

        let runs_dir = RunArchiver::resolve_runs_dir();
        if let Ok(entries) = fs::read_dir(&runs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        if let Ok(entries) = fs::read_dir("./results") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
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
                model: "google/gemini-2.5-flash".to_string(),
                task: TaskType::Http,
                effort: "low".to_string(),
                max_turns: 1,
                budget_usd: 0.10,
                timeout_min: 1,
                api_key: None,
                workdir: temp_workdir.to_string_lossy().to_string(),
                eval_only: true,
                trials: 1,
                no_save: true,
                review_model: None,
                extend_if_unstable: false,
                max_trials: 9,
            },
        };
        assert!(run_cli(cli_run).await.is_ok());

        let _ = fs::remove_dir_all(&temp_workdir);

        let runs_dir = RunArchiver::resolve_runs_dir();
        if let Ok(entries) = fs::read_dir(&runs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        let res_dir = subdollarbench::config::get_repo_root().join("results");
        if let Ok(entries) = fs::read_dir(&res_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
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
                model: "google/gemini-2.5-flash".to_string(),
                task: TaskType::Redis,
                effort: "low".to_string(),
                max_turns: 1,
                budget_usd: 0.10,
                timeout_min: 1,
                api_key: None,
                workdir: temp_workdir.to_string_lossy().to_string(),
                eval_only: true,
                trials: 1,
                no_save: true,
                review_model: None,
                extend_if_unstable: false,
                max_trials: 9,
            },
        };
        assert!(run_cli(cli_run).await.is_ok());

        let _ = fs::remove_dir_all(&temp_workdir);

        let runs_dir = RunArchiver::resolve_runs_dir();
        if let Ok(entries) = fs::read_dir(&runs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        let res_dir = subdollarbench::config::get_repo_root().join("results");
        if let Ok(entries) = fs::read_dir(&res_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    #[tokio::test]
    async fn test_main_cli_ui_cancelled() {
        let cli_ui = Cli {
            command: Commands::Ui {
                host: "127.0.0.1".to_string(),
                port: 0,
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
        assert_eq!(verdict_code(&test_manifest_with_status("disqualified_cheat")), 1);
        assert_eq!(verdict_code(&test_manifest_with_status("agent_failed")), 2);
        assert_eq!(verdict_code(&test_manifest_with_status("agent_killed")), 2);
        assert_eq!(verdict_code(&test_manifest_with_status("mystery")), 2);
    }

    #[test]
    fn test_apply_trial_suffix() {
        assert_eq!(apply_trial_suffix("redis_m_20260917", Some(2)), "redis_m_20260917_trial2");
        assert_eq!(
            apply_trial_suffix("redis_m_20260917_trial1", Some(2)),
            "redis_m_20260917_trial1"
        );
        assert_eq!(apply_trial_suffix("redis_m_20260917", None), "redis_m_20260917");
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
