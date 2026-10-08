use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub total_memory: String,
    pub docker_version: String,
    pub rust_version: String,
    pub python_version: String,
    pub node_version: String,
    pub git_commit: String,
    pub git_branch: String,
    pub timestamp: String,
}

impl EnvironmentInfo {
    pub fn detect() -> Self {
        let os = Self::detect_os();
        let kernel = Self::cmd_output("uname", &["-s", "-r"]);
        let arch = Self::cmd_output("uname", &["-m"]);
        let (cpu_model, cpu_cores) = Self::detect_cpu();
        let total_memory = Self::detect_memory();
        let docker_version = Self::cmd_output("docker", &["--version"]);
        let rust_version = Self::detect_rust();
        let python_version = Self::cmd_output("python3", &["--version"]);
        let node_version = Self::cmd_output("node", &["--version"]);
        let git_commit = Self::detect_git_commit();
        let git_branch = Self::detect_git_branch();
        let timestamp = Utc::now().to_rfc3339();

        Self {
            os,
            kernel,
            arch,
            cpu_model,
            cpu_cores,
            total_memory,
            docker_version,
            rust_version,
            python_version,
            node_version,
            git_commit,
            git_branch,
            timestamp,
        }
    }

    fn detect_os() -> String {
        if let Ok(content) = fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(stripped) = line.strip_prefix("PRETTY_NAME=") {
                    return stripped.trim_matches('"').to_string();
                }
            }
        }
        std::env::consts::OS.to_string()
    }

    fn detect_cpu() -> (String, u32) {
        let mut model = "Unknown CPU".to_string();
        if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
            for line in content.lines() {
                if line.starts_with("model name") {
                    if let Some(val) = line.split(':').nth(1) {
                        model = val.trim().to_string();
                        break;
                    }
                }
            }
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);

        (model, cores)
    }

    fn detect_memory() -> String {
        if let Ok(content) = fs::read_to_string("/proc/meminfo") {
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = val.parse::<u64>() {
                            let gb = (kb as f64) / (1024.0 * 1024.0);
                            return format!("{:.1} GiB", gb);
                        }
                    }
                }
            }
        }
        "Unknown".to_string()
    }

    /// Cargo home for the login shell that owns the repo. Systemd runs
    /// as `ubuntu` with a minimal PATH, so `rustc` alone resolves nothing;
    /// the old code fell back to `su - ubuntu`, which fails as non-root and
    /// floods auth.log while recording rust_version "N/A". Probe explicit
    /// paths instead — no privilege escalation involved.
    fn cargo_bin(name: &str) -> String {
        let rel = format!(".cargo/bin/{name}");
        for base in [
            std::env::var("HOME").unwrap_or_default(),
            "/home/ubuntu".to_string(),
        ] {
            if base.is_empty() {
                continue;
            }
            let p = std::path::Path::new(&base).join(&rel);
            if p.is_file() {
                return p.to_string_lossy().to_string();
            }
        }
        name.to_string()
    }

    fn detect_rust() -> String {
        let direct = Self::cmd_output("rustc", &["--version"]);
        if direct != "N/A" && !direct.is_empty() {
            direct
        } else {
            Self::cmd_output(&Self::cargo_bin("rustc"), &["--version"])
        }
    }

    fn detect_git_commit() -> String {
        let out = Self::cmd_output("git", &["rev-parse", "--short", "HEAD"]);
        if out != "N/A" && !out.is_empty() {
            return out;
        }
        // Same no-su discipline: run git in-process with the repo as cwd.
        let repo = crate::config::get_repo_root();
        match Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(&repo)
            .output()
        {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if o.status.success() && !s.is_empty() {
                    s
                } else {
                    "N/A".to_string()
                }
            }
            Err(_) => "N/A".to_string(),
        }
    }

    fn detect_git_branch() -> String {
        let out = Self::cmd_output("git", &["rev-parse", "--abbrev-ref", "HEAD"]);
        if out != "N/A" && !out.is_empty() {
            return out;
        }
        let repo = crate::config::get_repo_root();
        match Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&repo)
            .output()
        {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if o.status.success() && !s.is_empty() {
                    s
                } else {
                    "N/A".to_string()
                }
            }
            Err(_) => "N/A".to_string(),
        }
    }

    fn cmd_output(cmd: &str, args: &[&str]) -> String {
        match Command::new(cmd).args(args).output() {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            }
            _ => "N/A".to_string(),
        }
    }

    pub fn to_markdown_table(&self) -> String {
        format!(
            "| Component | Specification |\n\
             |:---|:---|\n\
             | **Operating System** | {} |\n\
             | **Kernel & Architecture** | {} ({}) |\n\
             | **CPU Model** | {} ({} vCPUs) |\n\
             | **System Memory** | {} |\n\
             | **Docker Runtime** | {} |\n\
             | **Rust Version** | {} |\n\
             | **Python Version** | {} |\n\
             | **Node.js Version** | {} |\n\
             | **Git Baseline** | Commit `{}` (branch `{}`) |\n\
             | **Benchmark Captured** | {} |",
            self.os,
            self.kernel,
            self.arch,
            self.cpu_model,
            self.cpu_cores,
            self.total_memory,
            self.docker_version,
            self.rust_version,
            self.python_version,
            self.node_version,
            self.git_commit,
            self.git_branch,
            self.timestamp
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_environment() {
        let env = EnvironmentInfo::detect();
        assert!(!env.os.is_empty());
        assert!(!env.kernel.is_empty());
        assert!(!env.arch.is_empty());
        assert!(env.cpu_cores > 0);
        assert!(!env.total_memory.is_empty());
        assert!(!env.timestamp.is_empty());

        let md = env.to_markdown_table();
        assert!(md.contains("Operating System"));
        assert!(md.contains("Kernel & Architecture"));
        assert!(md.contains("CPU Model"));
        assert!(md.contains("System Memory"));
        assert!(md.contains("Git Baseline"));

        let serialized = serde_json::to_string(&env).unwrap();
        let deserialized: EnvironmentInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.os, env.os);
        assert_eq!(deserialized.cpu_cores, env.cpu_cores);
    }

    #[test]
    fn test_cmd_output_success_and_failure() {
        let out = EnvironmentInfo::cmd_output("echo", &["hello_subdollar"]);
        assert_eq!(out, "hello_subdollar");

        let err = EnvironmentInfo::cmd_output("nonexistent_command_xyz_123", &[]);
        assert_eq!(err, "N/A");
    }

    #[test]
    fn test_internal_detect_helpers() {
        assert!(!EnvironmentInfo::detect_os().is_empty());
        let (cpu, cores) = EnvironmentInfo::detect_cpu();
        assert!(!cpu.is_empty());
        assert!(cores > 0);
        assert!(!EnvironmentInfo::detect_memory().is_empty());
        assert!(!EnvironmentInfo::detect_rust().is_empty());
        assert!(!EnvironmentInfo::detect_git_commit().is_empty());
        assert!(!EnvironmentInfo::detect_git_branch().is_empty());
    }

    #[test]
    fn test_no_privilege_escalation_paths() {
        // Regression: detection must never shell out to `su` (PAM flood
        // as non-root + "N/A" manifests). Explicit cargo-bin probing only.
        let p = EnvironmentInfo::cargo_bin("rustc");
        assert!(p.ends_with(".cargo/bin/rustc") || p == "rustc");
        let r = EnvironmentInfo::detect_rust();
        assert!(!r.is_empty() && r != "N/A", "host has rustc: {r}");
        assert!(!EnvironmentInfo::detect_git_commit().is_empty());
    }
}
