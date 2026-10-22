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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use subdollarbench::campaign::{
    self, CampaignRollup, CampaignRunOptions, CampaignStageConfig, LoadedCampaign, RoundRecord,
    StageOutcome, StageSpec,
};
use subdollarbench::config::{Harness, TaskType};
use subdollarbench::pipeline::{BenchmarkConfig, BenchmarkPipeline, PipelineLogger};
use subdollarbench::report::RunArchiver;

/// Tee logger: live output for the operator + a buffered stage console +
/// an append-only `live.log` the GUI tails while the campaign runs.
struct CampaignLogger {
    buf: Arc<Mutex<Vec<String>>>,
    live: std::path::PathBuf,
    /// The campaign dir: the live status block is written here (round-8
    /// improvement A).
    cdir: std::path::PathBuf,
    /// The live block, kept in memory and merged into status.json only
    /// when a write is due (round-9 finding 1: re-reading and re-parsing
    /// the whole rollup per log line was gigabytes of JSON per cell).
    live_state: Mutex<serde_json::Value>,
    /// When the live block was last written; the throttle keeps a chatty
    /// agent from rewriting status.json per line.
    last_status: Mutex<std::time::Instant>,
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
        // The live status block: update the in-memory copy per line, and
        // merge-write at most every 5 s plus immediately on a verdict or a
        // phase change. status.json (which carries the whole rollup) is
        // read only when a write is actually due (round-9 finding 1).
        let immediate = {
            let mut live = self.live_state.lock().unwrap_or_else(|e| e.into_inner());
            apply_live_line(&mut live, msg)
        };
        let due = {
            let mut last = self.last_status.lock().unwrap_or_else(|e| e.into_inner());
            let due = immediate || last.elapsed() >= std::time::Duration::from_secs(5);
            if due {
                *last = std::time::Instant::now();
            }
            due
        };
        if due {
            let snapshot = self
                .live_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            update_status(&self.cdir, |v| {
                // Merge, so fields the logger does not track (the variant,
                // model and budgets written at attempt setup) survive.
                if v["live"].is_null() {
                    v["live"] = serde_json::json!({});
                }
                if let Some(obj) = snapshot.as_object() {
                    for (k, val) in obj {
                        v["live"][k] = val.clone();
                    }
                }
            });
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
    update_status(cdir, |v| {
        v["state"] = serde_json::json!(state);
        v["current_stage"] = serde_json::json!(current_stage);
        if let Some(r) = rollup {
            v["rollup"] = serde_json::to_value(r).unwrap_or(serde_json::Value::Null);
            // Mirror the campaign spend into the live block (round-8
            // improvement A): the Run/Live views read one place.
            if v["live"].is_null() {
                v["live"] = serde_json::json!({});
            }
            v["live"]["campaign_spent"] = serde_json::json!(r.total_cost_usd);
            // R23 D5: the live status shows the remaining dollars.
            let cap = v["live"]["campaign_budget"].as_f64().unwrap_or(0.0);
            v["live"]["dollars_left"] = serde_json::json!((cap - r.total_cost_usd).max(0.0));
        }
    });
}

/// One process-wide lock for the status.json read-modify-write: the
/// campaign loop and the log-driven live updater both write it, and the
/// GUI polls it (round-8 improvement A).
static STATUS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Merge-update status.json: a writer never drops what another one wrote.
fn update_status(cdir: &Path, f: impl FnOnce(&mut serde_json::Value)) {
    let _guard = STATUS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = status_path(cdir);
    let mut v = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    f(&mut v);
    v["updated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    atomic_write_json(&path, &v);
}

/// Apply one log line to the live block; true when it is a verdict or a
/// phase change worth an immediate status write (round-8 improvement A).
/// Apply one log line to the in-memory live block; true when a status
/// write is due immediately (verdicts, phase changes). Timestamps are RFC
/// 3339: the console renders relative ages (round-9 findings 2 and 4).
fn apply_live_line(live: &mut serde_json::Value, msg: &str) -> bool {
    let t = msg.trim_start();
    let now = chrono::Utc::now().to_rfc3339();
    live["last_activity_at"] = serde_json::json!(now);
    // A fresh verification starts here: clear the check counters and the
    // first failure, so "last check" never shows a stale or skipped line.
    let reset_checks = |live: &mut serde_json::Value| {
        live["check_pass"] = serde_json::json!(0);
        live["check_total"] = serde_json::json!(0);
        live["summary"] = serde_json::Value::Null;
        live["first_fail"] = serde_json::Value::Null;
    };
    let num = |s: &str, key: &str| -> Option<f64> {
        s.split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .and_then(|v| v.parse::<f64>().ok())
    };
    if let Some(rest) = t.strip_prefix("[LIVE] Turn ") {
        if let Some((turn, max)) = rest.split_once('/') {
            let max = max.split_whitespace().next().unwrap_or("");
            live["turn"] = serde_json::json!(turn.trim().parse::<u64>().unwrap_or(0));
            live["max_turns"] = serde_json::json!(max.parse::<u64>().unwrap_or(0));
        }
        return false;
    }
    if let Some(rest) = t.strip_prefix("[TELEMETRY] ") {
        if let Some(v) = num(rest, "turn=") {
            live["turn"] = serde_json::json!(v as u64);
        }
        if let Some(v) = num(rest, "max_turns=") {
            live["max_turns"] = serde_json::json!(v as u64);
        }
        if let Some(v) = num(rest, "spent=") {
            // Provider-reported *API* spend, not the effective cost the
            // budgets bound (round-9 finding 3; the console labels it).
            live["ticket_spent"] = serde_json::json!(v);
        }
        return false;
    }
    if t.starts_with("─── ticket ") {
        let rest = t.trim_start_matches("─── ticket ");
        let (id_part, tail) = rest.split_once(" [").unwrap_or((rest, ""));
        live["ticket"] = serde_json::json!(id_part.trim().parse::<u64>().unwrap_or(0));
        if let Some((kind, tail)) = tail.split_once("] ") {
            live["type"] = serde_json::json!(kind);
            if let Some((title, rest)) = tail.split_once(" (acceptance") {
                live["title"] = serde_json::json!(title.trim());
                if let Some(budget) = rest.rsplit_once("budget $") {
                    // The line ends `budget $0.20) ───`: take the numeric
                    // prefix.
                    let digits: String = budget
                        .1
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.')
                        .collect();
                    if let Ok(v) = digits.parse::<f64>() {
                        live["ticket_budget"] = serde_json::json!(v);
                    }
                }
            }
        }
        live["round"] = serde_json::json!(0);
        live["ticket_spent"] = serde_json::json!(0.0);
        live["phase"] = serde_json::json!("agent");
        live["phase_started_at"] = serde_json::json!(now);
        reset_checks(live);
        return true;
    }
    if t.starts_with("[FEEDBACK] ━━━ Repair Round ") {
        let rest = t.trim_start_matches("[FEEDBACK] ━━━ Repair Round ");
        let round = rest.split('/').next().unwrap_or("1").trim();
        live["round"] = serde_json::json!(round.parse::<u64>().unwrap_or(1));
        live["phase"] = serde_json::json!("repair");
        live["phase_started_at"] = serde_json::json!(now);
        reset_checks(live);
        return true;
    }
    // "Initial attempt passed 5 of 6 stages" / "Round 1 test result: 2 of 6
    // stages passed": the pass/total summary (round-9 finding 2).
    if t.starts_with("[FEEDBACK] Initial attempt ") || t.starts_with("[FEEDBACK] Round ") {
        if let Some((a, b)) = t
            .split_once(" passed ")
            .or_else(|| t.split_once(" test result: "))
            .and_then(|(_, rest)| rest.split_once(" of "))
        {
            let pass = a.trim().trim_start_matches('(').trim();
            let total = b.split_whitespace().next().unwrap_or("");
            if let (Ok(_), Ok(_)) = (pass.parse::<u64>(), total.parse::<u64>()) {
                live["summary"] = serde_json::json!(format!("{pass}/{total}"));
            }
        }
        live["last_check_at"] = serde_json::json!(now);
        return true;
    }
    if t.starts_with("Stage ") && t.contains("-> ") {
        let total = live["check_total"].as_u64().unwrap_or(0) + 1;
        live["check_total"] = serde_json::json!(total);
        if t.contains("-> PASS") {
            live["check_pass"] = serde_json::json!(live["check_pass"].as_u64().unwrap_or(0) + 1);
        } else if !t.contains("skipped due to failure") && live["first_fail"].is_null() {
            // The first real failure of this verification, not the skipped
            // tail that follows it (round-9 finding 2). One line at most:
            // the repro that follows the first line would flood the card
            // (round-10 D5).
            live["first_fail"] = serde_json::json!(campaign::truncate_line(t, 160));
        }
        live["last_check_at"] = serde_json::json!(now);
        return true;
    }
    if t.starts_with("[VERIFY]") {
        reset_checks(live);
        live["phase"] = serde_json::json!("verifying");
        return false;
    }
    if t.starts_with("[SETUP]") || t.starts_with("[DIAGNOSTICS]") {
        live["phase"] = serde_json::json!("verifying");
        return false;
    }
    if t.starts_with("[PERF]") {
        live["phase"] = serde_json::json!("perf");
        return true;
    }
    if t.starts_with("[CRASH]") {
        live["phase"] = serde_json::json!("crash");
        return true;
    }
    if (t.starts_with("[THOUGHT]") || t.starts_with("[ACTION]") || t.starts_with("[RESPONSE]"))
        && live["phase"].as_str() != Some("agent")
    {
        live["phase"] = serde_json::json!("agent");
        live["phase_started_at"] = serde_json::json!(now);
        return true;
    }
    false
}

/// Mark the ticket as running without discarding the rollup written so far.
fn bump_current_stage(cdir: &Path, stage: u32) {
    update_status(cdir, |v| {
        v["state"] = serde_json::json!("running");
        v["current_stage"] = serde_json::json!(stage);
        if v.get("rollup").is_none() {
            v["rollup"] = serde_json::Value::Null;
        }
    });
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
    for s in &lc.spec.stages {
        println!(
            "  {} [{}] {} — acceptance {:?}, tier {}, max_turns {}, feedback {}",
            format!("s{}", s.id).bold(),
            s.kind,
            s.title,
            s.acceptance.stages_for(&lc.spec.task),
            s.tier,
            s.max_turns,
            s.feedback_rounds
        );
        println!("      prompt: {}", lc.prompt_path(s).display());
    }
    println!("budget: tier-weighted pool from the campaign cap at run time (fixed stage budgets are legacy)");
    println!(
        "workspace: runs/<campaign_id>/workspace (continuity across tickets, git-committed per stage)"
    );
    Ok(())
}

/// Append the P2 career hints below the harness note (which the pipeline
/// prepends to the prompt).
fn with_hints(preamble: Option<String>, hints: Option<&str>) -> Option<String> {
    match (preamble, hints) {
        (Some(p), Some(h)) => Some(format!("{p}\n\n{h}")),
        (None, Some(h)) => Some(h.to_string()),
        (p, None) => p,
    }
}

/// Persist the career ladder state at every phase boundary (P1).
fn write_ladder_state(cdir: &Path, state: &campaign::LadderState) {
    atomic_write_json(
        &cdir.join("ladder_state.json"),
        &serde_json::to_value(state).unwrap_or_default(),
    );
}

/// Copy a chained workspace including `.git` (the stage history must
/// continue); `copy_tree` skips symlinks and creates the destination.
fn copy_workspace_all(src: &Path, dst: &Path) -> Result<()> {
    subdollarbench::verifier::sql::crash::copy_tree(src, dst).map_err(|e| anyhow::anyhow!(e))?;
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
    phase: Option<u32>,
    chain: Option<String>,
    refork: Option<String>,
) -> Result<()> {
    let lc = campaign::load_campaign(&name).context("load campaign")?;
    // R23 D3: dollars are the denominator; a paid launch without a list
    // price has no dollar bound. Eval-only runs do not spend.
    campaign::require_model_list_price(&model, eval_only)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
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
    let phase_spec = match phase {
        Some(p) => Some(
            lc.spec
                .phases
                .iter()
                .find(|ph| ph.id == p)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("campaign '{}' has no phase {}", lc.spec.name, p))?,
        ),
        None => None,
    };
    match (phase, chain.as_deref()) {
        (None, Some(_)) => anyhow::bail!("--chain requires --phase"),
        (Some(1), Some(_)) => anyhow::bail!("phase 1 has no chain"),
        (Some(p), None) if p > 1 => {
            anyhow::bail!("phase {p} requires --chain <previous phase cell id>")
        }
        _ => {}
    }
    if refork.is_some() && chain.is_none() {
        anyhow::bail!("--refork requires --chain");
    }
    let refork_reason = refork.as_deref().map(str::trim).unwrap_or("");
    if refork.is_some() && refork_reason.is_empty() {
        anyhow::bail!("--refork needs a non-empty reason");
    }
    let runs_dir = RunArchiver::resolve_runs_dir();
    let mut refork_info: Option<campaign::ReforkInfo> = None;

    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let model_tag = model.replace(['/', ':'], "_");
    let campaign_id = format!("campaign_{}_{}_{}", lc.spec.name, model_tag, ts);
    // One campaign at a time, CLI or GUI: the lock covers every path and
    // is released when this process ends (round-7 finding 1).
    let _campaign_lock = campaign::CampaignLock::acquire(&campaign_id)
        .map_err(|e| anyhow::anyhow!("{e} — stop it first or wait for it to finish"))?;

    let (chain_state, chain_career) = if let Some(chain_id) = chain.as_deref() {
        if chain_id.contains('/') || chain_id.contains("..") {
            anyhow::bail!("invalid --chain id {chain_id:?}");
        }
        // R1 (D2): any existing successor uses up the chain, including
        // running, cancelled and crashed cells; a harness-faulted cell is a
        // non-result and does not. The campaign lock is held here.
        let successor = campaign::chain_already_used(&runs_dir, chain_id);
        refork_info =
            campaign::resolve_refork(successor.as_deref(), Some(chain_id), refork.as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;
        let chain_dir = runs_dir.join(chain_id);
        let status: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(chain_dir.join("status.json"))
                .with_context(|| format!("read chain cell {}", chain_dir.display()))?,
        )
        .with_context(|| format!("parse chain cell {}", chain_dir.display()))?;
        let rollup: campaign::CampaignRollup = serde_json::from_value(
            status
                .get("rollup")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("chain cell {chain_id} has no rollup"))?,
        )
        .with_context(|| format!("parse chain rollup {chain_id}"))?;
        let state = std::fs::read_to_string(chain_dir.join("ladder_state.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<campaign::LadderState>(&t).ok());
        let phase = phase.expect("a chain implies a phase");
        let mut state = campaign::validate_chain(
            &rollup,
            state.as_ref(),
            phase,
            &model,
            &harness.to_string(),
            &effort,
            subdollarbench::report::rules::RULES_VERSION,
        )?;
        if let Some(ps) = &phase_spec {
            state.hints = campaign::phase_hints(&rollup, &lc.spec, ps, &state);
        }
        // Q7: carry the predecessor's failure records forward; the hints
        // for a later phase must not depend on the immediate cell still
        // holding the ticket.
        for rec in &rollup.stages {
            if let Some(f) = &rec.failure {
                state.last_failures.insert(rec.id, f.clone());
            }
        }
        let career = campaign::career_from(&rollup);
        (Some(state), Some(career))
    } else {
        (None, None)
    };
    let cdir = runs_dir.join(&campaign_id);
    std::fs::create_dir_all(&cdir)?;
    // The campaign-level live fields (round-8 improvement A): every later
    // status write merges into this document.
    update_status(&cdir, |v| {
        let started = v["started_at"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        v["started_at"] = serde_json::json!(started);
        v["wall_cap_min"] = serde_json::json!(wall_min.unwrap_or(0));
        v["live"] = serde_json::json!({
            "campaign": lc.spec.name,
            "model": model,
            "harness": harness.to_string(),
            "effort": effort,
            "campaign_budget": budget_usd,
            "campaign_spent": 0.0,
            "dollars_left": budget_usd,
            "career_phase": phase,
            "chain_from": chain,
            "refork": refork_info,
        });
        v["career_phase"] = serde_json::json!(phase);
        v["chain_from"] = serde_json::json!(chain);
        v["refork"] = serde_json::json!(refork_info);
    });
    let workdir = cdir.join("workspace");

    if let Some(chain_id) = chain.as_deref() {
        if workspace.is_some() {
            anyhow::bail!("--workspace cannot be combined with --chain");
        }
        let chain_dir = runs_dir.join(chain_id);
        let src_ws = chain_dir.join("workspace");
        if !src_ws.is_dir() {
            anyhow::bail!("chain cell {chain_id} has no workspace");
        }
        copy_workspace_all(&src_ws, &workdir)?;
        for name in ["sql-data", "sql-data.snap", "sql-sentinels.snap"] {
            let src = chain_dir.join(name);
            if src.is_dir() {
                copy_store(&src, &cdir.join(name))?;
            }
        }
        if chain_dir.join("baseline.json").is_file() {
            std::fs::copy(chain_dir.join("baseline.json"), cdir.join("baseline.json"))?;
        }
        for f in sentinel_files(&chain_dir) {
            if let Some(name) = f.file_name() {
                let _ = std::fs::copy(&f, cdir.join(name));
            }
        }
        println!(
            "[CONTINUE] phase {} from {} (workspace, store, sentinels, baseline)",
            phase.unwrap_or(0),
            chain_id
        );
    }
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
            "=== CAMPAIGN {} · {} · {} · cap ${:.2}{} ===",
            lc.spec.name,
            model,
            harness,
            budget_usd,
            phase.map(|p| format!(" · phase {p}")).unwrap_or_default()
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
    if let Some(state) = &chain_state {
        *last_green.lock().unwrap() = state.last_green.clone();
    }
    let initial_state = chain_state.clone().unwrap_or_default();
    write_ladder_state(&cdir, &initial_state);
    let state_cdir = cdir.clone();
    let state_green = last_green.clone();
    let state_failures = Arc::new(Mutex::new(
        chain_state
            .as_ref()
            .map(|s| s.last_failures.clone())
            .unwrap_or_default(),
    ));
    let on_stage = {
        let failures = state_failures.clone();
        move |r: &CampaignRollup| {
            write_status(&state_cdir, "running", None, Some(r));
            if let Some(mut st) = r.ladder_state.clone() {
                st.last_green = state_green.lock().map(|g| g.clone()).unwrap_or(None);
                let mut lf = failures.lock().unwrap_or_else(|e| e.into_inner());
                for rec in &r.stages {
                    if let Some(f) = &rec.failure {
                        lf.insert(rec.id, f.clone());
                    }
                }
                st.last_failures = lf.clone();
                write_ladder_state(&state_cdir, &st);
            }
        }
    };
    let rollup = campaign::run_campaign_phase(
        &lc,
        &opts,
        phase_spec.as_ref(),
        chain_state.as_ref(),
        chain.as_deref(),
        chain_career.as_ref(),
        |attempt| {
            let job = StageJob {
                stage: attempt.stage.clone(),
                variant: attempt.variant,
                prompt_path: attempt.prompt_path.clone(),
                budget_usd: attempt.budget_usd,
                max_turns: attempt.max_turns,
                lite_profile: attempt.lite_profile.clone(),
                passed_lite: attempt.passed_lite.clone(),
                passed: attempt.passed.clone(),
                outcomes: attempt.outcomes.clone(),
                cap_headroom_usd: attempt.cap_headroom_usd,
                hints: attempt.hints.clone(),
                catch_up: attempt.catch_up,
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
    .await;
    let mut rollup = match rollup {
        Ok(r) => r,
        Err(e) => {
            // Round-11 O2: a cancel (SIGINT/SIGTERM sets the flag) leaves an
            // honest state instead of reading as a crashed cell. A dead
            // writer — a crash or kill -9 — cannot write anything and reads
            // as stalled, which is what "stalled" means.
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                write_status(&cdir, "cancelled", None, None);
                update_status(&cdir, |v| {
                    v["stop_reason"] = serde_json::json!("on request");
                });
            }
            return Err(e);
        }
    };
    {
        let mut lf = state_failures.lock().unwrap_or_else(|e| e.into_inner());
        for rec in &rollup.stages {
            if let Some(f) = &rec.failure {
                lf.insert(rec.id, f.clone());
            }
        }
    }
    if let Some(st) = rollup.ladder_state.as_mut() {
        st.last_green = last_green.lock().map(|g| g.clone()).unwrap_or(None);
        st.last_failures = state_failures.lock().map(|l| l.clone()).unwrap_or_default();
        write_ladder_state(&cdir, st);
    }

    rollup.refork = refork_info.clone();
    write_status(&cdir, "done", None, Some(&rollup));
    let rollup_path = cdir.join("campaign.json");
    std::fs::write(&rollup_path, serde_json::to_string_pretty(&rollup)?)?;
    print_rollup(&rollup);
    println!("rollup: {}", rollup_path.display());
    if let Some(reason) = &rollup.harness_fault {
        // Round-14 N2: the cell stopped on a harness fault, so it measured
        // the harness, not the model. Poison it before the board rebuild:
        // the portal and the console both skip a poisoned campaign.
        let note = format!(
            "poisoned {} by the campaign runner\nreason: harness fault: {reason}\n",
            chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
        );
        let _ = std::fs::write(
            cdir.join(subdollarbench::report::portal::POISONED_FILE),
            note,
        );
        eprintln!("[campaign] HARNESS FAULT: {reason}; the cell is poisoned and not published");
    }
    // The public sdb-sql board reads these rollups: rebuild it now so a
    // finished cell is never served as a mid-run snapshot (the engine
    // rebuilds only on demand; a standalone `campaign run` writes the file
    // directly).
    let runs = subdollarbench::report::archive::RunArchiver::resolve_runs_dir();
    let portal = subdollarbench::report::archive::RunArchiver::resolve_portal_dir();
    let _ = subdollarbench::report::portal::PortalExporter::export_sql_board(
        runs.to_str().unwrap_or("./runs"),
        portal.to_str().unwrap_or("./portal"),
    );
    Ok(())
}

/// One attempt handed from the orchestrator to this executor: the ticket
/// (full or lite), its prompt/budget/turns, and whether the workspace must
/// roll back to the last green commit first.
struct StageJob {
    stage: StageSpec,
    variant: campaign::StageVariant,
    prompt_path: std::path::PathBuf,
    budget_usd: f64,
    max_turns: u32,
    /// Feature flags of the lite variant (`LiteSpec.profile`), empty for a
    /// full attempt.
    lite_profile: Vec<String>,
    /// Lite profile of earlier stages this ladder passed on a lite variant.
    passed_lite: Vec<(u32, Vec<String>)>,
    /// Every stage passed so far (full or lite); the declared acceptance
    /// list is intersected with it plus the current ticket.
    passed: Vec<u32>,
    /// Ladder state before this attempt (round-10 D2): drives the collapsed
    /// SPEC.md sections and the harness note.
    outcomes: Vec<(u32, campaign::LadderOutcome)>,
    cap_headroom_usd: Option<f64>,
    hints: Option<String>,
    catch_up: bool,
    rollback_first: bool,
}

/// Copy a candidate-owned store tree: regular files and directories only.
/// Symlinks are never followed (a link to a host file must not leak into a
/// snapshot) and FIFOs/devices are skipped instead of blocking the copy.
fn copy_store(src: &Path, dst: &Path) -> Result<()> {
    subdollarbench::verifier::sql::crash::copy_tree(src, dst).map_err(|e| anyhow::anyhow!(e))?;
    Ok(())
}

/// Every `sentinel-<stage>.json` directly under `dir`.
fn sentinel_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("sentinel-") && n.ends_with(".json")
                })
                .unwrap_or(false)
        })
        .collect()
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
    // sdb-sql: the harness-owned data directory rolls back with the
    // workspace ("A rollback restores the data directory snapshot along
    // with the code"). Snapshot the store this attempt starts from; a later
    // rollback restores it.
    if lc.spec.task == "sql" {
        let data = cdir.join("sql-data");
        let snap = cdir.join("sql-data.snap");
        let sent_snap = cdir.join("sql-sentinels.snap");
        if job.rollback_first && snap.is_dir() {
            let _ = std::fs::remove_dir_all(&data);
            match copy_store(&snap, &data) {
                Ok(()) => println!("  [ROLLBACK] sdb-sql store restored from its snapshot"),
                Err(e) => println!("  [WARN] could not restore the sdb-sql store snapshot: {e}"),
            }
            // The sentinel chain describes the state this attempt is about
            // to start from: restoring the store without it would leave
            // later tickets checking reads against data the rollback
            // removed.
            for f in sentinel_files(cdir) {
                let _ = std::fs::remove_file(&f);
            }
            for f in sentinel_files(&sent_snap) {
                if let Some(name) = f.file_name() {
                    let _ = std::fs::copy(&f, cdir.join(name));
                }
            }
            let _ = std::fs::remove_dir_all(&sent_snap);
        }
        if data.is_dir() {
            let _ = std::fs::remove_dir_all(&snap);
            if let Err(e) = copy_store(&data, &snap) {
                // A store the harness cannot even copy costs the rollback
                // protection, not the campaign: never abort here.
                println!("  [WARN] could not snapshot the sdb-sql store: {e}");
            }
            let _ = std::fs::remove_dir_all(&sent_snap);
            let _ = std::fs::create_dir_all(&sent_snap);
            for f in sentinel_files(cdir) {
                if let Some(name) = f.file_name() {
                    let _ = std::fs::copy(&f, sent_snap.join(name));
                }
            }
        }
    }
    // Round-10 D1: the per-round feedback report is harness-owned. Remove
    // any copy (this ticket's or an earlier ticket's, possibly resurrected
    // by a rollback) before the agent's first turn so a stale report can
    // never be read as the current truth; the copy written during this
    // attempt's repair rounds is removed again before the commit.
    let feedback_report = workdir.join("FEEDBACK_REPORT.md");
    let _ = std::fs::remove_file(&feedback_report);
    // The suite that actually grades this ticket (round-10 D2): the same
    // list drives the verifier, the log header, and the harness note.
    let mut effective = campaign::Acceptance::accepted_for(
        stage.acceptance.stages_for(&lc.spec.task),
        &job.passed,
        stage.id,
    );
    if job.catch_up {
        // Q4: a catch-up grades the target's declared list plus every
        // ticket the career already passed (7-10 when retrying 4).
        effective = campaign::catch_up_accepted(effective, &job.passed);
    }
    // sdb-sql: the harness-owned contract so far. The pipeline writes it
    // once the workspace is prepared (genesis wipes it first).
    let spec_md = if lc.spec.task == "sql" {
        if job.catch_up {
            Some(lc.render_spec_md_catch_up(stage, &job.prompt_path, &job.outcomes, &job.passed)?)
        } else {
            Some(lc.render_spec_md(stage, &job.prompt_path, &job.outcomes)?)
        }
    } else {
        None
    };
    let preamble = if lc.spec.task == "sql" {
        campaign::attempt_preamble(
            &lc.spec.stages,
            stage,
            job.variant,
            job.rollback_first,
            &job.outcomes,
            &effective,
        )
    } else {
        None
    };
    // P2: the phase-start career hints ride on every attempt's preamble.
    let preamble = with_hints(preamble, job.hints.as_deref());
    let run_id = format!("{campaign_id}_s{}_{variant_tag}", stage.id);
    let stage_dir = cdir.join(format!("s{}_{variant_tag}", stage.id));
    std::fs::create_dir_all(&stage_dir)?;
    bump_current_stage(cdir, stage.id);
    // The attempt's live block: what the Board, Run and Live views show
    // while this ticket is in flight (round-8 improvement A).
    let now = chrono::Utc::now().to_rfc3339();
    update_status(cdir, |v| {
        let campaign = v["live"]["campaign"].clone();
        let model_l = v["live"]["model"].clone();
        let harness_l = v["live"]["harness"].clone();
        let effort_l = v["live"]["effort"].clone();
        let budget_l = v["live"]["campaign_budget"].clone();
        let spent_l = v["live"]["campaign_spent"].as_f64().unwrap_or(0.0);
        let dollars_left = (budget_l.as_f64().unwrap_or(0.0) - spent_l).max(0.0);
        v["live"] = serde_json::json!({
            "campaign": campaign,
            "model": model_l,
            "harness": harness_l,
            "effort": effort_l,
            "campaign_budget": budget_l,
            "campaign_spent": v["live"]["campaign_spent"].clone(),
            "dollars_left": dollars_left,
            "ticket": stage.id,
            "title": stage.title,
            "type": stage.kind,
            "variant": variant_tag,
            "round": 0,
            "phase": "agent",
            "phase_started_at": now,
            "turn": 0,
            "max_turns": job.max_turns,
            "ticket_spent": 0.0,
            "ticket_budget": job.budget_usd,
            "last_activity_at": now,
        });
    });

    let buf = Arc::new(Mutex::new(Vec::<String>::new()));
    let logger: Arc<dyn PipelineLogger> = Arc::new(CampaignLogger {
        buf: buf.clone(),
        live: cdir.join("live.log"),
        cdir: cdir.to_path_buf(),
        live_state: Mutex::new(serde_json::json!({})),
        last_status: Mutex::new(std::time::Instant::now()),
    });
    logger.log(&format!(
        "─── ticket {} [{}] {} (acceptance {:?} effective, declared {:?}, budget ${:.2}) ───",
        stage.id,
        stage.kind,
        stage.title,
        effective,
        stage.acceptance.stages_for(&lc.spec.task),
        job.budget_usd
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
            // The suite at stage k is the *passed* stages' suites plus k
            // itself: a stage the model failed or skipped contributes
            // nothing, so one failure cannot sink every later ticket.
            accepted_stages: effective.clone(),
            lite_profile: job.lite_profile.clone(),
            passed_lite: job.passed_lite.clone(),
            passed_stages: job.passed.clone(),
            spec_md,
            preamble,
            cap_headroom_usd: job.cap_headroom_usd,
        }),
        baseline_rps,
    };

    let output = BenchmarkPipeline::execute(config, logger, cancel).await?;
    // D1: the report served its repair rounds; it must not enter the commit.
    let _ = std::fs::remove_file(&feedback_report);

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
    std::fs::write(stage_dir.join("console.log"), &console)?;
    if let Ok(m) = serde_json::to_string_pretty(&output.manifest) {
        let _ = std::fs::write(stage_dir.join("manifest.json"), m);
    }
    let passed = output.benchmark_result.pass_rate >= 100.0 && !output.disqualified;
    // F2: an agent that exited after the round was graded is a candidate
    // failure, not a harness fault. Only a real harness fault / infra error
    // measured nothing.
    let verdict = if passed {
        "pass"
    } else if output.infra_error || output.harness_fault.is_some() {
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
    // Campaign rounds and the classified cause (gui-spec.md §5.1/§5.2):
    // recorded from the manifest's per-round data, or derived for attempts
    // that predate it (eval-only runs, old binaries) so old cells render.
    let mut rounds: Vec<RoundRecord> = if output.manifest.rounds.is_empty() {
        vec![RoundRecord {
            round: 0,
            passed_stages: br.passed_stages,
            total_stages: br.total_stages,
            turns: output.manifest.turns,
            cost_usd: br.effective_cost_usd.unwrap_or(br.total_cost_usd),
            duration_seconds: Some(output.manifest.duration_seconds),
            git_commit: git_commit.clone(),
            failure: None,
            derived: true,
        }]
    } else {
        let last = output.manifest.rounds.last().map(|r| r.round).unwrap_or(0);
        output
            .manifest
            .rounds
            .iter()
            .map(|r| RoundRecord {
                round: r.round,
                passed_stages: r.passed_stages,
                total_stages: r.total_stages,
                turns: r.turns,
                cost_usd: r.cost_usd,
                duration_seconds: r.duration_seconds,
                git_commit: (r.round == last).then(|| git_commit.clone()).flatten(),
                failure: None,
                derived: false,
            })
            .collect()
    };
    for (i, round) in rounds.iter_mut().enumerate() {
        let (stages, tail): (Vec<subdollarbench::verifier::StageResult>, Option<&str>) =
            if output.manifest.rounds.is_empty() {
                (output.manifest.stages.clone(), None)
            } else {
                (
                    output.manifest.rounds[i].stage_results.clone(),
                    output.manifest.rounds[i].candidate_logs_tail.as_deref(),
                )
            };
        round.failure = campaign::classify_failure(
            stage.id,
            &job.passed,
            &stages,
            output.agent_failed,
            output.infra_error || output.harness_fault.is_some(),
            tail,
        );
    }
    // F2: the manager-era crash shape is an output-limit truncation
    // (Haiku hit 65,536 completion tokens, an unterminated edit JSON, then a
    // provider 400). Keep it in the stage record so the Board can say why.
    if output.agent_failed && output.harness_fault.is_none() {
        if let Some(f) = rounds.last_mut().and_then(|r| r.failure.as_mut()) {
            campaign::mark_agent_crash(f, &console);
        }
    }
    let failure = rounds.last().and_then(|r| r.failure.clone());
    Ok(StageOutcome {
        run_id,
        passed,
        pass_rate: br.pass_rate,
        passed_stages: br.passed_stages,
        total_stages: br.total_stages,
        stage_results: output.manifest.stages.clone(),
        effective_cost_usd: br.effective_cost_usd.unwrap_or(br.total_cost_usd),
        // The provider-billed share: the cap's headroom converts into this
        // unit before it becomes an attempt budget (round-11 finding 1).
        api_cost_usd: br.total_cost_usd,
        turns: output.manifest.turns,
        duration_seconds: Some(output.manifest.duration_seconds),
        max_call_usd: output.max_call_usd,
        stop_reason: output.stop_reason.clone(),
        agent_failed: output.agent_failed,
        infra_error: output.infra_error,
        disqualified: output.disqualified,
        git_commit,
        git_stat,
        rounds,
        failure,
        harness_fault: output.harness_fault.clone(),
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
            stop_reason: None,
            variant: "full".to_string(),
            tier: 1,
            points: if passed { 1.0 } else { 0.0 },
            run_id: format!("campaign_fixture_s{id}"),
            git_commit: None,
            git_stat: None,
            rounds: Vec::new(),
            failure: None,
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
            rules_version: subdollarbench::report::rules::RULES_VERSION.to_string(),
            points: 0.0,
            max_points: 0.0,
            phase: None,
            chain_from: None,
            career: None,
            ladder_state: None,
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
            cap_reached: false,
            budget_usd: 0.0,
            wall_min: 0,
            harness_fault: None,
            refork: None,
        }
    }

    /// The tee logger feeds the stage console buffer and the live tail file.
    /// Round-9 findings 1-3: the in-memory live block tracks the current
    /// verification — the first *real* failure, the running counters, the
    /// pass/total summary — and resets at the next [VERIFY].
    #[test]
    fn apply_live_line_tracks_the_current_check() {
        let mut live = serde_json::json!({});
        let lines = [
            "─── ticket 6 [storage] Transactions (acceptance [0, 1, 3], budget $0.20) ───",
            "[TELEMETRY] turn=3 max_turns=30 spent=0.012 tokens=1000",
            "[VERIFY] Checking anti-cheat compliance",
            "  Stage 0: Genesis -> PASS",
            "  Stage 1: Expressions -> FAIL Kind: expected OK 0, got ERR syntax",
            "  Stage 3: UPDATE -> FAIL skipped due to failure in earlier stages",
            "  Stage 4: Persistence -> FAIL skipped due to failure in earlier stages",
            "[FEEDBACK] Initial attempt passed 2 of 3 stages (66.7%). Starting interactive repair loop (max 1 rounds)...",
        ];
        for l in lines {
            apply_live_line(&mut live, l);
        }
        assert_eq!(live["ticket"], 6);
        assert_eq!(live["type"], "storage");
        assert_eq!(live["ticket_budget"], 0.2);
        assert_eq!(live["ticket_spent"], 0.012);
        assert_eq!(live["turn"], 3);
        assert_eq!(live["check_total"], 4);
        assert_eq!(live["check_pass"], 1);
        assert_eq!(live["summary"], "2/3");
        let first = live["first_fail"].as_str().unwrap_or_default();
        assert!(
            first.contains("Stage 1"),
            "first real failure, not skipped: {first}"
        );
        // A new verification resets the counters and the failure.
        apply_live_line(&mut live, "[VERIFY] Checking anti-cheat compliance");
        assert_eq!(live["check_total"], 0);
        assert_eq!(live["check_pass"], 0);
        assert!(live["first_fail"].is_null());
        assert!(live["summary"].is_null());
        // Round-10 D5: a *fresh* verification's first failure is one line,
        // capped at 160 chars; the multi-line repro that follows the FAIL
        // line would otherwise flood the header card.
        apply_live_line(&mut live, "[VERIFY] Checking anti-cheat compliance");
        let long = format!(
            "  Stage 11: Indexes -> FAIL Kind: expected OK 0, got ERR syntax\nMinimal reproduction. Start from an empty database and send: {}\n",
            "CREATE TABLE t (id INTEGER); ".repeat(40)
        );
        apply_live_line(&mut live, &long);
        let f = live["first_fail"].as_str().unwrap_or_default();
        assert!(!f.contains('\n'), "one line: {f}");
        assert!(f.chars().count() <= 160, "len {}", f.chars().count());
        assert!(f.contains("Stage 11"), "{f}");
        // Timestamps are RFC 3339 so the console can show relative ages.
        assert!(live["last_activity_at"]
            .as_str()
            .unwrap_or("")
            .contains('T'));
    }

    #[test]
    fn test_campaign_logger_buffer_and_live_log() {
        let dir = tmp("logger");
        let buf = Arc::new(Mutex::new(Vec::<String>::new()));
        let logger = CampaignLogger {
            buf: buf.clone(),
            live: dir.join("live.log"),
            cdir: dir.clone(),
            live_state: Mutex::new(serde_json::json!({})),
            last_status: Mutex::new(std::time::Instant::now()),
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
                phases: Vec::new(),
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
            lite_profile: Vec::new(),
            passed_lite: Vec::new(),
            passed: Vec::new(),
            outcomes: Vec::new(),
            cap_headroom_usd: None,
            hints: None,
            catch_up: false,
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

    #[test]
    fn hint_preamble_appends_below_the_harness_note() {
        assert_eq!(
            with_hints(Some("note".into()), Some("hint")),
            Some("note\n\nhint".to_string())
        );
        assert_eq!(with_hints(None, Some("hint")), Some("hint".to_string()));
        assert_eq!(
            with_hints(Some("note".into()), None),
            Some("note".to_string())
        );
        assert_eq!(with_hints(None, None), None);
    }

    #[test]
    fn r1_a_cancelled_or_status_only_successor_blocks_the_chain() {
        let dir = tmp("r1_scan");
        let runs = dir.join("runs");
        let cancelled = runs.join("campaign_cancelled");
        std::fs::create_dir_all(&cancelled).unwrap();
        std::fs::write(
            cancelled.join("status.json"),
            serde_json::json!({
                "state": "cancelled",
                "chain_from": "campaign_root",
                "finished_at": ""
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            campaign::chain_already_used(&runs, "campaign_root").as_deref(),
            Some("campaign_cancelled")
        );

        // A successor that only reached status.json blocks too.
        let running = runs.join("campaign_running");
        std::fs::create_dir_all(&running).unwrap();
        std::fs::write(
            running.join("status.json"),
            serde_json::json!({"state": "running", "live": {"chain_from": "campaign_root"}})
                .to_string(),
        )
        .unwrap();
        assert!(campaign::chain_already_used(&runs, "campaign_root").is_some());

        // A harness-faulted successor alone is a non-result and does not.
        let fdir = tmp("r1_fault");
        let fruns = fdir.join("runs");
        let faulted = fruns.join("campaign_faulted");
        std::fs::create_dir_all(&faulted).unwrap();
        std::fs::write(
            faulted.join("campaign.json"),
            serde_json::json!({
                "chain_from": "campaign_root",
                "harness_fault": "provider billing"
            })
            .to_string(),
        )
        .unwrap();
        assert!(campaign::chain_already_used(&fruns, "campaign_root").is_none());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fdir);
    }

    #[test]
    fn q6_a_chain_cell_cannot_be_continued_twice() {
        let dir = tmp("chain_reuse");
        let runs = dir.join("runs");
        let root = runs.join("campaign_root");
        let child = runs.join("campaign_child");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            child.join("campaign.json"),
            serde_json::json!({
                "chain_from": "campaign_root",
                "finished_at": "2026-10-21T12:00:00Z",
                "harness_fault": null
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            campaign::chain_already_used(&runs, "campaign_root").as_deref(),
            Some("campaign_child")
        );
        // An unfinished or faulted cell does not claim the chain.
        let faulted = runs.join("campaign_faulted");
        std::fs::create_dir_all(&faulted).unwrap();
        std::fs::write(
            faulted.join("campaign.json"),
            serde_json::json!({
                "chain_from": "campaign_root",
                "finished_at": "2026-10-21T12:00:00Z",
                "harness_fault": "provider billing"
            })
            .to_string(),
        )
        .unwrap();
        assert!(campaign::chain_already_used(&runs, "campaign_root").is_some());
        // A different parent is not a fork of this chain.
        assert!(campaign::chain_already_used(&runs, "campaign_other").is_none());
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
            None,
            None,
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
