//! Post-hoc annotation of accepted turn-1 reasoning cutoffs.
//!
//! A cutoff trial exits clean with zero tool calls and an empty workspace:
//! the model thought itself into `finish: length` on turn 1 before acting
//! (measured: ~30k reasoning tokens, idle `succeeded`, 0 tools). New trials
//! can recover live (same-session nudge, cap 2); trials that exhaust
//! recovery — and all archived trials, which predate recovery — keep their
//! honest 0% and get flagged here instead of rescored.
//!
//! Detection uses bench records only (manifest + console.log + workspace
//! dir), never the external OpenCode session store, so the command is
//! self-contained and repeatable after any wave, old and new rows alike:
//! opencode harness + `failed_tests` + 0% + agent launched (Turn 1 in the
//! console) + zero `[ACTION]` lines + empty workspace + no billing/auth
//! signature (a 403 never got to think — different failure, excluded).
//!
//! Annotation only: `cutoff_turn1: true` on trial manifests + bench.db
//! `runs` rows. Scores, costs, parents, and results JSON are untouched by
//! design (nothing numeric changes, so merges have nothing to propagate).
//! Dry-run by default; `--yes` writes with tmp-file + rename.

use crate::config::get_repo_root;
use crate::report::leaderboard::LeaderboardManager;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct CutoffAnnotateReport {
    pub manifests_scanned: usize,
    pub annotated: Vec<String>,
    pub db_annotated: usize,
    pub skipped_billing: usize,
    pub warnings: Vec<String>,
}

/// True when the row still needs evaluation (absent or null flag). An
/// explicit `false` means human-cleared — never re-touch; `true` means
/// already annotated (idempotence).
fn needs_cutoff_check(m: &Value) -> bool {
    m.get("cutoff_turn1").map(|v| v.is_null()).unwrap_or(true)
}

/// The cutoff signature over bench records. Every condition is load-bearing:
///
/// - opencode + failed_tests + 0%: the measured shape (a cheat DQ wrote
///   files; a killed agent isn't `failed_tests`; a partial pass acted).
/// - "Turn 1 started" present: the agent launched (excludes setup failures
///   that never reached the model).
/// - zero `[ACTION]` lines + empty workspace: never acted, never produced.
/// - no billing/auth signature: a rejected key never got to think.
fn is_cutoff_trial(manifest: &Value, console: &str, workspace_dir: &Path) -> bool {
    // Trials only: parent manifests echo trial console lines into their own
    // log, which mimics the signature on an all-zero parent. The flag marks
    // trial-level events; aggregates stay unflagged (scores unchanged, so
    // merges have nothing to propagate anyway).
    let is_trial = manifest
        .get("run_id")
        .and_then(|r| r.as_str())
        .map(|id| {
            id.rsplit_once("_trial")
                .map(|(_, s)| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                == Some(true)
        })
        .unwrap_or(false);
    if !is_trial {
        return false;
    }
    if manifest.get("harness").and_then(|h| h.as_str()) != Some("opencode") {
        return false;
    }
    if manifest.get("status").and_then(|s| s.as_str()) != Some("failed_tests") {
        return false;
    }
    if manifest.get("pass_rate").and_then(|p| p.as_f64()) != Some(0.0) {
        return false;
    }
    if !console.contains("Turn 1 started") {
        return false;
    }
    if console.matches("[ACTION]").count() > 0 {
        return false;
    }
    if crate::sandbox::opencode::OpenCodeRunner::workspace_has_content(workspace_dir) {
        return false;
    }
    // A billing/auth rejection never got to think — different failure.
    // Match BOTH shapes: the `[BILLING] <reason>` lines the pipeline logs
    // when the tripwire fires (classify_billing_error does not re-match its
    // own reason strings — verified live: all three reason shapes return
    // None), and raw provider stderr shapes via the classifier.
    for line in console.lines() {
        let t = line.trim();
        if t.contains("[BILLING]") || crate::sandbox::omp::classify_billing_error(t).is_some() {
            return false;
        }
    }
    true
}

/// Dry-run by default. `apply=false` reports what would change and writes
/// nothing, matching `repair`/`cost-annotate` semantics.
pub fn run_cutoff_annotate(runs_dir: &Path, apply: bool) -> Result<CutoffAnnotateReport> {
    let root = get_repo_root();
    let canon_runs = root.join("runs");
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(runs_dir) != canon(&canon_runs) {
        anyhow::bail!(
            "annotate-cutoff operates on the canonical repo runs dir ({}); got {}",
            canon_runs.display(),
            runs_dir.display()
        );
    }

    // One pass over trial manifests: read manifest + console + workspace.
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&canon_runs)
        .with_context(|| format!("read {}", canon_runs.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let mut flagged: Vec<(PathBuf, String)> = Vec::new();
    let mut skipped_billing = 0usize;
    let manifests_scanned = entries.len();
    for e in &entries {
        let manifest_path = e.path().join("manifest.json");
        let Ok(text) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if !needs_cutoff_check(&v) {
            continue;
        }
        // Cheap gates first (manifest-only); disk reads only for candidates.
        if v.get("harness").and_then(|h| h.as_str()) != Some("opencode") {
            continue;
        }
        if v.get("status").and_then(|s| s.as_str()) != Some("failed_tests") {
            continue;
        }
        if v.get("pass_rate").and_then(|p| p.as_f64()) != Some(0.0) {
            continue;
        }
        let console = std::fs::read_to_string(e.path().join("console.log")).unwrap_or_default();
        if !is_cutoff_trial(&v, &console, &e.path().join("workspace")) {
            // Count billing-shaped near-misses so the report shows the
            // exclusion working, not just silent skips.
            if v.get("harness").and_then(|h| h.as_str()) == Some("opencode")
                && console.contains("Turn 1 started")
                && console.matches("[ACTION]").count() == 0
                && console
                    .lines()
                    .any(|l| crate::sandbox::omp::classify_billing_error(l.trim()).is_some())
            {
                skipped_billing += 1;
            }
            continue;
        }
        let id = v
            .get("run_id")
            .and_then(|r| r.as_str())
            .unwrap_or("<unknown>")
            .to_string();
        flagged.push((manifest_path, id));
    }

    // Write manifests (tmp + rename, never partial).
    let mut annotated = Vec::new();
    for (path, id) in &flagged {
        annotated.push(id.clone());
        if apply {
            let text = std::fs::read_to_string(path)?;
            let mut v = serde_json::from_str::<Value>(&text)?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("cutoff_turn1".to_string(), Value::Bool(true));
            }
            let tmp = path.with_extension("json.annotate-tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)
                .with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, path)
                .with_context(|| format!("rename onto {}", path.display()))?;
        }
    }

    // bench.db `runs` rows, so analysis queries agree with the manifests.
    let mut db_annotated = 0usize;
    if let Some(store) = LeaderboardManager::open_store_if_exists() {
        let ids: std::collections::HashSet<&str> =
            flagged.iter().map(|(_, id)| id.as_str()).collect();
        for mut m in store.load_run_manifests() {
            if !ids.contains(m.run_id.as_str()) {
                continue;
            }
            if m.cutoff_turn1 == Some(true) {
                continue;
            }
            db_annotated += 1;
            if apply {
                m.cutoff_turn1 = Some(true);
                let _ = store.upsert_run_manifest(&m);
            }
        }
    }

    Ok(CutoffAnnotateReport {
        manifests_scanned,
        annotated,
        db_annotated,
        skipped_billing,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cutoff_manifest() -> Value {
        serde_json::json!({
            "run_id": "pdf_openrouter_x_20261008_000000_w3_trial1",
            "model": "openrouter/inclusionai/ling-3.1-flash",
            "task": "pdf",
            "effort": "max",
            "harness": "opencode",
            "status": "failed_tests",
            "pass_rate": 0.0,
            "cutoff_turn1": null
        })
    }

    fn cutoff_console() -> String {
        "[11:52:01] [LIVE] Agent initialized (opencode 2.0.24). Max turns: 100\n\
         [11:52:05] [LIVE] Turn 1 started\n\
         [11:59:19] [OPENCODE] Finished! Tokens: prompt=0, cached=0, completion=0\n\
         [11:59:19] [SETUP] No runnable start.sh or Dockerfile found in ./workspace! Skipping verification.\n"
            .to_string()
    }

    fn empty_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutoff_ws_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_cutoff_signature_matches_measured_shape() {
        let ws = empty_workspace("sig");
        assert!(is_cutoff_trial(&cutoff_manifest(), &cutoff_console(), &ws));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn test_acting_trials_never_match() {
        let ws = empty_workspace("act");
        // Acted: tool calls in the console.
        let mut console = cutoff_console();
        console.push_str("[12:00:00] [ACTION] write server.py\n");
        assert!(!is_cutoff_trial(&cutoff_manifest(), &console, &ws));
        // Produced files (even without parsed tool events).
        std::fs::write(ws.join("server.py"), "x").unwrap();
        assert!(!is_cutoff_trial(&cutoff_manifest(), &cutoff_console(), &ws));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn test_non_cutoff_rows_excluded() {
        let ws = empty_workspace("exc");
        // Billing rejection never got to think — both the pipeline's
        // [BILLING] tag and raw provider stderr shapes exclude.
        let mut billing = cutoff_console();
        billing.push_str("[12:00:00] [BILLING] provider auth rejected (HTTP 401)\n");
        assert!(!is_cutoff_trial(&cutoff_manifest(), &billing, &ws));
        let mut raw = cutoff_console();
        raw.push_str("HTTP/1.1 401 Unauthorized\n");
        assert!(!is_cutoff_trial(&cutoff_manifest(), &raw, &ws));
        // Wrong harness / status / pass rate.
        let mut m = cutoff_manifest();
        m["harness"] = Value::String("omp".to_string());
        assert!(!is_cutoff_trial(&m, &cutoff_console(), &ws));
        let mut m = cutoff_manifest();
        m["status"] = Value::String("completed".to_string());
        assert!(!is_cutoff_trial(&m, &cutoff_console(), &ws));
        let mut m = cutoff_manifest();
        m["pass_rate"] = Value::Number(serde_json::Number::from_f64(83.3).unwrap());
        assert!(!is_cutoff_trial(&m, &cutoff_console(), &ws));
        // Never launched (setup failure, no Turn 1).
        assert!(!is_cutoff_trial(
            &cutoff_manifest(),
            "[11:52:01] [SETUP] network failed\n",
            &ws
        ));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn test_needs_cutoff_check_is_idempotent_and_respects_false() {
        assert!(needs_cutoff_check(&cutoff_manifest()));
        let mut done = cutoff_manifest();
        done["cutoff_turn1"] = Value::Bool(true);
        assert!(!needs_cutoff_check(&done), "re-runs must no-op");
        // Absent field counts as unscanned too (pre-flag rows).
        let mut absent = cutoff_manifest();
        absent.as_object_mut().unwrap().remove("cutoff_turn1");
        assert!(needs_cutoff_check(&absent));
        // Explicit false = human-cleared, never re-touch.
        let mut cleared = cutoff_manifest();
        cleared["cutoff_turn1"] = Value::Bool(false);
        assert!(!needs_cutoff_check(&cleared));
    }
}
