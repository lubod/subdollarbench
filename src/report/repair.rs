//! One-shot data repair for audit findings (dry-run by default, `--yes` writes).
//!
//! Three independent problems, one command, in dependency order:
//! 1. **Trial-verdict cures.** Trial `runs/*/manifest.json` rows stamped
//!    `disqualified_cheat` are re-scanned with the *current* compliance
//!    checker; proven false positives (test-stub imports, import-line
//!    daemon names) are rewritten to the verdict the pipeline chain would
//!    produce today (`failed_tests`, or `completed` at 100%). Trials
//!    stamped `agent_killed` at 100% pass become `completed` (solved is
//!    solved — see `pipeline.rs`). Genuine cheats keep their stamp.
//! 2. **Loose-trial consolidation.** `results/*_trialN.json` files with no
//!    parent JSON (interrupted waves) are merged into real parents via
//!    `consolidate_multi_trial_run` — the same path the pipeline uses, so
//!    DB rows, trial-file cleanup, trial-dir hiding, and parent manifests
//!    all behave identically. Groups whose parent already exists are
//!    skipped, never clobbered.
//! 3. **Stale `runs` pruning.** `bench.db` `runs` rows with no `runs/<id>/`
//!    directory (pre-Oct-5 disk maintenance) are deleted; they currently
//!    surface as ghosts in the Runs tab.
//!
//! Already-merged parents whose subrun verdicts changed in step 1 are
//! re-merged so parent status follows. `graded_under` is preserved from the
//! unanimous trial value (or `"mixed"`): a repair recomputes bookkeeping,
//! it never re-grades.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::get_repo_root;
use crate::report::leaderboard::{BenchmarkRunResult, LeaderboardManager};
use crate::report::portal::PortalExporter;
use crate::verifier::compliance::ComplianceChecker;

pub struct RepairReport {
    pub manifests_scanned: usize,
    pub cured_dq: Vec<String>,
    pub cured_agent_killed: Vec<String>,
    pub still_dq: Vec<String>,
    pub loose_groups: usize,
    pub loose_trials: usize,
    pub consolidated: Vec<String>,
    pub skipped_existing_parent: Vec<String>,
    pub parents_remerged: Vec<String>,
    pub pruned_runs: Vec<String>,
    pub warnings: Vec<String>,
}

/// Strip a trailing `_trial<N>` suffix; None when the id is not a trial.
fn trial_parent_stem(id: &str) -> Option<String> {
    let (stem, suffix) = id.rsplit_once("_trial")?;
    if !suffix.chars().all(|c| c.is_ascii_digit()) || stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

/// Verdict the current pipeline chain would produce for these signals.
fn chain_verdict(pass_rate: f64) -> &'static str {
    if pass_rate >= 100.0 {
        "completed"
    } else {
        "failed_tests"
    }
}

fn read_manifest(path: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn manifest_status(path: &Path) -> Option<String> {
    read_manifest(path).and_then(|v| v.get("status")?.as_str().map(|s| s.to_string()))
}

/// Trial-cure scan results (phase 1).
type CureScan = Result<(Vec<String>, Vec<String>, Vec<String>, usize, Vec<String>)>;

/// Phase 1: cure trial manifests. Returns (cured_dq, cured_killed, still_dq).
/// Writes only when `apply`.
fn cure_trial_manifests(runs_dir: &Path, apply: bool) -> CureScan {
    let mut cured_dq = Vec::new();
    let mut cured_killed = Vec::new();
    let mut still_dq = Vec::new();
    let mut warnings = Vec::new();
    let mut scanned = 0usize;
    let entries = std::fs::read_dir(runs_dir)
        .with_context(|| format!("read runs dir {}", runs_dir.display()))?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // Trial manifests only: merged-parent dirs hold derived copies
        // (rebuilt by merges below — curing them directly would be
        // overwritten anyway, and a missing workspace/ would falsely
        // "pass" an empty scan). Parents are identified by the run.json
        // that only consolidate writes; single-trial runs have none.
        if !dir.join("workspace").is_dir() || dir.join("run.json").exists() {
            continue;
        }
        let mp = dir.join("manifest.json");
        let Some(status) = manifest_status(&mp) else {
            continue;
        };
        scanned += 1;
        let id = entry.file_name().to_string_lossy().to_string();
        if status == "disqualified_cheat" {
            match ComplianceChecker::check_no_frameworks(&dir.join("workspace")) {
                Ok(()) => {
                    let pass = read_manifest(&mp)
                        .and_then(|v| v.get("pass_rate")?.as_f64())
                        .unwrap_or(0.0);
                    let new_status = chain_verdict(pass);
                    cured_dq.push(format!("{id} -> {new_status}"));
                    if apply {
                        let mut doc = read_manifest(&mp).unwrap();
                        doc["status"] = serde_json::Value::String(new_status.to_string());
                        let text = serde_json::to_string_pretty(&doc)?;
                        std::fs::write(&mp, text)?;
                    }
                }
                Err(reason) => {
                    still_dq.push(format!("{id}: {reason}"));
                }
            }
        } else if status == "agent_killed" {
            let pass = read_manifest(&mp)
                .and_then(|v| v.get("pass_rate")?.as_f64())
                .unwrap_or(0.0);
            if pass >= 100.0 {
                cured_killed.push(id.clone());
                if apply {
                    let mut doc = read_manifest(&mp).unwrap();
                    doc["status"] = serde_json::Value::String("completed".to_string());
                    let text = serde_json::to_string_pretty(&doc)?;
                    std::fs::write(&mp, text)?;
                }
            }
        }
        let _ = &mut warnings;
    }
    Ok((cured_dq, cured_killed, still_dq, scanned, warnings))
}

/// Phase 2+3: sync cured statuses into result rows, consolidate loose
/// groups, and re-merge affected parents. `cured_ids` maps trial id ->
/// new status.
#[allow(clippy::too_many_arguments)]
/// Consolidation outcome: (consolidated, skipped_existing, remerged,
/// warnings, loose_groups, loose_trials).
type Consolidation = Result<(
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    usize,
    usize,
)>;

fn consolidate_and_remerge(
    results_dir: &Path,
    cured: &BTreeMap<String, String>,
    apply: bool,
) -> Consolidation {
    // Load every result row.
    let mut rows: BTreeMap<String, BenchmarkRunResult> = BTreeMap::new();
    for entry in std::fs::read_dir(results_dir)
        .with_context(|| format!("read results dir {}", results_dir.display()))?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") || name == "ref_history.jsonl" {
            continue;
        }
        let text = match std::fs::read_to_string(entry.path()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Ok(row) = serde_json::from_str::<BenchmarkRunResult>(&text) {
            rows.insert(name.trim_end_matches(".json").to_string(), row);
        }
    }
    let mut consolidated = Vec::new();
    let mut skipped_existing = Vec::new();
    let mut warnings = Vec::new();
    // Sync cured statuses into loose rows and embedded subruns.
    let mut dirty_parents: BTreeSet<String> = BTreeSet::new();
    for (id, row) in rows.iter_mut() {
        if let Some(new_status) = cured.get(id) {
            row.status = Some(new_status.clone());
            dirty_parents.insert(id.clone());
        }
        for sub in row.subruns.iter_mut() {
            if let Some(new_status) = cured.get(&sub.id) {
                if sub.status.as_deref() != Some(new_status.as_str()) {
                    sub.status = Some(new_status.clone());
                    dirty_parents.insert(id.clone());
                }
            }
        }
    }
    // Group loose trials (no subruns of their own) by parent stem.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, row) in &rows {
        if row.subruns.is_empty() {
            if let Some(stem) = trial_parent_stem(id) {
                groups.entry(stem).or_default().push(id.clone());
            }
        }
    }
    let loose_trials: usize = groups.values().map(|v| v.len()).sum();
    let loose_groups = groups.len();
    for (parent_id, trial_ids) in &groups {
        if rows.contains_key(parent_id) {
            skipped_existing.push(parent_id.clone());
            continue;
        }
        let mut trial_rows: Vec<BenchmarkRunResult> = trial_ids
            .iter()
            .filter_map(|tid| rows.get(tid).cloned())
            .collect();
        trial_rows.sort_by(|a, b| a.id.cmp(&b.id));
        // Trial manifests are authoritative for status (loose JSON rows
        // predate the status field); rows were already synced above.
        let unanimous_grader = {
            let g: BTreeSet<String> = trial_rows
                .iter()
                .filter_map(|r| r.graded_under.clone())
                .collect();
            if g.len() == 1 {
                Some(g.into_iter().next().unwrap())
            } else if g.is_empty() {
                None
            } else {
                Some("mixed".to_string())
            }
        };
        if apply {
            // Persist synced statuses before merging (merge reads rows).
            for tid in trial_ids {
                if let Some(row) = rows.get(tid) {
                    let text = serde_json::to_string_pretty(row)?;
                    std::fs::write(results_dir.join(format!("{tid}.json")), text)?;
                }
            }
        }
        let merged = if apply {
            LeaderboardManager::consolidate_multi_trial_run(
                parent_id,
                &trial_rows,
                true,
                None,
                None,
            )
        } else {
            LeaderboardManager::consolidate_multi_trial_run(
                parent_id,
                &trial_rows,
                false,
                None,
                None,
            )
        };
        match merged {
            Some(_) => {
                consolidated.push(parent_id.clone());
                if apply {
                    if let Some(g) = unanimous_grader {
                        restore_graded_under(results_dir, parent_id, &g)?;
                    }
                }
            }
            None => warnings.push(format!("{parent_id}: merge returned None")),
        }
    }
    // Re-merge already-merged parents whose subruns changed, excluding
    // freshly consolidated ones (their statuses are already current).
    let consolidated_set: BTreeSet<String> = consolidated.iter().cloned().collect();
    let mut remerged = Vec::new();
    for pid in dirty_parents {
        if consolidated_set.contains(&pid) || trial_parent_stem(&pid).is_some() {
            continue; // loose trial file itself, or just consolidated
        }
        let Some(parent) = rows.get(&pid) else {
            continue;
        };
        if parent.subruns.is_empty() {
            continue;
        }
        let old_grader = parent.graded_under.clone();
        let old_drift = parent.ref_drift;
        if apply {
            // Persist synced subrun statuses first.
            let text = serde_json::to_string_pretty(parent)?;
            std::fs::write(results_dir.join(format!("{pid}.json")), text)?;
        }
        let merged = if apply {
            LeaderboardManager::consolidate_multi_trial_run(
                &pid,
                &parent.subruns,
                true,
                old_drift,
                parent.protocol.clone(),
            )
        } else {
            LeaderboardManager::consolidate_multi_trial_run(
                &pid,
                &parent.subruns,
                false,
                old_drift,
                parent.protocol.clone(),
            )
        };
        if merged.is_some() {
            remerged.push(pid.clone());
            if apply {
                if let Some(g) = old_grader {
                    restore_graded_under(results_dir, &pid, &g)?;
                }
            }
        } else {
            warnings.push(format!("{pid}: re-merge returned None"));
        }
    }
    Ok((
        consolidated,
        skipped_existing,
        remerged,
        warnings,
        loose_groups,
        loose_trials,
    ))
}

/// Restore a parent's original grader after a repair merge (a repair
/// recomputes bookkeeping; it never re-grades).
fn restore_graded_under(results_dir: &Path, parent_id: &str, grader: &str) -> Result<()> {
    let path = results_dir.join(format!("{parent_id}.json"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read repaired parent {}", path.display()))?;
    let mut val: serde_json::Value = serde_json::from_str(&text)?;
    val["graded_under"] = serde_json::Value::String(grader.to_string());
    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
    if let Some(store) = LeaderboardManager::open_store_if_exists() {
        if let Ok(row) = serde_json::from_str::<BenchmarkRunResult>(&val.to_string()) {
            let _ = store.upsert_parent(&row);
        }
    }
    Ok(())
}

/// Phase 4: drop `runs` rows with no directory on disk.
fn prune_stale_runs(runs_dir: &Path, apply: bool) -> Result<Vec<String>> {
    let mut pruned = Vec::new();
    let Some(store) = LeaderboardManager::open_store_if_exists() else {
        return Ok(pruned);
    };
    for m in store.load_run_manifests() {
        if !runs_dir.join(&m.run_id).is_dir() {
            pruned.push(m.run_id.clone());
            if apply {
                let _ = store.delete_run_manifest(&m.run_id);
            }
        }
    }
    Ok(pruned)
}

pub struct RepairOutcome {
    pub report: RepairReport,
}

/// Dry-run by default; `apply=true` writes (manifests, results JSON,
/// bench.db, trial-file cleanup via consolidate).
pub fn run_repair(results_dir: &Path, runs_dir: &Path, apply: bool) -> Result<RepairReport> {
    // Operate on canonical repo dirs: consolidate_multi_trial_run and the
    // manifest lookups resolve repo-root paths internally. Refuse anything
    // else rather than half-migrating two locations.
    let root = get_repo_root();
    let canon_results = root.join("results");
    let canon_runs = root.join("runs");
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(results_dir) != canon(&canon_results) || canon(runs_dir) != canon(&canon_runs) {
        anyhow::bail!(
            "repair operates on the canonical repo dirs ({} and {}); got {} and {} — run from the repo root with defaults",
            canon_results.display(),
            canon_runs.display(),
            results_dir.display(),
            runs_dir.display()
        );
    }
    let (cured_dq, cured_killed, still_dq, scanned, mut warnings) =
        cure_trial_manifests(&canon_runs, apply)?;
    let cured: BTreeMap<String, String> = cured_dq
        .iter()
        .filter_map(|e| {
            e.split_once(" -> ")
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .chain(
            cured_killed
                .iter()
                .map(|id| (id.clone(), "completed".to_string())),
        )
        .collect();
    let (consolidated, skipped_existing, remerged, mut warnings2, loose_groups, loose_trials) =
        consolidate_and_remerge(&canon_results, &cured, apply)?;
    warnings.append(&mut warnings2);
    let pruned = prune_stale_runs(&canon_runs, apply)?;
    // Refresh the static portal when anything changed on disk.
    if apply && (!consolidated.is_empty() || !remerged.is_empty()) {
        // Round-12 P1: one resolver for every writer.
        let portal = crate::report::archive::RunArchiver::resolve_portal_dir();
        let _ = PortalExporter::export_sql_board(
            canon_runs.to_str().unwrap_or("./runs"),
            portal.to_str().unwrap_or("./portal"),
        );
    }
    Ok(RepairReport {
        manifests_scanned: scanned,
        cured_dq,
        cured_agent_killed: cured_killed,
        still_dq,
        loose_groups,
        loose_trials,
        consolidated,
        skipped_existing_parent: skipped_existing,
        parents_remerged: remerged,
        pruned_runs: pruned,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trial_parent_stem() {
        assert_eq!(
            trial_parent_stem("redis_m_1_w0_trial3").as_deref(),
            Some("redis_m_1_w0")
        );
        assert_eq!(trial_parent_stem("redis_m_1_w0"), None);
        assert_eq!(trial_parent_stem("weird_trialX"), None);
        assert_eq!(trial_parent_stem("_trial1"), None);
    }

    #[test]
    fn test_chain_verdict() {
        assert_eq!(chain_verdict(100.0), "completed");
        assert_eq!(chain_verdict(0.0), "failed_tests");
        assert_eq!(chain_verdict(77.8), "failed_tests");
    }

    use crate::report::Store;
    use serde_json::json;

    fn unique_temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sdb-repair-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// One trial-shaped run dir: `manifest.json` + an empty `workspace/`.
    fn write_manifest(runs: &Path, id: &str, status: &str, pass_rate: Option<f64>) {
        let dir = runs.join(id);
        std::fs::create_dir_all(dir.join("workspace")).expect("workspace dir");
        let mut doc = json!({
            "run_id": id,
            "model": "test-model",
            "task": "redis",
            "status": status,
            "passed_stages": 2,
            "total_stages": 4,
            "efficiency_score": 0.42,
            "cost_usd": 0.001,
            "tokens": {"prompt_tokens": 10, "cached_tokens": 2, "completion_tokens": 3, "total_tokens": 15},
        });
        if let Some(p) = pass_rate {
            doc["pass_rate"] = json!(p);
        }
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&doc).expect("manifest json"),
        )
        .expect("write manifest");
    }

    fn manifest_doc(runs: &Path, id: &str) -> serde_json::Value {
        let text = std::fs::read_to_string(runs.join(id).join("manifest.json")).expect("read");
        serde_json::from_str(&text).expect("manifest json")
    }

    /// Mixed trial zoo: provable false positives (clean / test-stub
    /// workspaces), a genuine flask cheat, agent_killed stamps at and below
    /// 100%, plus dirs the scan must skip (merged parent with run.json, no
    /// manifest, no workspace, non-dir entry).
    fn build_cure_fixture(runs: &Path) {
        write_manifest(runs, "task_m_1_w0_trial1", "disqualified_cheat", Some(50.0));
        write_manifest(runs, "task_m_1_w0_trial2", "disqualified_cheat", Some(0.0));
        std::fs::write(
            runs.join("task_m_1_w0_trial2/workspace/server.py"),
            "from flask import Flask\napp = Flask(__name__)\n",
        )
        .expect("genuine cheat");
        write_manifest(
            runs,
            "task_m_1_w0_trial3",
            "disqualified_cheat",
            Some(100.0),
        );
        std::fs::write(
            runs.join("task_m_1_w0_trial3/workspace/stub.py"),
            "import http.server\nclass H(http.server.BaseHTTPRequestHandler): pass\n",
        )
        .expect("test-stub false positive");
        // DQ with no pass_rate: the chain verdict must fall back to 0%.
        write_manifest(runs, "task_m_1_w0_trial4", "disqualified_cheat", None);
        write_manifest(runs, "task_m_1_w0_trial5", "agent_killed", Some(100.0));
        write_manifest(runs, "task_m_1_w0_trial6", "agent_killed", Some(99.9));
        write_manifest(runs, "task_m_1_w0_trial7", "completed", Some(100.0));
        // Merged-parent dir (workspace + run.json) is never scanned/cured.
        write_manifest(runs, "task_m_1_w0", "disqualified_cheat", Some(50.0));
        std::fs::write(runs.join("task_m_1_w0/run.json"), "{}\n").expect("run.json");
        std::fs::create_dir_all(runs.join("no_manifest_trial/workspace")).expect("workspace");
        std::fs::create_dir_all(runs.join("bare_dir")).expect("bare dir");
        std::fs::write(
            runs.join("bare_dir/manifest.json"),
            r#"{"status":"disqualified_cheat"}"#,
        )
        .expect("manifest without workspace");
        std::fs::write(runs.join("notes.txt"), "not a run\n").expect("stray file");
    }

    #[test]
    fn test_cure_scan_dry_run_classifies_only_provable_cases() {
        let root = unique_temp("cure-dry");
        let runs = root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        build_cure_fixture(&runs);

        let (cured_dq, cured_killed, still_dq, scanned, warnings) =
            cure_trial_manifests(&runs, false).expect("scan");

        assert_eq!(scanned, 7, "only trial manifests with a status are scanned");
        let mut dq = cured_dq;
        dq.sort();
        assert_eq!(
            dq,
            vec![
                "task_m_1_w0_trial1 -> failed_tests".to_string(),
                "task_m_1_w0_trial3 -> completed".to_string(),
                "task_m_1_w0_trial4 -> failed_tests".to_string(),
            ]
        );
        assert_eq!(cured_killed, vec!["task_m_1_w0_trial5".to_string()]);
        assert_eq!(still_dq.len(), 1, "the flask cheat stays disqualified");
        assert!(
            still_dq[0].starts_with("task_m_1_w0_trial2: Anti-Cheat Violation"),
            "{still_dq:?}"
        );
        assert!(warnings.is_empty());

        // Dry run: every manifest on disk still carries its original stamp.
        for id in [
            "task_m_1_w0_trial1",
            "task_m_1_w0_trial3",
            "task_m_1_w0_trial4",
            "task_m_1_w0",
        ] {
            assert_eq!(
                manifest_doc(&runs, id)["status"],
                "disqualified_cheat",
                "{id}: dry run must not write"
            );
        }
        assert_eq!(
            manifest_doc(&runs, "task_m_1_w0_trial5")["status"],
            "agent_killed",
            "dry run must not write"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_cure_apply_changes_only_the_verdict_field() {
        let root = unique_temp("cure-apply");
        let runs = root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        build_cure_fixture(&runs);

        let ids = [
            "task_m_1_w0_trial1",
            "task_m_1_w0_trial2",
            "task_m_1_w0_trial3",
            "task_m_1_w0_trial4",
            "task_m_1_w0_trial5",
            "task_m_1_w0_trial6",
            "task_m_1_w0_trial7",
            "task_m_1_w0",
        ];
        let before: Vec<serde_json::Value> = ids.iter().map(|id| manifest_doc(&runs, id)).collect();

        let (cured_dq, cured_killed, still_dq, scanned, _) =
            cure_trial_manifests(&runs, true).expect("apply");
        assert_eq!(scanned, 7);
        let mut dq = cured_dq;
        dq.sort();
        assert_eq!(
            dq,
            vec![
                "task_m_1_w0_trial1 -> failed_tests".to_string(),
                "task_m_1_w0_trial3 -> completed".to_string(),
                "task_m_1_w0_trial4 -> failed_tests".to_string(),
            ]
        );
        assert_eq!(cured_killed, vec!["task_m_1_w0_trial5".to_string()]);
        assert_eq!(still_dq.len(), 1);

        for (id, want) in [
            ("task_m_1_w0_trial1", "failed_tests"),
            ("task_m_1_w0_trial3", "completed"),
            ("task_m_1_w0_trial4", "failed_tests"),
            ("task_m_1_w0_trial5", "completed"),
        ] {
            let pos = ids.iter().position(|x| *x == id).unwrap();
            let after = manifest_doc(&runs, id);
            assert_eq!(after["status"], want, "{id} verdict");
            // The verdict is the ONLY field a cure may change: strip it from
            // both snapshots and require full equality (scores, tokens, cost).
            let mut a = after.clone();
            let mut b = before[pos].clone();
            a.as_object_mut().unwrap().remove("status");
            b.as_object_mut().unwrap().remove("status");
            assert_eq!(a, b, "{id}: repair must not rescore");
            assert_eq!(a["efficiency_score"], 0.42, "{id} score untouched");
            assert_eq!(a["cost_usd"], 0.001, "{id} cost untouched");
            assert_eq!(a["tokens"], b["tokens"], "{id} tokens untouched");
        }
        for id in [
            "task_m_1_w0_trial2",
            "task_m_1_w0_trial6",
            "task_m_1_w0_trial7",
            "task_m_1_w0",
        ] {
            let pos = ids.iter().position(|x| *x == id).unwrap();
            assert_eq!(manifest_doc(&runs, id), before[pos], "{id}: untouched");
        }
        assert_eq!(
            std::fs::read_to_string(runs.join("task_m_1_w0/run.json")).unwrap(),
            "{}\n",
            "parent run.json untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_cure_scan_missing_runs_dir_is_an_error() {
        let root = unique_temp("cure-missing");
        let err = cure_trial_manifests(&root.join("nope"), false).unwrap_err();
        assert!(
            format!("{err:#}").contains("read runs dir"),
            "missing runs dir must error, got: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn result_row(id: &str, pass_rate: f64, status: Option<&str>) -> BenchmarkRunResult {
        let mut v = json!({
            "id": id,
            "model": "test-model",
            "task": "redis",
            "language": "Go",
            "pass_rate": pass_rate,
            "passed_stages": 0,
            "total_stages": 4,
            "prompt_tokens": 10,
            "cached_tokens": 0,
            "completion_tokens": 5,
            "total_cost_usd": 0.001,
            "savings_percent": 0.0,
            "efficiency_score": 0.5,
            "timestamp": "2026-10-01T00:00:00Z"
        });
        if let Some(s) = status {
            v["status"] = json!(s);
        }
        serde_json::from_value(v).expect("row fixture")
    }

    fn write_row(results: &Path, row: &BenchmarkRunResult) {
        std::fs::write(
            results.join(format!("{}.json", row.id)),
            serde_json::to_string_pretty(row).expect("serialize row"),
        )
        .expect("write row");
    }

    fn read_dir_bytes(dir: &Path) -> BTreeMap<String, String> {
        std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| {
                (
                    e.file_name().to_string_lossy().to_string(),
                    std::fs::read_to_string(e.path()).unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Dry run must classify (groups, skips, remerges) and write nothing:
    /// `--yes` is what mutates, and only the apply path may persist synced
    /// statuses.
    #[test]
    fn test_consolidate_dry_run_classifies_and_writes_nothing() {
        let root = unique_temp("consolidate-dry");
        let results = root.join("results");
        std::fs::create_dir_all(&results).unwrap();

        let mut alpha1 = result_row("alpha_trial1", 100.0, Some("completed"));
        alpha1.graded_under = Some("rules-a".to_string());
        let mut alpha2 = result_row("alpha_trial2", 0.0, Some("failed_tests"));
        alpha2.graded_under = Some("rules-a".to_string());
        write_row(&results, &alpha1);
        write_row(&results, &alpha2);

        // Loose trial whose parent already exists: skipped, never clobbered.
        let gamma_t1 = result_row("gamma_trial1", 100.0, Some("completed"));
        let mut gamma = result_row("gamma", 100.0, Some("completed"));
        gamma.subruns = vec![gamma_t1.clone()];
        write_row(&results, &gamma_t1);
        write_row(&results, &gamma);

        // Merged parent whose embedded trial changed: re-merge bookkeeping.
        let mut delta_t1 = result_row("delta_trial1", 100.0, Some("agent_killed"));
        delta_t1.graded_under = Some("rules-d".to_string());
        let mut delta = result_row("delta", 0.0, Some("agent_killed"));
        delta.subruns = vec![delta_t1];
        delta.graded_under = Some("rules-d".to_string());
        delta.protocol = Some("fixed_n".to_string());
        write_row(&results, &delta);

        // Single non-trial row: belongs to no group and no parent.
        write_row(&results, &result_row("epsilon", 50.0, Some("failed_tests")));
        std::fs::write(results.join("ref_history.jsonl"), "{\"old\":true}\n").unwrap();
        std::fs::write(results.join("notes.txt"), "not a result\n").unwrap();

        let before = read_dir_bytes(&results);
        let cured: BTreeMap<String, String> = [
            ("alpha_trial1".to_string(), "completed".to_string()),
            ("delta_trial1".to_string(), "completed".to_string()),
        ]
        .into_iter()
        .collect();

        let (consolidated, skipped, remerged, warnings, loose_groups, loose_trials) =
            consolidate_and_remerge(&results, &cured, false).expect("dry run");

        assert_eq!(consolidated, vec!["alpha".to_string()]);
        assert_eq!(skipped, vec!["gamma".to_string()]);
        assert_eq!(remerged, vec!["delta".to_string()]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!((loose_groups, loose_trials), (2, 3));
        assert!(
            !results.join("alpha.json").exists(),
            "dry run must not consolidate"
        );
        assert_eq!(
            read_dir_bytes(&results),
            before,
            "dry run must leave every results file byte-identical"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A merged parent with no changed subruns is never re-merged: dirty
    /// tracking must not rewrite clean parents.
    #[test]
    fn test_consolidate_dry_run_leaves_clean_parents_alone() {
        let root = unique_temp("consolidate-clean");
        let results = root.join("results");
        std::fs::create_dir_all(&results).unwrap();
        let mut sub = result_row("zeta_trial1", 100.0, Some("completed"));
        sub.graded_under = Some("rules-z".to_string());
        let mut parent = result_row("zeta", 100.0, Some("completed"));
        parent.subruns = vec![sub];
        parent.graded_under = Some("rules-z".to_string());
        write_row(&results, &parent);
        let before = read_dir_bytes(&results);

        let (consolidated, skipped, remerged, warnings, groups, trials) =
            consolidate_and_remerge(&results, &BTreeMap::new(), false).expect("dry run");

        assert!(consolidated.is_empty());
        assert!(skipped.is_empty());
        assert!(remerged.is_empty(), "clean parent must not re-merge");
        assert!(warnings.is_empty());
        assert_eq!((groups, trials), (0, 0));
        assert_eq!(read_dir_bytes(&results), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `prune_stale_runs` reads the resolved primary store, so the test points
    /// `SDB_BENCH_DB` at a temp DB. Env is process-global while other tests
    /// run in parallel: hold it only across the call, restore it via a Drop
    /// guard, and bail out defensively if another test owns the variable.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct BenchDbEnv {
        saved: Option<String>,
    }

    impl BenchDbEnv {
        fn set(path: &Path) -> Self {
            let saved = std::env::var("SDB_BENCH_DB").ok();
            std::env::set_var("SDB_BENCH_DB", path);
            Self { saved }
        }
    }

    impl Drop for BenchDbEnv {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var("SDB_BENCH_DB", v),
                None => std::env::remove_var("SDB_BENCH_DB"),
            }
        }
    }

    fn run_manifest(run_id: &str, completed_at: &str) -> crate::report::RunManifest {
        serde_json::from_value(json!({
            "run_id": run_id,
            "model": "test-model",
            "task": "redis",
            "status": "completed",
            "language": "Go",
            "started_at": "2026-10-01T00:00:00Z",
            "completed_at": completed_at,
            "duration_seconds": 1.0,
            "pass_rate": 100.0,
            "passed_stages": 4,
            "total_stages": 4,
            "stages": [],
            "throughput_req_sec": null,
            "tokens": {"prompt_tokens": 1, "cached_tokens": 0, "completion_tokens": 1, "total_tokens": 2},
            "cost_usd": 0.0,
            "savings_percent": 0.0,
            "efficiency_score": 0.0,
            "files": []
        }))
        .expect("manifest fixture")
    }

    #[test]
    fn test_prune_stale_runs_dry_run_then_apply() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let root = unique_temp("prune");
        let db = root.join("bench.db");
        {
            let store = Store::open(&db).expect("open temp db");
            store
                .upsert_run_manifest(&run_manifest("keep-1", "2026-10-03T00:00:00Z"))
                .unwrap();
            store
                .upsert_run_manifest(&run_manifest("stale-1", "2026-10-02T00:00:00Z"))
                .unwrap();
            store
                .upsert_run_manifest(&run_manifest("stale-2", "2026-10-01T00:00:00Z"))
                .unwrap();
        }
        let runs = root.join("runs");
        std::fs::create_dir_all(runs.join("keep-1")).unwrap();

        let dry = {
            let env = BenchDbEnv::set(&db);
            if Store::resolve_db_path() != db {
                drop(env);
                eprintln!("skipping prune assertions: another test owns SDB_BENCH_DB");
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            prune_stale_runs(&runs, false).expect("dry run")
        };
        let mut dry_sorted = dry.clone();
        dry_sorted.sort();
        assert_eq!(
            dry_sorted,
            vec!["stale-1".to_string(), "stale-2".to_string()]
        );
        {
            let store = Store::open(&db).unwrap();
            assert_eq!(store.run_manifest_count(), 3, "dry run must not delete");
        }

        let applied = {
            let env = BenchDbEnv::set(&db);
            if Store::resolve_db_path() != db {
                drop(env);
                eprintln!("skipping prune assertions: another test owns SDB_BENCH_DB");
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            prune_stale_runs(&runs, true).expect("apply")
        };
        assert_eq!(applied.len(), 2);
        {
            let store = Store::open(&db).unwrap();
            assert_eq!(store.run_manifest_count(), 1);
            assert!(store.get_run_manifest("keep-1").is_some());
            assert!(store.get_run_manifest("stale-1").is_none());
            assert!(store.get_run_manifest("stale-2").is_none());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Grader restoration after a repair merge: the file and the DB row both
    /// carry the passed-through grader, and nothing else is rescored.
    #[test]
    fn test_restore_graded_under_writes_file_and_db_without_regrading() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let root = unique_temp("restore");
        let db = root.join("bench.db");
        {
            let _ = Store::open(&db).expect("open temp db");
        }
        let results = root.join("results");
        std::fs::create_dir_all(&results).unwrap();
        let mut row = result_row("parent-restore", 100.0, Some("completed"));
        row.graded_under = Some("rules-old".to_string());
        row.efficiency_score = 1.75;
        write_row(&results, &row);

        let env = BenchDbEnv::set(&db);
        if Store::resolve_db_path() != db {
            drop(env);
            eprintln!("skipping restore assertions: another test owns SDB_BENCH_DB");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        restore_graded_under(&results, "parent-restore", "rules-old-kept").expect("restore");
        drop(env);

        let after: BenchmarkRunResult = serde_json::from_str(
            &std::fs::read_to_string(results.join("parent-restore.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(after.graded_under.as_deref(), Some("rules-old-kept"));
        assert_eq!(after.pass_rate, 100.0, "grader restore must not regrade");
        assert_eq!(after.efficiency_score, 1.75, "score untouched");
        assert_eq!(after.status.as_deref(), Some("completed"));

        let store = Store::open(&db).unwrap();
        let parents = store.load_all_parents();
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].graded_under.as_deref(), Some("rules-old-kept"));
        assert_eq!(parents[0].efficiency_score, 1.75);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_run_repair_refuses_non_canonical_dirs() {
        let root = unique_temp("guard");
        let err = match run_repair(&root.join("results"), &root.join("runs"), false) {
            Ok(_) => panic!("non-canonical dirs must be refused"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("canonical repo dirs"), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
