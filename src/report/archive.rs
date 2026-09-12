use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::report::environment::EnvironmentInfo;
use crate::verifier::StageResult;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub run_id: String,
    pub model: String,
    pub task: String,
    pub status: String,
    pub language: String,
    #[serde(default)]
    pub effort: Option<String>,
    pub started_at: String,
    pub completed_at: String,
    pub duration_seconds: f64,
    #[serde(default)]
    pub turns: Option<u32>,
    pub pass_rate: f64,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub stages: Vec<StageResult>,
    pub throughput_req_sec: Option<f64>,
    #[serde(default)]
    pub reference_throughput_req_sec: Option<f64>,
    pub tokens: RunTokenUsage,
    pub cost_usd: f64,
    #[serde(default)]
    pub effective_cost_usd: Option<f64>,
    pub savings_percent: f64,
    pub efficiency_score: f64,
    #[serde(default)]
    pub throughput_score: Option<f64>,
    pub files: Vec<FileInfo>,
    #[serde(default)]
    pub env: Option<EnvironmentInfo>,
    #[serde(default)]
    pub git_commit: Option<String>,
    #[serde(default)]
    pub is_published: Option<bool>,
    #[serde(default)]
    pub method_version: Option<String>,
}

impl RunManifest {
    pub fn effective_cost(&self) -> f64 {
        self.effective_cost_usd.unwrap_or(self.cost_usd)
    }
}

pub struct RunArchiver;

impl RunArchiver {
    pub fn resolve_runs_dir() -> PathBuf {
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
        fs::create_dir_all(&run_dir)?;

        let mut manifest_to_save = manifest.clone();

        // Ensure env is populated
        if manifest_to_save.env.is_none() {
            let env = EnvironmentInfo::detect();
            manifest_to_save.git_commit = Some(env.git_commit.clone());
            manifest_to_save.env = Some(env);
        }

        // 1. Write manifest.json
        let manifest_json = serde_json::to_string_pretty(&manifest_to_save)?;
        fs::write(run_dir.join("manifest.json"), manifest_json)?;

        // 2. Write console.log
        fs::write(run_dir.join("console.log"), console_log)?;

        // 3. Write env.json if available
        if let Some(ref env) = manifest_to_save.env {
            if let Ok(env_json) = serde_json::to_string_pretty(env) {
                let _ = fs::write(run_dir.join("env.json"), env_json);
            }
        }

        // 4. Snapshot candidate workspace files
        let ws_dest = run_dir.join("workspace");
        fs::create_dir_all(&ws_dest)?;
        if workspace_dir.exists() {
            Self::copy_dir_recursive(workspace_dir, &ws_dest)?;
        }

        Ok(run_dir)
    }

    pub fn list_runs(runs_dir: &Path) -> Vec<RunManifest> {
        let mut manifests = Vec::new();
        if let Ok(entries) = fs::read_dir(runs_dir) {
            for entry in entries.flatten() {
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
        let manifest_file = runs_dir.join(clean_id).join("manifest.json");
        let content = fs::read_to_string(manifest_file).ok()?;
        serde_json::from_str::<RunManifest>(&content).ok()
    }

    pub fn get_console_log(runs_dir: &Path, run_id: &str) -> Option<String> {
        let clean_id = Path::new(run_id).file_name()?.to_str()?;
        let log_file = runs_dir.join(clean_id).join("console.log");
        fs::read_to_string(log_file).ok()
    }

    pub fn get_workspace_file(runs_dir: &Path, run_id: &str, filename: &str) -> Option<String> {
        let clean_id = Path::new(run_id).file_name()?.to_str()?;
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
            run_id: "test_run_123".to_string(),
            model: "google/gemini-2.5-flash".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Go".to_string(),
            effort: Some("high".to_string()),
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
            throughput_req_sec: Some(45000.0),
            reference_throughput_req_sec: Some(71000.0),
            tokens: RunTokenUsage {
                prompt_tokens: 10000,
                cached_tokens: 8000,
                completion_tokens: 1500,
                total_tokens: 11500,
            },
            cost_usd: 0.0025,
            effective_cost_usd: Some(0.005),
            savings_percent: 65.0,
            efficiency_score: 400.0,
            throughput_score: Some(907.0),
            files: scanned,
            env: None,
            git_commit: None,
            is_published: None,
            method_version: Some("0.1.0".to_string()),
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
        assert_eq!(loaded.model, "google/gemini-2.5-flash");
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
        let symlink_path = runs_dir.join("test_run_123").join("workspace").join("evil_symlink");
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&outside_file, &symlink_path);
            let escaped = RunArchiver::get_workspace_file(&runs_dir, "test_run_123", "evil_symlink");
            assert!(escaped.is_none(), "Symlink escape outside workspace should be denied");
        }

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
