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
            s.acceptance.stages_for(&lc.spec.task),
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
    wall_min: Option<u64>,
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
        wall_min,
    };

    write_status(&cdir, "running", Some(0), None);

    let on_stage = |r: &CampaignRollup| write_status(&cdir, "running", None, Some(r));
    // Termination safety: a foreground campaign killed with Ctrl-C/SIGTERM
    // must cancel the running stage so the pipeline's cleanup guards reap
    // its containers (a SIGTERM default-kill skips Drop entirely and leaked
    // a reference container for 2h on 2026-10-14).
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = async {
                    match term.as_mut() {
                        Some(t) => {
                            t.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {},
            }
            eprintln!(
                "\n[CAMPAIGN] termination signal received: cancelling the running stage; containers will be reaped."
            );
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    let last_green: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let rollup = campaign::run_campaign(
        &lc,
        &opts,
        |attempt| {
            let job = StageJob {
                stage: attempt.stage.clone(),
                variant: attempt.variant,
                prompt_path: attempt.prompt_path.clone(),
                budget_usd: attempt.budget_usd,
                max_turns: attempt.max_turns,
                rollback_first: attempt.rollback_first,
            };
            let stage = job.stage.clone();
            let lc = lc.clone();
            let model = model.clone();
            let effort = effort.clone();
            let api_key = api_key.clone();
            let cdir = cdir.clone();
            let workdir = workdir.clone();
            let campaign_id = campaign_id.clone();
            let cancel = cancel.clone();
            let last_green = last_green.clone();
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
                    Some(cancel),
                    &job,
                    &last_green,
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
/// One attempt handed from the orchestrator to this executor: the ticket
/// (full or lite), its prompt/budget/turns, and whether the workspace must
/// roll back to the last green commit first.
struct StageJob {
    stage: StageSpec,
    variant: campaign::StageVariant,
    prompt_path: std::path::PathBuf,
    budget_usd: f64,
    max_turns: u32,
    rollback_first: bool,
}

async fn rollback_workspace(workdir: &Path, commit: &str) -> Result<()> {
    for args in [vec!["reset", "--hard", commit], vec!["clean", "-fd"]] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(workdir)
            .output()
            .with_context(|| format!("git {args:?} in {}", workdir.display()))?;
        if !out.status.success() {
            anyhow::bail!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
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
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    job: &StageJob,
    last_green: &Arc<Mutex<Option<String>>>,
) -> Result<StageOutcome> {
    let variant_tag = match job.variant {
        campaign::StageVariant::Full => "full",
        campaign::StageVariant::Lite => "lite",
    };
    if job.rollback_first {
        let anchor = { last_green.lock().unwrap().clone() };
        if let Some(commit) = anchor {
            println!(
                "{}",
                format!(
                    "  [ROLLBACK] stage {} {}: resetting workspace to the last green commit {}",
                    stage.id,
                    variant_tag,
                    &commit[..commit.len().min(8)]
                )
                .yellow()
            );
            rollback_workspace(workdir, &commit).await?;
        }
    }
    // sdb-sql: the harness-owned contract so far, written after any
    // rollback (a `git clean` would remove it) and before the session.
    if lc.spec.task == "sql" {
        let md = lc.render_spec_md(stage, &job.prompt_path)?;
        std::fs::write(workdir.join("SPEC.md"), md)
            .with_context(|| format!("write SPEC.md in {}", workdir.display()))?;
    }
    let run_id = format!("{campaign_id}_s{}_{variant_tag}", stage.id);
    let stage_dir = cdir.join(format!("s{}_{variant_tag}", stage.id));
    std::fs::create_dir_all(&stage_dir)?;
    bump_current_stage(cdir, stage.id);

    let buf = Arc::new(Mutex::new(Vec::<String>::new()));
    let logger: Arc<dyn PipelineLogger> = Arc::new(CampaignLogger {
        buf: buf.clone(),
        live: cdir.join("live.log"),
    });
    logger.log(&format!(
        "─── ticket {} [{}] {} (acceptance {:?}, budget ${:.2}) ───",
        stage.id,
        stage.kind,
        stage.title,
        stage.acceptance.stages_for(&lc.spec.task),
        stage.budget_usd
    ));

    let baseline_rps = read_baseline_rps(cdir);
    let config = BenchmarkConfig {
        model: model.to_string(),
        task: TaskType::parse(&lc.spec.task)?,
        effort: effort.to_string(),
        harness,
        max_turns: job.max_turns,
        budget_usd: job.budget_usd,
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
            prompt_path: job.prompt_path.clone(),
            continuity: stage.id > 0,
            accepted_stages: stage.acceptance.stages_for(&lc.spec.task).to_vec(),
        }),
        baseline_rps,
    };

    let output = BenchmarkPipeline::execute(config, logger, cancel).await?;

    // The genesis stage's measured GET throughput becomes the perf gate's
    // baseline (same bench method every stage; the manifest only carries a
    // throughput when the stage hit 100%).
    if stage.id == 0 {
        if let Some(tp) = output.manifest.throughput_req_sec {
            if tp > 0.0 {
                let v = serde_json::json!({
                    "throughput_req_sec": tp,
                    "reference_throughput_req_sec": output.manifest.reference_throughput_req_sec,
                    "source": "campaign genesis stage bench (median GET)",
                    "campaign": campaign_id,
                });
                let _ = std::fs::write(
                    cdir.join("baseline.json"),
                    serde_json::to_string_pretty(&v).unwrap_or_default(),
                );
            }
        }
    }

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
    let verdict_tag = if job.variant == campaign::StageVariant::Lite {
        format!("{verdict} (lite)")
    } else {
        verdict.to_string()
    };
    let (git_commit, git_stat) =
        match campaign::git_commit_stage(workdir, stage.id, &stage.title, &verdict_tag) {
            Ok((h, s)) => {
                if let Ok(patch) = campaign::git_head_patch(workdir) {
                    let _ = std::fs::write(stage_dir.join("stage.diff"), patch);
                }
                if passed {
                    // Rollback anchor: the last state that fully passed.
                    *last_green.lock().unwrap() = Some(h.clone());
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
    campaign::copy_dir_excluding_git(src, dst)
}

/// Campaign perf baseline: the genesis stage's measured GET throughput
/// (median of the bench reps). Missing/unreadable -> None, and the perf
/// gate is then skipped rather than failed.
fn read_baseline_rps(cdir: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(cdir.join("baseline.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("throughput_req_sec").and_then(|t| t.as_f64())
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

#[cfg(test)]
mod tests {
    use super::*;
    use subdollarbench::campaign::{CampaignRollup, StageRecord};

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("test_main_campaign_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stage_record(id: u32, passed: bool, status: &str) -> StageRecord {
        StageRecord {
            id,
            title: format!("Stage {id}"),
            kind: "build".to_string(),
            passed,
            pass_rate: if passed { 100.0 } else { 0.0 },
            passed_stages: u32::from(passed),
            total_stages: 1,
            regressed: Vec::new(),
            regression_severity: 0,
            effective_cost_usd: 0.01,
            turns: None,
            duration_seconds: None,
            status: status.to_string(),
            variant: "full".to_string(),
            tier: 1,
            points: if passed { 1.0 } else { 0.0 },
            run_id: format!("campaign_fixture_s{id}"),
            git_commit: None,
            git_stat: None,
        }
    }

    fn rollup_with(
        stages: Vec<StageRecord>,
        reached: Option<u32>,
        over_budget: bool,
        total_cost_usd: f64,
    ) -> CampaignRollup {
        CampaignRollup {
            over_wall: false,
            points: 0.0,
            max_points: 0.0,
            campaign_id: "campaign_fixture".to_string(),
            name: "fixture".to_string(),
            model: "m".to_string(),
            harness: "omp".to_string(),
            effort: "low".to_string(),
            task: "redis".to_string(),
            started_at: "2026-10-14T00:00:00Z".to_string(),
            finished_at: "2026-10-14T00:10:00Z".to_string(),
            stages,
            reached,
            regression_free_tickets: 1,
            total_cost_usd,
            over_budget,
        }
    }

    /// The tee logger feeds the stage console buffer and the live tail file.
    #[test]
    fn test_campaign_logger_buffer_and_live_log() {
        let dir = tmp("logger");
        let buf = Arc::new(Mutex::new(Vec::<String>::new()));
        let logger = CampaignLogger {
            buf: buf.clone(),
            live: dir.join("live.log"),
        };
        logger.log("ticket 0 started");

        let buf = buf.lock().unwrap();
        assert_eq!(buf.len(), 1);
        assert!(buf[0].contains("ticket 0 started"), "{:?}", buf[0]);
        let live = std::fs::read_to_string(dir.join("live.log")).unwrap();
        assert!(live.contains("ticket 0 started"), "{live}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// status.json round trip: write with and without a rollup, then bump the
    /// current stage without discarding what was written.
    #[test]
    fn test_status_write_bump_and_rollup_round_trip() {
        let dir = tmp("status");
        write_status(&dir, "running", Some(0), None);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(status_path(&dir)).unwrap()).unwrap();
        assert_eq!(v["state"], "running");
        assert_eq!(v["current_stage"], 0);
        assert_eq!(v["rollup"], serde_json::Value::Null);

        // With a rollup: the serialized view is embedded.
        let rollup = rollup_with(vec![stage_record(0, true, "passed")], Some(0), false, 0.01);
        write_status(&dir, "running", None, Some(&rollup));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(status_path(&dir)).unwrap()).unwrap();
        assert_eq!(v["rollup"]["campaign_id"], "campaign_fixture");

        // bump keeps the rollup and marks the ticket boundary.
        bump_current_stage(&dir, 1);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(status_path(&dir)).unwrap()).unwrap();
        assert_eq!(v["state"], "running");
        assert_eq!(v["current_stage"], 1);
        assert_eq!(v["rollup"]["campaign_id"], "campaign_fixture");

        // Missing status.json: bump starts from an empty object, no panic.
        let empty = tmp("status_missing");
        bump_current_stage(&empty, 2);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(status_path(&empty)).unwrap()).unwrap();
        assert_eq!(v["current_stage"], 2);
        assert_eq!(v["rollup"], serde_json::Value::Null);

        // Unwritable destination: atomic write fails closed, no partial file.
        let missing_parent = empty.join("no-such-dir");
        atomic_write_json(&missing_parent.join("status.json"), &serde_json::json!({}));
        assert!(!missing_parent.join("status.json").exists());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// baseline.json parsing: absent / unparseable / wrong shape all degrade
    /// to None; a real number is read back.
    #[test]
    fn test_read_baseline_rps_paths() {
        let dir = tmp("baseline");
        assert_eq!(read_baseline_rps(&dir), None);

        std::fs::write(dir.join("baseline.json"), "{not json").unwrap();
        assert_eq!(read_baseline_rps(&dir), None);

        std::fs::write(
            dir.join("baseline.json"),
            serde_json::json!({"throughput_req_sec": "fast"}).to_string(),
        )
        .unwrap();
        assert_eq!(read_baseline_rps(&dir), None);

        std::fs::write(
            dir.join("baseline.json"),
            serde_json::json!({
                "throughput_req_sec": 4242.5,
                "source": "campaign genesis stage bench (median GET)"
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(read_baseline_rps(&dir), Some(4242.5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Workspace seeding copies content but excludes any `.git` history.
    #[test]
    fn test_copy_workspace_excludes_git() {
        let src = tmp("copy_src");
        let dst = tmp("copy_dst");
        std::fs::write(src.join("start.sh"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("src/main.go"), "package main\n").unwrap();
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::write(src.join(".git/config"), "[core]\n").unwrap();

        copy_workspace(&src, &dst).unwrap();
        assert!(dst.join("start.sh").exists());
        assert!(dst.join("src/main.go").exists());
        assert!(!dst.join(".git").exists());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// print_rollup covers pass/fail/fault marks, regression annotations,
    /// commit hashes, reached/stopped phrasing, over-budget, and the
    /// zero-cost tickets/$ fallback.
    #[test]
    fn test_print_rollup_all_branches() {
        // Reached a late ticket with a regression, a fault fallback, commit.
        let mut pass_stage = stage_record(0, true, "passed");
        pass_stage.turns = Some(4);
        pass_stage.git_commit = Some("abc1234".to_string());
        let mut failed_stage = stage_record(1, false, "failed");
        failed_stage.regressed = vec![2];
        failed_stage.regression_severity = 3;
        let fault_stage = stage_record(2, false, "fault");
        print_rollup(&rollup_with(
            vec![pass_stage, failed_stage, fault_stage],
            Some(0),
            true,
            1.2345,
        ));

        // Ticket 0 fault, no reached ticket, zero cost -> "--" tickets/$.
        let fault0 = stage_record(0, false, "fault");
        print_rollup(&rollup_with(vec![fault0], None, false, 0.0));

        // Ticket 0 plain failure -> the none/failed phrasing.
        let failed0 = stage_record(0, false, "failed");
        print_rollup(&rollup_with(vec![failed0], None, false, 0.5));
    }

    /// `run_one_stage` constructs the stage config and refuses an unsupported
    /// task before `BenchmarkPipeline::execute` — no docker, no agent, but the
    /// stage dir + status staging are exercised.
    #[tokio::test]
    async fn test_run_one_stage_refuses_unsupported_task_before_pipeline() {
        use subdollarbench::campaign::{Acceptance, CampaignSpec, LoadedCampaign, StageSpec};

        let dir = tmp("run_one_stage_bad_task");
        let cdir = dir.join("campaign");
        let workdir = dir.join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();
        let lc = LoadedCampaign {
            spec: CampaignSpec {
                name: "fixture".to_string(),
                version: 1,
                task: "dns".to_string(),
                ladder: campaign::LadderMode::PassStop,
                title: "Fixture".to_string(),
                description: String::new(),
                stages: Vec::new(),
            },
            root: dir.clone(),
        };
        let stage = StageSpec {
            id: 0,
            kind: "build".to_string(),
            title: "Stage zero".to_string(),
            prompt: "p0.md".to_string(),
            acceptance: Acceptance {
                redis_stages: vec![1],
                proxy_stages: Vec::new(),
                queue_stages: Vec::new(),
                http_stages: Vec::new(),
                sql_stages: Vec::new(),
            },
            budget_usd: 0.1,
            max_turns: 3,
            feedback_rounds: 1,
            tier: 1,
            needs: Vec::new(),
            lite: None,
        };
        let job = StageJob {
            stage: stage.clone(),
            variant: campaign::StageVariant::Full,
            prompt_path: lc.prompt_path(&stage),
            budget_usd: stage.budget_usd,
            max_turns: stage.max_turns,
            rollback_first: false,
        };
        let last_green: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let err = run_one_stage(
            &lc,
            &stage,
            "campaign_fixture",
            "model",
            "low",
            Harness::Omp,
            None,
            5,
            &cdir,
            &workdir,
            true,
            None,
            &job,
            &last_green,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown task"), "{err:#}");
        // Staging happened before the refusal.
        assert!(cdir.join("s0_full").is_dir());
        assert!(cdir.join("status.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `run` fails on a missing campaign name in load_campaign — before the
    /// engine status probe, so no live engine is touched and nothing is
    /// written under the temp runs dir.
    #[tokio::test]
    async fn test_run_missing_campaign_fails_before_engine() {
        let env = crate::tests::SdbEnv::new("campaign_run_missing").await;
        let err = run(
            "no-such-campaign-fixture".to_string(),
            "test-model".to_string(),
            "low".to_string(),
            Harness::Omp,
            1.0,
            None,
            5,
            None,
            true,
            None,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("load campaign"), "{msg}");
        assert!(!env.root.join("runs").exists());
        drop(env);
    }
}
