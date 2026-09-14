use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::RwLock;

static STATUS_CACHE: LazyLock<Arc<RwLock<Option<(OmpVersionStatus, std::time::Instant)>>>> =
    LazyLock::new(|| Arc::new(RwLock::new(None)));
static IS_UPDATING: AtomicBool = AtomicBool::new(false);

const CACHE_TTL_SECS: u64 = 600; // 10 minutes

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OmpVersionStatus {
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub up_to_date: bool,
    pub image_exists: bool,
    pub is_updating: bool,
    pub last_error: Option<String>,
}

impl Default for OmpVersionStatus {
    fn default() -> Self {
        Self {
            installed_version: None,
            latest_version: None,
            up_to_date: false,
            image_exists: false,
            is_updating: false,
            last_error: None,
        }
    }
}

#[derive(Deserialize)]
struct NpmLatestResponse {
    version: String,
}

pub fn parse_omp_version(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix("omp/").unwrap_or(s);
    let s = s.strip_prefix('v').unwrap_or(s);
    s.trim().to_string()
}

pub async fn fetch_latest_npm_version() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("subdollarbench-updater/0.2.0")
        .build()
        .context("Failed to create HTTP client")?;

    let resp = client
        .get("https://registry.npmjs.org/@oh-my-pi/pi-coding-agent/latest")
        .send()
        .await
        .context("Failed to query npm registry for @oh-my-pi/pi-coding-agent")?;

    if !resp.status().is_success() {
        return Err(anyhow!("npm registry returned HTTP {}", resp.status()));
    }

    let parsed: NpmLatestResponse = resp
        .json()
        .await
        .context("Failed to parse npm registry response JSON")?;

    Ok(parsed.version.trim().to_string())
}

pub fn check_image_exists() -> bool {
    Command::new("docker")
        .args(["image", "inspect", "subdollar-sandbox"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn get_installed_sandbox_version() -> Option<String> {
    let output = Command::new("docker")
        .args(["run", "--rm", "subdollar-sandbox", "omp", "--version"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_omp_version(&raw);
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

pub async fn check_status(force_refresh: bool) -> OmpVersionStatus {
    if !force_refresh {
        let cache = STATUS_CACHE.read().await;
        if let Some((ref status, ref timestamp)) = *cache {
            if timestamp.elapsed().as_secs() < CACHE_TTL_SECS {
                let mut s = status.clone();
                s.is_updating = IS_UPDATING.load(Ordering::SeqCst);
                return s;
            }
        }
    }

    let image_exists = check_image_exists();
    let installed = if image_exists {
        get_installed_sandbox_version()
    } else {
        None
    };

    let mut last_error = None;
    let latest = match fetch_latest_npm_version().await {
        Ok(v) => Some(v),
        Err(e) => {
            last_error = Some(e.to_string());
            None
        }
    };

    let up_to_date = match (&installed, &latest) {
        (Some(inst), Some(lat)) => inst == lat,
        _ => false,
    };

    let status = OmpVersionStatus {
        installed_version: installed,
        latest_version: latest,
        up_to_date,
        image_exists,
        is_updating: IS_UPDATING.load(Ordering::SeqCst),
        last_error,
    };

    let mut cache = STATUS_CACHE.write().await;
    *cache = Some((status.clone(), std::time::Instant::now()));

    status
}

pub async fn update_sandbox_image(target_version: Option<&str>) -> Result<OmpVersionStatus> {
    if IS_UPDATING.swap(true, Ordering::SeqCst) {
        return Err(anyhow!("Sandbox update is already in progress"));
    }

    struct ResetOnDrop;
    impl Drop for ResetOnDrop {
        fn drop(&mut self) {
            IS_UPDATING.store(false, Ordering::SeqCst);
        }
    }
    let _guard = ResetOnDrop;

    let target_ver = match target_version {
        Some(v) => v.to_string(),
        None => match fetch_latest_npm_version().await {
            Ok(v) => v,
            Err(_) => "latest".to_string(),
        },
    };

    let root = crate::config::get_repo_root();
    let dockerfile = root.join("Dockerfile.sandbox");
    if !dockerfile.exists() {
        return Err(anyhow!(
            "Dockerfile.sandbox not found at {}",
            dockerfile.display()
        ));
    }

    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("build")
        .arg("-t")
        .arg("subdollar-sandbox")
        .arg("-f")
        .arg(&dockerfile)
        .arg("--build-arg")
        .arg(format!("OMP_VERSION={}", target_ver))
        .arg(&root);

    let output = cmd
        .output()
        .await
        .context("Failed to run docker build for subdollar-sandbox")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker build failed: {}", stderr));
    }

    // Force refresh the status cache
    let status = check_status(true).await;
    Ok(status)
}

pub async fn auto_update_if_needed() -> Result<()> {
    let auto_update_env = std::env::var("SUBDOLLAR_AUTO_UPDATE_OMP")
        .unwrap_or_else(|_| "true".to_string())
        .to_lowercase();

    if auto_update_env == "false" || auto_update_env == "0" || auto_update_env == "off" {
        return Ok(());
    }

    let status = check_status(false).await;

    if !status.image_exists {
        println!("[OMP Automation] subdollar-sandbox image not found. Building automatically...");
        update_sandbox_image(None).await?;
        println!("[OMP Automation] subdollar-sandbox built successfully.");
        return Ok(());
    }

    if let (Some(installed), Some(latest)) = (&status.installed_version, &status.latest_version) {
        if installed != latest {
            println!(
                "[OMP Automation] Installed OMP (v{}) is behind latest (v{}). Updating subdollar-sandbox...",
                installed, latest
            );
            match update_sandbox_image(Some(latest)).await {
                Ok(new_status) => {
                    println!(
                        "[OMP Automation] Successfully updated subdollar-sandbox to OMP v{}.",
                        new_status.installed_version.unwrap_or_else(|| latest.clone())
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[OMP Automation Warning] Failed to update OMP sandbox: {}. Continuing with existing v{}.",
                        e, installed
                    );
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_omp_version() {
        assert_eq!(parse_omp_version("omp/18.1.21"), "18.1.21");
        assert_eq!(parse_omp_version("omp/18.1.12\n"), "18.1.12");
        assert_eq!(parse_omp_version("v1.2.3"), "1.2.3");
        assert_eq!(parse_omp_version("  18.1.21  "), "18.1.21");
    }

    #[tokio::test]
    async fn test_check_status_returns_sensible_structure() {
        let status = check_status(false).await;
        if status.image_exists {
            assert!(status.installed_version.is_some());
        }
    }
}
