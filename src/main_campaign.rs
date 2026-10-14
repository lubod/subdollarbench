//! Campaign CLI: list / dry-run / run. The orchestration math lives in
//! `subdollarbench::campaign`; this module wires the real pipeline in and
//! persists per-stage evidence + the rollup under
//! `runs/<campaign_id>/` (workspace, sN/console.log, sN/manifest.json,
//! sN/stage.diff, campaign.json).
//!
//! Campaign stages deliberately do NOT go to the single-shot boards:
//! `save_results: false` keeps the leaderboard/DB clean — a campaign row is
//! a ticket, not a benchmark run. The rollup is the campaign's record.

use anyhow::{Context, Result};
use colored::*;
use std::path::Path;
use std::sync::{Arc, Mutex};

use subdollarbench::campaign::{
    self, CampaignRollup, CampaignRunOptions, CampaignStageConfig, LoadedCampaign, StageOutcome,
    StageSpec,
};
use subdollarbench::config::{Harness, TaskType};
use subdollarbench::pipeline::{BenchmarkConfig, BenchmarkPipeline, PipelineLogger};
use subdollarbench::report::RunArchiver;

/// Tee logger: live output for the operator + a buffered stage console +
/// an append-only `live.log` the GUI tails while the campaign runs.
struct CampaignLogger {
    buf: Arc<Mutex<Vec<String>>>,
    live: std::path::PathBuf,
}

impl PipelineLogger for CampaignLogger {
    fn log(&self, msg: &str) {
        println!("{msg}");
        let ts = chrono::Utc::now().format("%H:%M:%S").to_string();
        if let Ok(mut b) = self.buf.lock() {
            b.push(format!("[{ts}] {msg}"));
        }
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.live)
            {
                let _ = writeln!(f, "[{ts}] {msg}");
            }
        }
    }
}

/// Atomic-ish JSON write (tmp + rename): the GUI polls these files while
/// the campaign runs, so it must never observe a torn document.
fn atomic_write_json(path: &Path, v: &serde_json::Value) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn status_path(cdir: &Path) -> std::path::PathBuf {
    cdir.join("status.json")
}

/// Publish the live campaign view: state + current ticket + rollup-so-far.
fn write_status(
    cdir: &Path,
    state: &str,
    current_stage: Option<u32>,
    rollup: Option<&CampaignRollup>,
) {
    let mut v = serde_json::json!({
        "state": state,
        "current_stage": current_stage,
        "updated_at": chrono::Utc::now().to_rfc3339(),
    });
    if let Some(r) = rollup {
        v["rollup"] = serde_json::to_value(r).unwrap_or(serde_json::Value::Null);
    }
    atomic_write_json(&status_path(cdir), &v);
}

/// Mark the ticket as running without discarding the rollup written so far.
fn bump_current_stage(cdir: &Path, stage: u32) {
    let mut v = std::fs::read_to_string(status_path(cdir))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    v["state"] = serde_json::json!("running");
    v["current_stage"] = serde_json::json!(stage);
    v["updated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    if v.get("rollup").is_none() {
        v["rollup"] = serde_json::Value::Null;
    }
    atomic_write_json(&status_path(cdir), &v);
}

pub fn list() -> Result<()> {
    let root = campaign::campaigns_root();
    let names = campaign::list_campaigns();
    if names.is_empty() {
        println!("no campaigns under {}", root.display());
        return Ok(());
    }
    for n in names {
        match campaign::load_campaign(&n) {
            Ok(lc) => println!(
                "{}: {} ({} stages, task {})",
                n.bold(),
                lc.spec.title,
                lc.spec.stages.len(),
                lc.spec.task
            ),
            Err(e) => println!("{}: INVALID — {e:#}", n.red()),
        }
    }
    Ok(())
}

pub fn dry_run(name: &str, model: &str) -> Result<()> {
    let lc = campaign::load_campaign(name).context("load campaign")?;
    println!(
        "{} — {} (task {}, {} stages) · model {}",
        lc.spec.name.bold(),
        lc.spec.title,
        lc.spec.task,
        lc.spec.stages.len(),
        model
    );
    if !lc.spec.description.is_empty() {
        println!("{}", lc.spec.description);
    }
    let mut total = 0.0;
    for s in &lc.spec.stages {
        total += s.budget_usd;
        println!(
            "  {} [{}] {} — acceptance {:?}, budget ${:.2}, max_turns {}, feedback {}",
            format!("s{}", s.id).bold(),
            s.kind,
            s.title,
            s.acceptance.redis_stages,
            s.budget_usd,
            s.max_turns,
            s.feedback_rounds
        );
        println!("      prompt: {}", lc.prompt_path(s).display());
    }
    println!("sum of stage budgets: ${total:.2} (campaign cap is set at run time)");
    println!(
        "workspace: runs/<campaign_id>/workspace (continuity across tickets, git-committed per stage)"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    name: String,
    model: String,
    effort: String,
    harness: Harness,
    budget_usd: f64,
    timeout_min: u64,
    api_key: Option<String>,
    eval_only: bool,
    workspace: Option<String>,
) -> Result<()> {
    let lc = campaign::load_campaign(&name).context("load campaign")?;
    // Foreground campaigns use the canonical worker-0 ports (6379/6380/…);
    // a live engine wave holds them. Fail fast instead of colliding.
    if let Ok(v) = subdollarbench::remote::get_json("http://127.0.0.1:3000", "/api/status").await {
        if v.get("is_running")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            anyhow::bail!(
                "engine is running a wave; campaigns run foreground on worker-0 ports — \
                 stop the wave first (`subdollarbench stop`) or run campaigns from an idle engine"
            );
        }
    }
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let model_tag = model.replace(['/', ':'], "_");
    let campaign_id = format!("campaign_{}_{}_{}", lc.spec.name, model_tag, ts);
    let runs_dir = RunArchiver::resolve_runs_dir();
    let cdir = runs_dir.join(&campaign_id);
    std::fs::create_dir_all(&cdir)?;
    let workdir = cdir.join("workspace");

    if let Some(src) = &workspace {
        let src = Path::new(src);
        if !src.is_dir() {
            anyhow::bail!("--workspace {} is not a directory", src.display());
        }
        copy_workspace(src, &workdir)?;
        println!("workspace seeded from {}", src.display());
    }

    println!(
        "{}",
        format!(
            "=== CAMPAIGN {} · {} · {} · cap ${:.2} ===",
            lc.spec.name, model, harness, budget_usd
        )
        .bold()
    );
    println!("campaign dir: {}", cdir.display());
    println!("workspace:    {}", workdir.display());
    if eval_only {
        println!(
            "{}",
            "[EVAL-ONLY] tickets are skipped; each stage verifies the existing workspace.".yellow()
        );
    }

    let opts = CampaignRunOptions {
        campaign_id: campaign_id.clone(),
        model: model.clone(),
        harness: harness.to_string(),
        effort: effort.clone(),
        budget_usd,
    };

    write_status(&cdir, "running", Some(0), None);

    let on_stage = |r: &CampaignRollup| write_status(&cdir, "running", None, Some(r));
    let rollup = campaign::run_campaign(
        &lc,
        &opts,
        |stage| {
            let stage = stage.clone();
            let lc = lc.clone();
            let model = model.clone();
            let effort = effort.clone();
            let api_key = api_key.clone();
            let cdir = cdir.clone();
            let workdir = workdir.clone();
            let campaign_id = campaign_id.clone();
            async move {
                run_one_stage(
                    &lc,
                    &stage,
                    &campaign_id,
                    &model,
                    &effort,
                    harness,
                    api_key,
                    timeout_min,
                    &cdir,
                    &workdir,
                    eval_only,
                )
                .await
            }
        },
        Some(&on_stage),
    )
    .await?;

    write_status(&cdir, "done", None, Some(&rollup));
    let rollup_path = cdir.join("campaign.json");
    std::fs::write(&rollup_path, serde_json::to_string_pretty(&rollup)?)?;
    print_rollup(&rollup);
    println!("rollup: {}", rollup_path.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_one_stage(
    lc: &LoadedCampaign,
    stage: &StageSpec,
    campaign_id: &str,
    model: &str,
    effort: &str,
    harness: Harness,
    api_key: Option<String>,
    timeout_min: u64,
    cdir: &Path,
    workdir: &Path,
    eval_only: bool,
) -> Result<StageOutcome> {
    let run_id = format!("{campaign_id}_s{}", stage.id);
    let stage_dir = cdir.join(format!("s{}", stage.id));
    std::fs::create_dir_all(&stage_dir)?;
    bump_current_stage(cdir, stage.id);

    let buf = Arc::new(Mutex::new(Vec::<String>::new()));
    let logger: Arc<dyn PipelineLogger> = Arc::new(CampaignLogger {
        buf: buf.clone(),
        live: cdir.join("live.log"),
    });
    logger.log(&format!(
        "─── ticket {} [{}] {} (acceptance {:?}, budget ${:.2}) ───",
        stage.id, stage.kind, stage.title, stage.acceptance.redis_stages, stage.budget_usd
    ));

    let config = BenchmarkConfig {
        model: model.to_string(),
        task: TaskType::parse(&lc.spec.task)?,
        effort: effort.to_string(),
        harness,
        max_turns: stage.max_turns,
        budget_usd: stage.budget_usd,
        timeout_min,
        api_key,
        workdir: workdir.to_path_buf(),
        eval_only,
        run_id: Some(run_id.clone()),
        // A ticket is not a single-shot board row.
        save_results: false,
        review_model: None,
        batch_reference: None,
        worker: 0,
        feedback_rounds: stage.feedback_rounds,
        campaign_stage: Some(CampaignStageConfig {
            campaign: lc.spec.name.clone(),
            stage: stage.id,
            prompt_path: lc.prompt_path(stage),
            continuity: stage.id > 0,
            accepted_stages: stage.acceptance.redis_stages.clone(),
        }),
    };

    let output = BenchmarkPipeline::execute(config, logger, None).await?;

    // Evidence: console + manifest + git commit + diff.
    let console = buf.lock().unwrap().join("\n");
    std::fs::write(stage_dir.join("console.log"), console)?;
    if let Ok(m) = serde_json::to_string_pretty(&output.manifest) {
        let _ = std::fs::write(stage_dir.join("manifest.json"), m);
    }
    let passed = output.benchmark_result.pass_rate >= 100.0 && !output.disqualified;
    let verdict = if passed {
        "pass"
    } else if output.agent_failed || output.infra_error {
        "fault"
    } else {
        "fail"
    };
    let (git_commit, git_stat) =
        match campaign::git_commit_stage(workdir, stage.id, &stage.title, verdict) {
            Ok((h, s)) => {
                if let Ok(patch) = campaign::git_head_patch(workdir) {
                    let _ = std::fs::write(stage_dir.join("stage.diff"), patch);
                }
                (Some(h), Some(s))
            }
            Err(e) => {
                eprintln!("  [CAMPAIGN] git commit failed: {e:#}");
                (None, None)
            }
        };

    let br = &output.benchmark_result;
    Ok(StageOutcome {
        run_id,
        passed,
        pass_rate: br.pass_rate,
        passed_stages: br.passed_stages,
        total_stages: br.total_stages,
        stage_results: output.manifest.stages.clone(),
        effective_cost_usd: br.effective_cost_usd.unwrap_or(br.total_cost_usd),
        turns: output.manifest.turns,
        duration_seconds: Some(output.manifest.duration_seconds),
        agent_failed: output.agent_failed,
        infra_error: output.infra_error,
        disqualified: output.disqualified,
        git_commit,
        git_stat,
    })
}

/// Recursive copy of a seed workspace; `.git` is excluded so the campaign
/// starts its own stage history (archived candidates may carry their own).
fn copy_workspace(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_workspace(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn print_rollup(r: &CampaignRollup) {
    println!("\n{}", "=== CAMPAIGN ROLLUP ===".bold());
    for s in &r.stages {
        let mark = if s.passed {
            "PASS".green().to_string()
        } else {
            match s.status.as_str() {
                "fault" => "FAULT".yellow().to_string(),
                _ => "FAIL".red().to_string(),
            }
        };
        let reg = if s.regressed.is_empty() {
            String::new()
        } else {
            format!(
                " · regressions {:?} (sev {})",
                s.regressed, s.regression_severity
            )
            .red()
            .to_string()
        };
        println!(
            "  s{:<2} [{}] {:32} {} {}/{} (${:.4}, {} turns){}{}",
            s.id,
            s.kind,
            s.title,
            mark,
            s.passed_stages,
            s.total_stages,
            s.effective_cost_usd,
            s.turns.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
            reg,
            s.git_commit
                .as_ref()
                .map(|h| format!(" · {h}"))
                .unwrap_or_default()
        );
    }
    match r.reached {
        Some(id) => println!("reached: ticket {id}"),
        None => {
            let first = r.stages.first();
            let stopped = match first.map(|s| s.status.as_str()) {
                Some("fault") => "stopped: ticket 0 fault (harness/provider, not a model verdict)",
                _ => "reached: none (ticket 0 failed)",
            };
            println!("{stopped}");
        }
    }
    if r.over_budget {
        println!(
            "{}",
            "stopped: campaign budget reached (over_budget)".yellow()
        );
    }
    println!(
        "regression-free tickets: {} · total ${:.4} · tickets/$ {}",
        r.regression_free_tickets,
        r.total_cost_usd,
        r.tickets_per_dollar()
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "--".into())
    );
}
