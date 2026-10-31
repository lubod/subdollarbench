use anyhow::{Context, Result};
use clap::Parser;
use colored::*;
use std::path::Path;

use subdollarbench::config::{Cli, Commands};
use subdollarbench::remote;
use subdollarbench::report::PortalExporter;
use subdollarbench::web;

mod main_campaign;
mod main_queue;
mod tui;

#[cfg(test)]
fn apply_trial_suffix(run_id: &str, trial: Option<u32>) -> String {
    match trial {
        Some(t) if !run_id.contains("_trial") => format!("{run_id}_trial{t}"),
        _ => run_id.to_string(),
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

        Commands::Portal {
            runs_dir,
            out,
            variance,
            secret_scan,
            ..
        } => {
            if let Some(dir) = secret_scan {
                PortalExporter::secret_scan(std::path::Path::new(&dir))?;
                println!("secret scan: clean");
                return Ok(());
            }
            // Round-12 P1: the legacy pre-sdb-sql page is retired; the
            // public surface is the sdb-sql board (same as the engine's
            // rebuild endpoint and the campaign finish).
            println!("{}", "Exporting the sdb-sql board...".bold().cyan());
            let summary = PortalExporter::export_sql_board(&runs_dir, &out)?;
            println!("{}", "✅ Portal exported!".bold().green());
            println!("  Output: {}", out);
            println!("  Careers: {}", summary.run_count);
            if variance {
                println!("{}", "Career variance:".bold().cyan());
                print!("{}", PortalExporter::variance_report(&runs_dir)?);
            }
            println!("  Deploy: scripts/publish.sh (checked dry run; --yes pushes)");
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
                worker,
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
                    worker,
                )
                .await?
            }
        },

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

        Commands::Perfscore { workspace, json } => {
            subdollarbench::verifier::sql::perfscore::cli(&workspace, json)?;
        }

        Commands::Tui { server, interval } => tui::run(server, interval).await?,
        Commands::Status { server, json } => main_queue::handle_status(server, json).await?,

        Commands::Queue { server, json } => main_queue::handle_queue(server, json).await?,

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
    use subdollarbench::report::RunManifest;

    fn verdict_code(m: &RunManifest) -> i32 {
        match m.status.as_str() {
            "completed" => 0,
            "failed_tests" | "disqualified_cheat" => 1,
            _ => 2,
        }
    }

    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;
    use subdollarbench::config::Harness;

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
                assert_eq!(timeout_min, 120);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // P1: the phase/chain form parses too.
        let args = subdollarbench::campaign::launcher_args_with_phase(
            "openrouter/meta/muse-spark-1.3-contributor",
            "high",
            0.99,
            subdollarbench::campaign::CELL_WALL_MIN,
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

    /// R48 F1: `campaign run --worker 3` reaches the CLI and every SQL
    /// port moves to base + 300 (candidate, reference, proxy pool).
    #[test]
    fn r48_f1_worker_flag_sets_ports() {
        use subdollarbench::config::{CampaignAction, TaskType};
        use subdollarbench::web::server::worker_port;
        let cli = Cli::try_parse_from([
            "subdollarbench",
            "campaign",
            "run",
            "--name",
            "sdb-sql",
            "--model",
            "m",
            "--worker",
            "3",
        ])
        .expect("campaign run --worker must parse");
        match cli.command {
            Commands::Campaign {
                action: CampaignAction::Run { worker, .. },
            } => assert_eq!(worker, 3),
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(worker_port(TaskType::Sql.default_port(), 3), 5780);
        assert_eq!(worker_port(5481, 3), 5781);
        assert_eq!(worker_port(8101, 3), 8401);
    }

    /// R45 F5: the CLI surface the reviewer runs on a finished workspace.
    #[test]
    fn r45_f5_perfscore_command_parses() {
        let cli = Cli::try_parse_from([
            "subdollarbench",
            "perfscore",
            "--workspace",
            "/tmp/workspace",
            "--json",
        ])
        .expect("perfscore --workspace must parse");
        match cli.command {
            Commands::Perfscore { workspace, json } => {
                assert_eq!(workspace, "/tmp/workspace");
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    /// R51 F2: the CLI's secret scan names file, line and pattern, never the
    /// value; a clean tree passes.
    #[tokio::test]
    async fn r51_f2_secret_scan_cli() {
        let dir = std::env::temp_dir().join(format!(
            "r51_secret_scan_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cli = |scan: String| Cli {
            command: Commands::Portal {
                runs_dir: "./runs".to_string(),
                results_dir: "./results".to_string(),
                out: "./portal".to_string(),
                base_url: String::new(),
                keep: 50,
                variance: false,
                secret_scan: Some(scan),
            },
        };
        for (name, text, value, pattern) in [
            (
                "key.txt",
                "OPENROUTER_API_KEY=plainsecret123\n",
                "plainsecret123",
                "assignment",
            ),
            (
                "bearer.txt",
                "authorization: bearer bearerabc123XYZ\n",
                "bearerabc123XYZ",
                "bearer",
            ),
            (
                "sk.txt",
                "planted sk-or-v1-abcdefghijklmnop\n",
                "abcdefghijklmnop",
                "openrouter_key",
            ),
        ] {
            let path = dir.join(name);
            fs::write(&path, text).unwrap();
            let err = run_cli(cli(dir.to_string_lossy().to_string()))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(name), "file not named: {err}");
            assert!(err.contains(":1:"), "line not named: {err}");
            assert!(err.contains(pattern), "pattern not named: {err}");
            assert!(!err.contains(value), "the value leaked: {err}");
            fs::remove_file(&path).unwrap();
        }
        assert!(run_cli(cli(dir.to_string_lossy().to_string()))
            .await
            .is_ok());
        let _ = fs::remove_dir_all(&dir);
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
                "stages": [{
                    "id": 0, "title": "t0", "type": "genesis", "passed": true,
                    "pass_rate": 100.0, "passed_stages": 1, "total_stages": 1,
                    "regressed": [], "regression_severity": 0,
                    "effective_cost_usd": 0.01, "status": "passed",
                    "run_id": "run0", "variant": "full"
                }],
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
        // R47 F5: a ranked career carries its ticket fingerprint.
        let stamp = cell.join("s0_full");
        fs::create_dir_all(&stamp).unwrap();
        fs::write(
            stamp.join("manifest.json"),
            serde_json::json!({
                "run_id": "run0",
                "rules_version": subdollarbench::report::rules::RULES_VERSION
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
                variance: false,
                secret_scan: None,
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

    /// Consolidated parent result with one subrun per pass rate. Equal pass
    /// rates and no throughput make it `Stability::Stable` by construction
    /// (so `stability`/`extend` never hit their `process::exit` arms).

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

    // ---- Synthetic-fixture coverage (offline) ---------------------------

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
                    worker: 0,
                },
            },
        })
        .await
        .is_err());

        drop(env);
    }
}

#[cfg(test)]
mod r41_cli_tests {
    use super::*;
    use clap::CommandFactory;

    /// R41 F1: the CLI help is the campaign surface only.
    #[test]
    fn r41_f1_cli_is_sdb_sql_only() {
        let help = Cli::command().render_help().to_string();
        let commands: Vec<&str> = help
            .lines()
            .filter_map(|l| {
                let t = l.trim_start();
                (l.starts_with("  ") && !t.starts_with('-'))
                    .then(|| t.split_whitespace().next().unwrap_or(""))
            })
            .collect();
        for gone in [
            "run",
            "eval",
            "leaderboard",
            "matrix-report",
            "review-run",
            "regen-reviews",
            "stability",
            "matrix",
            "extend",
            "backfill",
            "repair",
            "cost-annotate",
            "annotate-cutoff",
            "replay",
            "migrate",
            "ps",
            "enqueue",
            "queue-extend",
            "sdiff",
            "review",
            "lboard",
        ] {
            assert!(
                !commands.contains(&gone),
                "retired command {gone} is still in help:\n{help}"
            );
        }
        for kept in [
            "campaign", "ui", "portal", "status", "queue", "sandbox", "doctor", "prompt",
        ] {
            assert!(
                help.contains(kept),
                "kept command {kept} is missing:\n{help}"
            );
        }
        for task in ["redis", "http", "pdf", "wal", "proxy", "pubsub", "binproto"] {
            assert!(
                !help.to_lowercase().contains(task),
                "retired task {task} leaks into help:\n{help}"
            );
        }
    }
}
