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
            LeaderboardManager::consolidate_multi_trial_run(parent_id, &trial_rows, true, None)
        } else {
            LeaderboardManager::consolidate_multi_trial_run(parent_id, &trial_rows, false, None)
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
            LeaderboardManager::consolidate_multi_trial_run(&pid, &parent.subruns, true, old_drift)
        } else {
            LeaderboardManager::consolidate_multi_trial_run(&pid, &parent.subruns, false, old_drift)
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
        let _ = PortalExporter::export_rollup(
            canon_results.to_str().unwrap_or("./results"),
            root.join("portal").to_str().unwrap_or("./portal"),
            "",
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
}
