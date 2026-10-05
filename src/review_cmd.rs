//! Review CLI handlers: `review-run` + `regen-reviews`.
//!
//! Pure move from `main.rs` (2026-10-05 split): zero behavior change.

use super::resolve_run_id;
use anyhow::{Context, Result};
use std::path::Path;
use subdollarbench::report::{PortalExporter, RunJudge, RunManifest};

/// `regen-reviews`: regenerate LLM code reviews + refresh the portal.
#[allow(clippy::too_many_arguments)]
pub async fn handle_regen_reviews(
    runs_dir: String,
    run_id: Option<String>,
    model: String,
    api_key: Option<String>,
    force: bool,
    results_dir: String,
    out: String,
    base_url: String,
    keep: usize,
    rollup_only: bool,
) -> Result<()> {
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
            std::path::Path::new("tasks")
                .join(&manifest.task)
                .join("prompt.md"),
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
                println!(
                    "[OK] {id}: {}+{} bullets",
                    review.good.len(),
                    review.bad.len()
                );
                done += 1;
            }
            Err(e) => {
                eprintln!("[FAIL] {id}: {e:#}");
                failed += 1;
            }
        }
    }
    let summary = if rollup_only {
        PortalExporter::export_rollup(&results_dir, &out, &base_url)?
    } else {
        PortalExporter::export(&runs_dir, &results_dir, &out, &base_url, keep)?
    };
    println!(
        "reviews: {done} regenerated, {skipped} skipped, {failed} failed; portal: {} {}",
        summary.run_count,
        if rollup_only {
            "models (rollup-only)"
        } else {
            "runs"
        },
    );
    Ok(())
}

/// `review-run`: deterministic stats + judge synthesis for one run.
pub async fn handle_review_run(
    run_id: String,
    runs_dir: String,
    model: String,
    api_key: Option<String>,
    force: bool,
) -> Result<()> {
    use subdollarbench::report::RunJudge;
    let id = resolve_run_id(&runs_dir, &run_id)?;
    let rdir = Path::new(&runs_dir).join(&id);
    let out_path = rdir.join("run-review.md");
    if out_path.exists() && !force {
        println!("run-review exists for {id} (use --force to regenerate)");
        return Ok(());
    }
    // Parent result: results/<id>.json preferred, else reconstruct
    // trial list from runs/<id>_trial* dirs.
    let results_path = subdollarbench::config::get_repo_root()
        .join("results")
        .join(format!("{id}.json"));
    type TrialRow = (String, f64, u32, u64, f64, f64);
    let (parent_model, parent_task, trials): (String, String, Vec<TrialRow>) =
        if let Ok(text) = std::fs::read_to_string(&results_path) {
            let v: serde_json::Value = serde_json::from_str(&text)?;
            let subs = v
                .get("subruns")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            let t = subs
                .iter()
                .map(|s| {
                    (
                        s.get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("?")
                            .to_string(),
                        s.get("pass_rate").and_then(|x| x.as_f64()).unwrap_or(0.0),
                        s.get("turns").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                        s.get("total_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                        s.get("total_cost_usd")
                            .and_then(|x| x.as_f64())
                            .unwrap_or(0.0),
                        s.get("duration_seconds")
                            .and_then(|x| x.as_f64())
                            .unwrap_or(0.0),
                    )
                })
                .collect();
            (
                v.get("model")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
                v.get("task")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
                t,
            )
        } else {
            anyhow::bail!("no results/{id}.json for {id}; run-review needs the merged parent");
        };
    // Per-trial review conclusions (best-effort: missing files skipped).
    // Plus deterministic failure reasons from trial manifests: failed
    // stage names + errors, so cross-trial failure claims are grounded
    // facts, not judge inference.
    let mut notes: Vec<(String, String)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (tid, _, _, _, _, _) in &trials {
        let mp = Path::new(&runs_dir).join(tid).join("manifest.json");
        let mut freasons = String::new();
        if let Ok(mtext) = std::fs::read_to_string(&mp) {
            if let Ok(mv) = serde_json::from_str::<serde_json::Value>(&mtext) {
                if let Some(stages) = mv.get("stages").and_then(|s| s.as_array()) {
                    let failed: Vec<String> = stages
                        .iter()
                        .filter(|s| s.get("passed") == Some(&serde_json::Value::Bool(false)))
                        .map(|s| {
                            let nm = s.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                            let er = s.get("error").and_then(|x| x.as_str()).unwrap_or("");
                            if er.is_empty() {
                                nm.to_string()
                            } else {
                                format!("{nm}: {er}")
                            }
                        })
                        .collect();
                    freasons = failed.join("; ");
                    if freasons.len() > 600 {
                        freasons.truncate(600);
                        freasons.push_str("...");
                    }
                }
            }
        }
        failures.push(freasons);
        let rp = Path::new(&runs_dir).join(tid).join("review.json");
        if let Ok(text) = std::fs::read_to_string(&rp) {
            if let Ok(rv) = serde_json::from_str::<serde_json::Value>(&text) {
                let c = rv
                    .get("conclusion")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !c.is_empty() {
                    notes.push((tid.clone(), c));
                }
            }
        }
    }
    let stats = RunJudge::run_stats_block_with_failures(
        &id,
        &parent_model,
        &parent_task,
        &trials,
        &failures,
    );
    print!("{}", stats);
    // Canonical best-trial workspace via the shared helper: the
    // parent row deserializes into the typed struct (subruns carry
    // full scores) so both callers converge on one rule. Fallback:
    // best tuple by pass rate when the parent file is missing.
    let best_id: String = {
        let rp2 = subdollarbench::config::get_repo_root()
            .join("results")
            .join(format!("{id}.json"));
        let from_parent: Option<String> = std::fs::read_to_string(&rp2)
            .ok()
            .and_then(|t| {
                serde_json::from_str::<subdollarbench::report::BenchmarkRunResult>(&t).ok()
            })
            .and_then(|parent| {
                subdollarbench::report::BenchmarkRunResult::canonical_best(&parent.subruns)
                    .map(|b| b.id.clone())
            });
        from_parent.unwrap_or_else(|| {
            trials
                .iter()
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|t| t.0.clone())
                .unwrap_or_default()
        })
    };
    let best_files =
        RunJudge::collect_workspace(&Path::new(&runs_dir).join(&best_id).join("workspace"));
    println!(
        "[REVIEW-RUN] canonical workspace: {best_id} ({} files)",
        best_files.len()
    );
    // Task prompt excerpt for grounding.
    let task_prompt = std::fs::read_to_string(
        subdollarbench::config::get_repo_root()
            .join("tasks")
            .join(&parent_task)
            .join("prompt.md"),
    )
    .unwrap_or_default();
    let prompt = RunJudge::build_synthesis_prompt(&stats, &task_prompt, &notes, &best_files);
    println!("[REVIEW-RUN] synthesizing with {model} ...");
    let review = RunJudge::judge_synthesis(api_key.as_deref(), &model, prompt).await?;
    RunJudge::validate(&review)?;
    let md = format!(
        "# Run review: {id}\n\nModel: {parent_model} task: {parent_task} judge: {model}\n\n## Statistics (deterministic)\n\n```\n{stats}\n```\n\n## Synthesis (LLM, advisory-only)\n\n### Strengths\n\n{}\n\n### Weaknesses\n\n{}\n\n### Conclusion\n\n{}\n",
        review.good.iter().map(|g| format!("- {g}\n")).collect::<String>(),
        review.bad.iter().map(|b| format!("- {b}\n")).collect::<String>(),
        review.conclusion,
    );
    std::fs::write(&out_path, &md)?;
    println!("wrote {}", out_path.display());
    Ok(())
}
