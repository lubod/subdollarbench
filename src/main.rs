use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use colored::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use subdollarbench::config::{Cli, Commands, TaskType};
use subdollarbench::pipeline::{BenchmarkConfig, BenchmarkPipeline, PipelineLogger};
use subdollarbench::report::{LeaderboardManager, PortalExporter, RunJudge, RunManifest};
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
        } => {
            let total_trials = trials.max(1);
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

            if total_trials > 1 && !subrun_results.is_empty() {
                let _ = LeaderboardManager::consolidate_multi_trial_run(
                    &parent_run_id,
                    &subrun_results,
                    !no_save,
                    batch_drift,
                );
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
                    let v = ProxyVerifier::new(p, None, vec![]);
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
