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
    // Round-12 P1: both paths publish the sdb-sql board; the legacy page is
    // retired (its flags stay accepted for CLI compatibility).
    let _ = (&results_dir, &base_url, keep, rollup_only);
    let summary = PortalExporter::export_sql_board(&runs_dir, &out)?;
    println!(
        "reviews: {done} regenerated, {skipped} skipped, {failed} failed; portal: {} cells",
        summary.run_count,
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

#[cfg(test)]
mod tests {
    use super::{handle_regen_reviews, handle_review_run};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sdb_review_cmd_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_json(path: &Path, value: &serde_json::Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    /// Full RunManifest shape (`handle_regen_reviews` deserializes it typed).
    fn manifest_json(run_id: &str, task: &str) -> serde_json::Value {
        json!({
            "run_id": run_id,
            "model": "fixture-model",
            "task": task,
            "status": "completed",
            "language": "Rust",
            "started_at": "2026-10-01T00:00:00Z",
            "completed_at": "2026-10-01T00:01:00Z",
            "duration_seconds": 61.0,
            "pass_rate": 100.0,
            "passed_stages": 4,
            "total_stages": 4,
            "stages": [],
            "throughput_req_sec": 900.0,
            "tokens": {
                "prompt_tokens": 10,
                "cached_tokens": 0,
                "completion_tokens": 5,
                "total_tokens": 15
            },
            "cost_usd": 0.01,
            "savings_percent": 0.0,
            "efficiency_score": 50.0,
            "files": []
        })
    }

    /// Trial row that also deserializes as a `BenchmarkRunResult`.
    fn subrun_json(id: &str, task: &str, pass_rate: f64) -> serde_json::Value {
        json!({
            "id": id,
            "model": "fixture-model",
            "task": task,
            "language": "Rust",
            "effort": "low",
            "pass_rate": pass_rate,
            "passed_stages": 2,
            "total_stages": 4,
            "throughput_req_sec": 900.0,
            "prompt_tokens": 5,
            "cached_tokens": 0,
            "completion_tokens": 1,
            "total_tokens": 6,
            "total_cost_usd": 0.1,
            "savings_percent": 5.0,
            "efficiency_score": 40.0,
            "timestamp": "2026-10-01T00:00:00Z",
            "method_version": "0.2.0",
            "turns": 7,
            "duration_seconds": 42.0
        })
    }

    fn parent_result_json(
        id: &str,
        task: &str,
        subruns: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        json!({
            "id": id,
            "model": "fixture-model",
            "task": task,
            "language": "Rust",
            "effort": "low",
            "pass_rate": 50.0,
            "passed_stages": 2,
            "total_stages": 4,
            "throughput_req_sec": 900.0,
            "prompt_tokens": 10,
            "cached_tokens": 1,
            "completion_tokens": 2,
            "total_tokens": 13,
            "total_cost_usd": 0.25,
            "savings_percent": 5.0,
            "efficiency_score": 40.0,
            "timestamp": "2026-10-01T00:00:00Z",
            "method_version": "0.2.0",
            "subruns": subruns
        })
    }

    #[tokio::test]
    async fn test_review_run_unknown_id_errors() {
        let runs = temp_dir("runs_empty");
        let err = handle_review_run(
            "missing".to_string(),
            runs.to_string_lossy().to_string(),
            "openrouter/judge:batch".to_string(),
            None,
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("no run matches prefix"), "{err}");
        let _ = std::fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_review_run_existing_review_short_circuits_without_force() {
        let runs = temp_dir("runs_short");
        let id = "test_review_short";
        let dir = runs.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run-review.md"), "KEEP\n").unwrap();
        handle_review_run(
            id.to_string(),
            runs.to_string_lossy().to_string(),
            "openrouter/judge:batch".to_string(),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("run-review.md")).unwrap(),
            "KEEP\n"
        );
        let _ = std::fs::remove_dir_all(&runs);
    }

    #[tokio::test]
    async fn test_review_run_without_parent_result_errors() {
        let runs = temp_dir("runs_noparent");
        let id = format!("test_review_noparent_{}", std::process::id());
        std::fs::create_dir_all(runs.join(&id)).unwrap();
        let err = handle_review_run(
            id.clone(),
            runs.to_string_lossy().to_string(),
            "openrouter/judge:batch".to_string(),
            None,
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs the merged parent"), "{err}");
        let _ = std::fs::remove_dir_all(&runs);
    }

    /// Drives the deterministic half of `review-run` end to end: parent
    /// result -> trial stats + failure reasons + trial conclusions ->
    /// canonical best workspace -> synthesis prompt. The judge call itself
    /// is forced onto a batch model so it bails before any HTTP.
    ///
    /// The run id is an absolute path: `Path::join` replaces the base for
    /// absolute components, so every lookup (`results/<id>.json`, trial
    /// manifests/reviews, best workspace) resolves inside the temp root
    /// without touching process-global state (env vars).
    #[tokio::test]
    async fn test_review_run_computes_stats_and_bails_before_network() {
        let root = temp_dir("runs_full");
        let id = root.join("run").to_string_lossy().to_string();
        let task = "redis";
        let trial1 = root.join("run_trial1").to_string_lossy().to_string();
        let trial2 = root.join("run_trial2").to_string_lossy().to_string();

        std::fs::create_dir_all(&id).unwrap();
        std::fs::write(Path::new(&id).join("run-review.md"), "STALE\n").unwrap();
        write_json(
            &Path::new(&trial1).join("manifest.json"),
            &json!({
                "stages": [
                    {"stage": 1, "name": "boot", "passed": true, "error": null},
                    {"stage": 2, "name": "persist", "passed": false, "error": "E".repeat(700)},
                    {"stage": 3, "name": "expire", "passed": false, "error": null}
                ]
            }),
        );
        write_json(
            &Path::new(&trial1).join("review.json"),
            &json!({
                "good": ["fast"],
                "bad": ["no docs"],
                "conclusion": "Trial one is solid."
            }),
        );
        std::fs::create_dir_all(Path::new(&trial1).join("workspace/src")).unwrap();
        std::fs::write(
            Path::new(&trial1).join("workspace/src/main.go"),
            "package main\n",
        )
        .unwrap();
        // Trial two: malformed manifest, review without a conclusion.
        std::fs::create_dir_all(&trial2).unwrap();
        std::fs::write(Path::new(&trial2).join("manifest.json"), "{not json").unwrap();
        write_json(
            &Path::new(&trial2).join("review.json"),
            &json!({"good": [], "bad": [], "conclusion": ""}),
        );
        // Parent result: stem must match `{id}.json` under the repo `results/`.
        write_json(
            &root.join("run.json"),
            &parent_result_json(
                &id,
                task,
                vec![
                    subrun_json(&trial1, task, 100.0),
                    subrun_json(&trial2, task, 0.0),
                ],
            ),
        );

        let result = handle_review_run(
            id.clone(),
            root.to_string_lossy().to_string(),
            "openrouter/judge:batch".to_string(),
            Some("sk-test".to_string()),
            true, // force: the stale run-review.md must not short-circuit
        )
        .await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("batch API judges stall"), "{err}");
        // Synthesis failed before the write: the stale review is untouched.
        assert_eq!(
            std::fs::read_to_string(Path::new(&id).join("run-review.md")).unwrap(),
            "STALE\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parent result that does *not* deserialize as `BenchmarkRunResult`
    /// (no typed fields) so the best-trial fallback picks the highest
    /// pass rate. Also exercises the "no stages array", "manifest
    /// unreadable" and "review unreadable/malformed" edges.
    #[tokio::test]
    async fn test_review_run_falls_back_to_best_trial_when_parent_untyped() {
        let root = temp_dir("runs_fallback");
        let id = root.join("run").to_string_lossy().to_string();
        let low = root.join("run_low").to_string_lossy().to_string();
        let high = root.join("run_high").to_string_lossy().to_string();

        std::fs::create_dir_all(&id).unwrap();
        // low: valid JSON manifest without a stages array, no review.
        write_json(&Path::new(&low).join("manifest.json"), &json!({}));
        // high: no manifest at all, malformed review JSON, real workspace.
        std::fs::create_dir_all(&high).unwrap();
        std::fs::write(Path::new(&high).join("review.json"), "{not json").unwrap();
        std::fs::create_dir_all(Path::new(&high).join("workspace")).unwrap();
        std::fs::write(Path::new(&high).join("workspace/main.go"), "package main").unwrap();
        write_json(
            &root.join("run.json"),
            &json!({
                "model": "fixture-model",
                "task": "redis",
                "subruns": [
                    {"id": low, "pass_rate": 10.0},
                    {"id": high, "pass_rate": 90.0}
                ]
            }),
        );

        let err = handle_review_run(
            id.clone(),
            root.to_string_lossy().to_string(),
            "openrouter/judge:batch".to_string(),
            Some("sk-test".to_string()),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("batch API judges stall"), "{err}");
        assert!(!Path::new(&id).join("run-review.md").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parent result without a `subruns` key: empty trial table, default
    /// best id, still a clean deterministic pass before the judge gate.
    #[tokio::test]
    async fn test_review_run_handles_empty_subruns_fixture() {
        let root = temp_dir("runs_empty_subs");
        let id = root.join("run").to_string_lossy().to_string();
        std::fs::create_dir_all(&id).unwrap();
        write_json(
            &root.join("run.json"),
            &json!({"model": "fixture-model", "task": "redis"}),
        );

        let err = handle_review_run(
            id.clone(),
            root.to_string_lossy().to_string(),
            "openrouter/judge:batch".to_string(),
            Some("sk-test".to_string()),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("batch API judges stall"), "{err}");
        assert!(!Path::new(&id).join("run-review.md").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_regen_reviews_skips_counts_and_exports_portal() {
        let runs = temp_dir("regen_runs");
        let results = temp_dir("regen_results");
        let out = temp_dir("regen_out");

        // Skipped: already reviewed, no --force.
        std::fs::create_dir_all(runs.join("job_skip")).unwrap();
        std::fs::write(runs.join("job_skip/review.json"), "{}").unwrap();
        // Failed: directory without a readable manifest.
        std::fs::create_dir_all(runs.join("job_nomanifest")).unwrap();
        // Failed before any HTTP: readable manifest, batch judge model.
        write_json(
            &runs.join("job_batch/manifest.json"),
            &manifest_json("job_batch", "redis"),
        );
        std::fs::create_dir_all(runs.join("job_batch/workspace")).unwrap();
        std::fs::write(runs.join("job_batch/workspace/main.go"), "package main").unwrap();

        handle_regen_reviews(
            runs.to_string_lossy().to_string(),
            None,
            "openrouter/judge:batch".to_string(),
            Some("sk-test".to_string()),
            false,
            results.to_string_lossy().to_string(),
            out.to_string_lossy().to_string(),
            "http://localhost.invalid".to_string(),
            10,
            false,
        )
        .await
        .unwrap();

        assert!(out.join("index.html").exists());
        // Round-12 P1: the regen path publishes the sdb-sql board; the
        // legacy per-run pages and results.json are gone for good.
        assert!(!out.join("results.json").exists());
        assert!(!out.join("runs").is_dir());
        assert!(runs.join("job_skip/review.json").exists());
        assert!(!runs.join("job_batch/review.json").exists());
        assert!(!runs.join("job_nomanifest/review.json").exists());
        let _ = std::fs::remove_dir_all(&runs);
        let _ = std::fs::remove_dir_all(&results);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn test_regen_reviews_force_keeps_old_review_on_failure() {
        let runs = temp_dir("regen_force_runs");
        let results = temp_dir("regen_force_results");
        let out = temp_dir("regen_force_out");
        let id = "job_force";
        write_json(
            &runs.join(id).join("manifest.json"),
            &manifest_json(id, "redis"),
        );
        write_json(
            &runs.join(id).join("review.json"),
            &json!({"good": [], "bad": [], "conclusion": "old review"}),
        );

        handle_regen_reviews(
            runs.to_string_lossy().to_string(),
            Some(id.to_string()),
            "openrouter/judge:batch".to_string(),
            Some("sk-test".to_string()),
            true,
            results.to_string_lossy().to_string(),
            out.to_string_lossy().to_string(),
            "http://localhost.invalid".to_string(),
            5,
            true, // rollup-only portal branch
        )
        .await
        .unwrap();

        assert!(out.join("index.html").exists());
        assert!(!out.join("runs").exists());
        let kept = std::fs::read_to_string(runs.join(id).join("review.json")).unwrap();
        assert!(kept.contains("old review"));
        let _ = std::fs::remove_dir_all(&runs);
        let _ = std::fs::remove_dir_all(&results);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn test_regen_reviews_id_filter_mismatch_is_noop() {
        let runs = temp_dir("regen_filter_runs");
        let results = temp_dir("regen_filter_results");
        let out = temp_dir("regen_filter_out");
        std::fs::create_dir_all(runs.join("other_run")).unwrap();

        handle_regen_reviews(
            runs.to_string_lossy().to_string(),
            Some("missing-id".to_string()),
            "openrouter/judge:batch".to_string(),
            None,
            true,
            results.to_string_lossy().to_string(),
            out.to_string_lossy().to_string(),
            "http://localhost.invalid".to_string(),
            5,
            true,
        )
        .await
        .unwrap();

        assert!(out.join("index.html").exists());
        assert!(!out.join("runs").exists());
        let _ = std::fs::remove_dir_all(&runs);
        let _ = std::fs::remove_dir_all(&results);
        let _ = std::fs::remove_dir_all(&out);
    }
}
