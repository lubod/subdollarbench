use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::report::environment::EnvironmentInfo;
use crate::verifier::StageResult;

/// How old a staging dir must be before `cleanup_stale_tmp_dirs` will remove
/// it. A live archive stages and renames within seconds; an hour is far beyond
/// any observed archive and far beyond any plausible crash-recovery window, so
/// this only ever removes genuine debris.
pub const STAGING_STALE_AFTER: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunTokenUsage {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// One feedback round of a campaign attempt: the initial try and every
/// repair round, with that round's own verifier results (gui-spec.md §5.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CampaignRound {
    /// 0 = initial, 1.. = repair.
    pub round: u32,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
    pub turns: Option<u32>,
    /// Equivalent compute cost of this round's own tokens; the attempt's
    /// `effective_cost_usd` stays the authoritative number.
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    /// Per-round verifier results, same shape as `stages`.
    #[serde(default)]
    pub stage_results: Vec<StageResult>,
    /// Candidate log tail when the server did not come up (the `no_start`
    /// classifier's evidence).
    #[serde(default)]
    pub candidate_logs_tail: Option<String>,
    /// R26 R23-7: why this session ended (dollars | turns | wall | ready |
    /// crash), recorded per session — initial and every repair — not only
    /// for the attempt's last session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub run_id: String,
    pub model: String,
    pub task: String,
    pub status: String,
    pub language: String,
    #[serde(default)]
    pub effort: Option<String>,
    /// Effective effort as negotiated (harness-interpreted): OMP records
    /// the session's thinking level (CLI downgrades included), OpenCode
    /// records `#<variant>` or `prose-only`. Requested effort is `effort`;
    /// this says what ran. None = unknown (pre-field row, no session).
    #[serde(default)]
    pub effort_effective: Option<String>,
    pub started_at: String,
    pub completed_at: String,
    pub duration_seconds: f64,
    #[serde(default)]
    pub turns: Option<u32>,
    pub pass_rate: f64,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub stages: Vec<StageResult>,
    /// Campaign rounds: the initial verification (0) and each repair round,
    /// in order. Empty for single-shot runs and for manifests written
    /// before this field existed (the GUI derives a round from `stages`).
    #[serde(default)]
    pub rounds: Vec<CampaignRound>,
    /// Advisory robustness probes (redis/http): reported, never scored.
    /// Empty on older rows and non-probed tasks.
    #[serde(default)]
    pub robustness: Vec<StageResult>,
    pub throughput_req_sec: Option<f64>,
    #[serde(default)]
    pub reference_throughput_req_sec: Option<f64>,
    /// Where the reference came from: "batch" (shared pre-batch median),
    /// "live" (per-trial measurement, single-trial runs), or None (pinned fallback).
    #[serde(default)]
    pub baseline_source: Option<String>,
    /// Post-batch bracket result. None per-trial; set on merged parent records.
    #[serde(default)]
    pub ref_drift: Option<bool>,
    /// Content-addressed reference image (e.g. redis@sha256:…). None = legacy
    /// run from before digest pinning; such runs form their own regime.
    #[serde(default)]
    pub ref_image: Option<String>,
    /// Whether this run's agent container was network-isolated with the
    /// allowlist proxy as its only egress. `Some(true)` means the run network
    /// was created --internal and the sidecar was up; `Some(false)` means the
    /// filter was off and the agent had unrestricted egress. None = archived
    /// before this field existed, when enforcement was never recorded.
    #[serde(default)]
    pub egress_enforced: Option<bool>,
    pub tokens: RunTokenUsage,
    pub cost_usd: f64,
    /// R24 R2: always `"snapshot"` — the committed list-price table.
    /// The live key delta is in `live_spend_delta_usd` for audit only.
    #[serde(default)]
    pub cost_source: Option<String>,
    /// Token-priced API cost from the committed snapshot, kept alongside
    /// `cost_usd`.
    #[serde(default)]
    pub cost_formula_usd: Option<f64>,
    /// R24 R2: the live key-usage delta, recorded for audit only. The
    /// scored cost is always the snapshot formula.
    #[serde(default)]
    pub live_spend_delta_usd: Option<f64>,
    #[serde(default)]
    pub effective_cost_usd: Option<f64>,
    pub savings_percent: f64,
    pub efficiency_score: f64,
    #[serde(default)]
    pub throughput_score: Option<f64>,
    pub files: Vec<FileInfo>,
    /// Canary echoes found in the candidate workspace as
    /// `(relative_path, guid)`. Advisory only: a model quoting the brief it
    /// was handed is reproducing benchmark material — evidence to report,
    /// never a verdict. `None` = clean scan or pre-canary row.
    #[serde(default)]
    pub canary_echoed: Option<Vec<(String, String)>>,
    /// Turn-1 reasoning cutoff accepted: the trial exited clean with zero
    /// tool calls and an empty workspace (model thought itself into
    /// `finish: length` before acting). Advisory annotation, never a
    /// rescoring — the 0% stands. Set post-hoc by `annotate-cutoff`
    /// (repeatable after any wave, old and new rows alike); never written
    /// live by the pipeline. `None` = not a cutoff or not yet scanned.
    #[serde(default)]
    pub cutoff_turn1: Option<bool>,
    #[serde(default)]
    pub env: Option<EnvironmentInfo>,
    #[serde(default)]
    pub git_commit: Option<String>,
    /// Rule-set fingerprint (see report::rules). Only comparable within one version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_version: Option<String>,
    /// R28-1: the SQLite oracle and cap policy actually recorded when this
    /// run was graded. None on old rows; readers show `—`/no label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_price_basis: Option<String>,
    /// R36 F1: the sandbox image ID this run used. None on old rows and
    /// eval-only runs; the Board shows `—`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_image: Option<String>,
    /// Trial count for consolidated parent manifests (None on single-trial records).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_trials: Option<u32>,
    /// Pass@1 across trials for consolidated parents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_1: Option<f64>,
    /// Wilson 95% lower bound for P@1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_1_lo: Option<f64>,
    /// Wilson 95% upper bound for P@1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_1_hi: Option<f64>,
    /// Trial-stability verdict (stable | watch | unstable) for consolidated parents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stability: Option<String>,
    /// Per-trial pass rates for consolidated parents (trial order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trial_pass_rates: Vec<f64>,
    /// Evaluation protocol (e.g. "sequential_cap" or "fixed_n")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// True if any trial achieved 100% stage pass rate
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solved: Option<bool>,
    /// 1-indexed trial number that first achieved 100% pass (None if unsolved)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solved_at: Option<u32>,
    /// Highest stage pass rate achieved across trials
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_stage_score: Option<f64>,
    /// Whether this run succeeded via feedback repair (true) vs initial attempt (false).
    /// None = unsolved or unknown; an attempted-but-unconverted repair reads
    /// None here (see `repair_attempted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repaired: Option<bool>,
    /// True when ≥1 repair round ran (trial = this trial; parent = any
    /// scored trial). False = feedback enabled but unused; None = disabled
    /// or unknown (pre-field row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_attempted: Option<bool>,
    /// Feedback repair rounds used (trial = this trial; parent = max across
    /// scored trials). None = disabled/unknown; Some(0) = enabled but unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_rounds: Option<u32>,
    #[serde(default)]
    pub is_published: Option<bool>,
    #[serde(default)]
    pub method_version: Option<String>,
    /// Sampling params at run time (None = provider default; OMP passes
    /// model defaults through, so most runs record None = reproducible-by-default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// SHA-256 of tasks/<task>/prompt.md at run time (reproducibility).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_sha: Option<String>,
    /// OMP harness version that ran this trial (digest-pinned Dockerfile;
    /// unlocks every future comparison).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omp_version: Option<String>,
    /// Agent harness that ran this trial ("omp" | "opencode"; None = legacy OMP rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Harness version (OMP sandbox version or opencode pin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
}

impl RunManifest {
    pub fn effective_cost(&self) -> f64 {
        self.effective_cost_usd.unwrap_or(self.cost_usd)
    }
}

pub struct RunArchiver;

impl RunArchiver {
    /// Where the public portal is written. `SDB_PORTAL_DIR` overrides
    /// (`isolate_test_runs_dir` points it at a temp dir so a `cargo test`
    /// run can never overwrite the served board — round-12 P1).
    pub fn resolve_portal_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("SDB_PORTAL_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        crate::config::get_repo_root().join("portal")
    }

    pub fn resolve_runs_dir() -> PathBuf {
        // Tests (and tools) point this at a temp dir via SDB_RUNS_DIR: the
        // real runs/ directory holds live cells whose locks and campaign
        // dirs must never be touched (round-8 finding 1).
        if let Ok(dir) = std::env::var("SDB_RUNS_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        let root = crate::config::get_repo_root();
        let runs = root.join("runs");
        if runs.exists() {
            runs
        } else if Path::new("./runs").exists() {
            PathBuf::from("./runs")
        } else {
            runs
        }
    }

    pub fn archive_run(
        runs_dir: &Path,
        manifest: &RunManifest,
        workspace_dir: &Path,
        console_log: &str,
    ) -> Result<PathBuf> {
        let clean_id = Path::new(&manifest.run_id)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&manifest.run_id);

        let run_dir = runs_dir.join(clean_id);
        // Write into `<id>.tmp` and rename into place so a crash mid-archive
        // never leaves a partial dir that looks like a completed run.
        // Best-effort cleanup of stale `.tmp` leftovers from a previous crash
        // (before creating our own staging dir, which cleanup would remove).
        Self::cleanup_stale_tmp_dirs(runs_dir);
        let tmp_dir = runs_dir.join(format!("{clean_id}.tmp"));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir)?;

        let mut manifest_to_save = manifest.clone();

        // Ensure env is populated
        if manifest_to_save.env.is_none() {
            let env = EnvironmentInfo::detect();
            manifest_to_save.git_commit = Some(env.git_commit.clone());
            manifest_to_save.env = Some(env);
        }

        // 1. Write manifest.json
        let manifest_json = serde_json::to_string_pretty(&manifest_to_save)?;
        fs::write(tmp_dir.join("manifest.json"), manifest_json)?;

        // 2. Write console.log (secret-scrubbed: the agent env-dumps the
        // live OPENROUTER_API_KEY into the log stream, and raw runs/ dirs
        // previously held real keys).
        let scrubbed = Self::scrub_secrets(console_log);
        fs::write(tmp_dir.join("console.log"), scrubbed)?;

        // 3. Write env.json if available
        if let Some(ref env) = manifest_to_save.env {
            if let Ok(env_json) = serde_json::to_string_pretty(env) {
                let _ = fs::write(tmp_dir.join("env.json"), env_json);
            }
        }

        // 4. Snapshot candidate workspace files
        let ws_dest = tmp_dir.join("workspace");
        fs::create_dir_all(&ws_dest)?;
        if workspace_dir.exists() {
            Self::copy_dir_recursive(workspace_dir, &ws_dest)?;
        }

        // 5. Publish atomically: a pre-existing complete dir is replaced only
        // after every byte above landed on disk.
        let _ = fs::remove_dir_all(&run_dir);
        fs::rename(&tmp_dir, &run_dir)?;

        Ok(run_dir)
    }

    pub fn list_runs(runs_dir: &Path) -> Vec<RunManifest> {
        let mut manifests = Vec::new();
        if let Ok(entries) = fs::read_dir(runs_dir) {
            for entry in entries.flatten() {
                if Self::is_staging_dir(&entry.path()) {
                    continue;
                }
                let manifest_file = entry.path().join("manifest.json");
                if manifest_file.exists() {
                    if let Ok(content) = fs::read_to_string(&manifest_file) {
                        if let Ok(m) = serde_json::from_str::<RunManifest>(&content) {
                            manifests.push(m);
                        }
                    }
                }
            }
        }
        manifests.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        manifests
    }

    pub fn get_run(runs_dir: &Path, run_id: &str) -> Option<RunManifest> {
        let clean_id = Path::new(run_id).file_name()?.to_str()?;
        if clean_id.ends_with(".tmp") || clean_id.starts_with('.') {
            return None;
        }
        let manifest_file = runs_dir.join(clean_id).join("manifest.json");
        let content = fs::read_to_string(manifest_file).ok()?;
        serde_json::from_str::<RunManifest>(&content).ok()
    }

    /// True for in-progress/stale staging dirs (`.tmp` suffix, dot dirs)
    /// that readers must never treat as completed runs.
    fn is_staging_dir(path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        name.ends_with(".tmp") || name.starts_with('.')
    }

    /// Remove abandoned `<id>.tmp` leftovers from crashed archives. Called on
    /// every archive and from `migrate` at startup.
    ///
    /// Bounded by age, deliberately. `archive_run` stages into `<id>.tmp` and
    /// renames it into place within seconds, and a wave runs trials in
    /// parallel - so a sweep that removed *every* staging dir would delete a
    /// sibling trial's in-progress archive mid-write, failing it or leaving a
    /// directory that renames into place with files missing. Anything younger
    /// than [`STAGING_STALE_AFTER`] belongs to a live writer; only older ones
    /// are crash debris.
    pub fn cleanup_stale_tmp_dirs(runs_dir: &Path) {
        Self::cleanup_stale_tmp_dirs_older_than(runs_dir, STAGING_STALE_AFTER);
    }

    /// Same sweep with the age bound supplied, so a test can declare every
    /// existing staging dir stale without backdating the filesystem.
    pub fn cleanup_stale_tmp_dirs_older_than(runs_dir: &Path, stale_after: Duration) {
        let now = SystemTime::now();
        if let Ok(entries) = fs::read_dir(runs_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_dir() || !Self::is_staging_dir(&p) {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok());
                match modified {
                    // Unreadable mtime: leave it. Guessing wrong here deletes
                    // someone's live archive.
                    None => continue,
                    Some(age) if age < stale_after => continue,
                    Some(_) => {
                        let _ = fs::remove_dir_all(&p);
                    }
                }
            }
        }
    }

    /// Strip secrets from a log stream before it touches disk. Reuses the
    /// portal's patterns so archive + export redact identically. Compiles
    /// regexes per call — archive is once-per-run, never hot.
    pub fn scrub_secrets(text: &str) -> String {
        let patterns = crate::report::portal::PortalExporter::secret_patterns();
        crate::report::portal::PortalExporter::redact(text, &patterns)
    }

    pub fn get_console_log(runs_dir: &Path, run_id: &str) -> Option<String> {
        let clean_id = Path::new(run_id).file_name()?.to_str()?;
        if clean_id.ends_with(".tmp") || clean_id.starts_with('.') {
            return None;
        }
        let log_file = runs_dir.join(clean_id).join("console.log");
        fs::read_to_string(log_file).ok()
    }

    pub fn get_workspace_file(runs_dir: &Path, run_id: &str, filename: &str) -> Option<String> {
        let clean_id = Path::new(run_id).file_name()?.to_str()?;
        if clean_id.ends_with(".tmp") || clean_id.starts_with('.') {
            return None;
        }
        if filename.contains("..") || filename.starts_with('/') {
            return None;
        }
        let ws_dir = runs_dir.join(clean_id).join("workspace");
        let file_path = ws_dir.join(filename);
        if let (Ok(can_ws), Ok(can_file)) = (ws_dir.canonicalize(), file_path.canonicalize()) {
            if !can_file.starts_with(&can_ws) {
                return None;
            }
            fs::read_to_string(can_file).ok()
        } else {
            None
        }
    }

    pub fn scan_workspace_files(workspace_dir: &Path) -> Vec<FileInfo> {
        let mut files = Vec::new();
        Self::scan_recursive(workspace_dir, "", &mut files);
        files.sort_by(|a, b| a.name.cmp(&b.name));
        files
    }

    fn scan_recursive(dir: &Path, rel_prefix: &str, out: &mut Vec<FileInfo>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let file_name = entry.file_name();
                let name_str = file_name.to_string_lossy();
                if name_str == ".git" || name_str == "target" || name_str == "node_modules" {
                    continue;
                }
                let rel_name = if rel_prefix.is_empty() {
                    name_str.to_string()
                } else {
                    format!("{}/{}", rel_prefix, name_str)
                };
                if p.is_file() {
                    if let Ok(meta) = entry.metadata() {
                        out.push(FileInfo {
                            name: rel_name,
                            size_bytes: meta.len(),
                        });
                    }
                } else if p.is_dir() {
                    Self::scan_recursive(&p, &rel_name, out);
                }
            }
        }
    }

    pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
        if !src.exists() {
            return Ok(());
        }
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            let dest_path = dst.join(entry.file_name());
            if ft.is_dir() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s == ".git" || s == "target" || s == "node_modules" {
                    continue;
                }
                fs::create_dir_all(&dest_path)?;
                Self::copy_dir_recursive(&entry.path(), &dest_path)?;
            } else {
                let _ = fs::copy(entry.path(), dest_path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_copy_dir_recursive_creates_dest_root() {
        let base = std::env::temp_dir().join(format!("test_copy_dst_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src_ws");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("main.go"), "package main").unwrap();
        std::fs::write(src.join("nested").join("a.txt"), "hi").unwrap();
        let dst = base.join("parent").join("workspace");
        assert!(!dst.exists());
        RunArchiver::copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("main.go")).unwrap(),
            "package main"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("nested").join("a.txt")).unwrap(),
            "hi"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    use super::*;

    #[test]
    fn test_archive_lifecycle() {
        let temp_dir = std::env::temp_dir().join(format!("test_archive_{}", std::process::id()));
        let runs_dir = temp_dir.join("runs");
        let ws_dir = temp_dir.join("workspace");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&ws_dir).unwrap();

        // Create sample workspace files (nested)
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::write(ws_dir.join("src/main.go"), "package main\nfunc main() {}\n").unwrap();
        fs::write(ws_dir.join("start.sh"), "#!/bin/bash\ngo run src/main.go\n").unwrap();

        // Create a symlink to test symlink skipping
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(ws_dir.join("start.sh"), ws_dir.join("symlink.sh"));

        let scanned = RunArchiver::scan_workspace_files(&ws_dir);
        assert!(scanned.iter().any(|f| f.name == "src/main.go"));

        let manifest = RunManifest {
            rounds: Vec::new(),
            run_id: "test_run_123".to_string(),
            model: "openrouter/meta/muse-spark-1.3-contributor".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Go".to_string(),
            effort: Some("high".to_string()),
            effort_effective: None,
            harness: None,
            started_at: "2026-09-04T06:30:00Z".to_string(),
            completed_at: "2026-09-04T06:31:00Z".to_string(),
            duration_seconds: 60.0,
            turns: Some(5),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            stages: vec![StageResult {
                stage: 1,
                name: "Stage 1: PING & ECHO".to_string(),
                passed: true,
                error: None,
            }],
            robustness: Vec::new(),
            throughput_req_sec: Some(45000.0),
            reference_throughput_req_sec: Some(71000.0),
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            egress_enforced: None,
            tokens: RunTokenUsage {
                prompt_tokens: 10000,
                cached_tokens: 8000,
                completion_tokens: 1500,
                total_tokens: 11500,
            },
            cost_usd: 0.0025,
            cost_source: None,
            cost_formula_usd: None,
            live_spend_delta_usd: None,
            effective_cost_usd: Some(0.005),
            savings_percent: 65.0,
            efficiency_score: 400.0,
            throughput_score: Some(907.0),
            files: scanned,
            canary_echoed: None,
            cutoff_turn1: None,
            env: None,
            git_commit: None,
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            trial_pass_rates: Vec::new(),
            is_published: None,
            method_version: Some("0.1.0".to_string()),
            temperature: None,
            top_p: None,
            seed: None,
            prompt_sha: None,
            omp_version: None,
            harness_version: None,
            rules_version: None,
            sqlite_version: None,
            cap_price_basis: None,
            sandbox_image: None,
            protocol: None,
            solved: None,
            solved_at: None,
            best_stage_score: None,
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        };

        let console_log = "[06:30:00] [INIT] Starting test run\n[06:31:00] [DONE] Finished!\n";
        let archived =
            RunArchiver::archive_run(&runs_dir, &manifest, &ws_dir, console_log).unwrap();
        assert!(archived.exists());

        // Verify listing
        let runs = RunArchiver::list_runs(&runs_dir);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "test_run_123");
        assert_eq!(runs[0].effort.as_deref(), Some("high"));

        // Verify get_run
        let loaded = RunArchiver::get_run(&runs_dir, "test_run_123").unwrap();
        assert_eq!(loaded.model, "openrouter/meta/muse-spark-1.3-contributor");
        assert_eq!(loaded.pass_rate, 100.0);
        assert!(loaded.env.is_some());

        // Verify get_console_log
        let log = RunArchiver::get_console_log(&runs_dir, "test_run_123").unwrap();
        assert!(log.contains("[INIT] Starting test run"));

        // Verify get_workspace_file with subpath
        let code =
            RunArchiver::get_workspace_file(&runs_dir, "test_run_123", "src/main.go").unwrap();
        assert!(code.contains("package main"));

        // Verify symlink escape protection
        let outside_file = temp_dir.join("secret.txt");
        let _ = fs::write(&outside_file, "super_secret_token");
        let symlink_path = runs_dir
            .join("test_run_123")
            .join("workspace")
            .join("evil_symlink");
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&outside_file, &symlink_path);
            let escaped =
                RunArchiver::get_workspace_file(&runs_dir, "test_run_123", "evil_symlink");
            assert!(
                escaped.is_none(),
                "Symlink escape outside workspace should be denied"
            );
        }

        let _ = fs::remove_dir_all(&temp_dir);
    }

    /// The sweep used to delete *every* staging dir. A wave runs trials in
    /// parallel and each stages into `<id>.tmp`, so one trial's cleanup would
    /// remove a sibling's in-progress archive mid-write - failing it, or letting
    /// a directory rename into place with files missing. Age-bounding is what
    /// separates crash debris from a live writer.
    #[test]
    fn test_cleanup_preserves_a_live_staging_dir() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_archive_race_{}", std::process::id()));
        let runs_dir = temp_dir.join("runs");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&runs_dir).unwrap();

        // A sibling trial, mid-archive, just created its staging dir.
        let live = runs_dir.join("sibling_trial.tmp");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("manifest.json"), "{\"in_progress\":true}").unwrap();

        // Production sweep: nothing is an hour old, so the live dir survives.
        RunArchiver::cleanup_stale_tmp_dirs(&runs_dir);
        assert!(
            live.exists(),
            "a live staging dir must survive another trial's cleanup"
        );
        assert!(live.join("manifest.json").exists(), "and keep its contents");

        // Debris of any age is still removed when the bound allows it.
        RunArchiver::cleanup_stale_tmp_dirs_older_than(&runs_dir, Duration::ZERO);
        assert!(!live.exists(), "genuine debris must still be swept");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_partial_archive_invisible_to_readers() {
        // A crashed archive leaves only `runs/<id>.tmp`; list_runs/get_run
        // must not see it, and the next successful archive (or cleanup) heals.
        let temp_dir =
            std::env::temp_dir().join(format!("test_archive_crash_{}", std::process::id()));
        let runs_dir = temp_dir.join("runs");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&runs_dir).unwrap();

        let stale = runs_dir.join("crashed_run.tmp");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("manifest.json"), "{partial").unwrap();

        assert!(RunArchiver::list_runs(&runs_dir).is_empty());
        assert!(RunArchiver::get_run(&runs_dir, "crashed_run").is_none());
        // Direct staging-id reads are refused even though the .tmp dir exists.
        assert!(RunArchiver::get_run(&runs_dir, "crashed_run.tmp").is_none());
        assert!(RunArchiver::get_console_log(&runs_dir, "crashed_run.tmp").is_none());
        assert!(RunArchiver::get_workspace_file(&runs_dir, "crashed_run.tmp", "x").is_none());

        // Declared stale explicitly: the production sweep is age-bounded so a
        // sibling trial's live staging dir is not deleted mid-write.
        RunArchiver::cleanup_stale_tmp_dirs_older_than(&runs_dir, Duration::ZERO);
        assert!(!stale.exists());

        // A `.tmp` dir for a live id must not shadow the complete archive.
        let ws = temp_dir.join("ws");
        fs::create_dir_all(&ws).unwrap();
        let manifest = RunManifest {
            run_id: "live_run".to_string(),
            model: "m".to_string(),
            task: "t".to_string(),
            status: "completed".to_string(),
            language: "Go".to_string(),
            effort: None,
            effort_effective: None,
            harness: None,
            started_at: "2026-09-25T00:00:00Z".to_string(),
            completed_at: "2026-09-25T00:01:00Z".to_string(),
            duration_seconds: 1.0,
            turns: None,
            pass_rate: 100.0,
            passed_stages: 1,
            total_stages: 1,
            stages: vec![],
            rounds: Vec::new(),
            robustness: Vec::new(),
            throughput_req_sec: None,
            reference_throughput_req_sec: None,
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            egress_enforced: None,
            tokens: RunTokenUsage {
                prompt_tokens: 1,
                cached_tokens: 0,
                completion_tokens: 1,
                total_tokens: 2,
            },
            cost_usd: 0.0,
            cost_source: None,
            cost_formula_usd: None,
            live_spend_delta_usd: None,
            effective_cost_usd: None,
            savings_percent: 0.0,
            efficiency_score: 0.0,
            throughput_score: None,
            files: vec![],
            canary_echoed: None,
            cutoff_turn1: None,
            env: None,
            git_commit: None,
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            trial_pass_rates: vec![],
            is_published: None,
            method_version: None,
            temperature: None,
            top_p: None,
            seed: None,
            prompt_sha: None,
            omp_version: None,
            harness_version: None,
            rules_version: None,
            sqlite_version: None,
            cap_price_basis: None,
            sandbox_image: None,
            protocol: None,
            solved: None,
            solved_at: None,
            best_stage_score: None,
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        };
        RunArchiver::archive_run(&runs_dir, &manifest, &ws, "log").unwrap();
        assert!(!runs_dir.join("live_run.tmp").exists());
        assert_eq!(RunArchiver::list_runs(&runs_dir).len(), 1);
        assert!(RunArchiver::get_run(&runs_dir, "live_run").is_some());
        // Re-archive replaces the complete dir (no duplicate, no .tmp left).
        RunArchiver::archive_run(&runs_dir, &manifest, &ws, "log2").unwrap();
        assert!(!runs_dir.join("live_run.tmp").exists());
        assert_eq!(RunArchiver::list_runs(&runs_dir).len(), 1);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
