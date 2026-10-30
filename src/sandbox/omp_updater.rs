use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::RwLock;

/// Cached `(status, when it was cached)` for the installed-vs-latest check.
type StatusCache = Arc<RwLock<Option<(OmpVersionStatus, std::time::Instant)>>>;

static STATUS_CACHE: LazyLock<StatusCache> = LazyLock::new(|| Arc::new(RwLock::new(None)));
static IS_UPDATING: AtomicBool = AtomicBool::new(false);

const CACHE_TTL_SECS: u64 = 600; // 10 minutes

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OmpVersionStatus {
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub up_to_date: bool,
    pub image_exists: bool,
    pub is_updating: bool,
    pub last_error: Option<String>,
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

/// R36 F1: the exact image ID a run uses. None when the image is absent
/// or Docker cannot inspect it (old rows and eval-only tests).
pub fn sandbox_image_id() -> Option<String> {
    let output = Command::new("docker")
        .args([
            "image",
            "inspect",
            "subdollar-sandbox",
            "--format",
            "{{.Id}}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
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
        .arg("--build-arg")
        .arg(format!("SANDBOX_UID={}", unsafe { libc::getuid() }))
        .arg("--build-arg")
        .arg(format!("SANDBOX_GID={}", unsafe { libc::getgid() }))
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

/// Ensure the pinned sandbox image is usable, then optionally track latest.
///
/// Two distinct concerns, deliberately not gated together:
///
/// - **Bootstrap** (always on): if the image is absent, build it. A fresh
///   checkout has no image and cannot run anything without this.
/// - **Auto-update** (OFF unless `SUBDOLLAR_AUTO_UPDATE_OMP` opts in): pulling
///   npm `latest` on every run silently changes the agent between runs, so two
///   runs a week apart are produced by different binaries while sharing one
///   `rules_version` comparability key. That is a reproducibility hole, and
///   `Dockerfile.sandbox`'s `ARG OMP_VERSION` pin is overridden by the rebuild,
///   so the pin in the file was not actually holding.
///
/// Set `SUBDOLLAR_AUTO_UPDATE_OMP=true` to restore the old drift-on-every-run
/// behaviour; the manifest records `omp_version` either way, so the drift is
/// at least visible after the fact.
pub async fn auto_update_if_needed() -> Result<()> {
    let auto_update_env = std::env::var("SUBDOLLAR_AUTO_UPDATE_OMP")
        .unwrap_or_else(|_| "false".to_string())
        .to_lowercase();
    let auto_update = !matches!(auto_update_env.as_str(), "false" | "0" | "off" | "");

    let status = check_status(false).await;

    if !status.image_exists {
        println!("[OMP Automation] subdollar-sandbox image not found. Building automatically...");
        update_sandbox_image(None).await?;
        println!("[OMP Automation] subdollar-sandbox built successfully.");
        return Ok(());
    }

    if !auto_update {
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
                        new_status
                            .installed_version
                            .unwrap_or_else(|| latest.clone())
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

    #[test]
    fn test_parse_omp_version_edge_cases() {
        assert_eq!(parse_omp_version("omp/"), "");
        assert_eq!(parse_omp_version("v"), "");
        assert_eq!(parse_omp_version("   "), "");
        assert_eq!(parse_omp_version("omp/v7.0.1"), "7.0.1");
        assert_eq!(parse_omp_version("x/v9"), "x/v9");
    }

    // -----------------------------------------------------------------
    // Fake-docker subprocess harness
    // -----------------------------------------------------------------
    //
    // The image/version/build paths shell out to `docker` via PATH, so a
    // temp-dir script named `docker` stands in for the engine. PATH is
    // process-global, so the scenarios run in a re-exec'd child of this test
    // binary: the parent only writes the fake binary and asserts the child's
    // status, while the child owns every env mutation (PATH/SDB_HOME).
    const DRIVER_ENV: &str = "SDB_COVTEST_UPD_DRIVER";
    const BIN_ENV: &str = "SDB_COVTEST_UPD_BIN";
    const WORK_ENV: &str = "SDB_COVTEST_UPD_WORK";
    const SCENARIO_ENV: &str = "SDB_COVTEST_UPD_SCENARIO";
    const ARGS_ENV: &str = "SDB_COVTEST_UPD_ARGS";

    const FAKE_DOCKER: &str = r#"#!/bin/sh
# covtest fake `docker`: scenario-driven exits, no engine.
scenario="${SDB_COVTEST_UPD_SCENARIO:-}"
if [ -n "${SDB_COVTEST_UPD_ARGS:-}" ]; then printf '%s\n' "$@" >> "$SDB_COVTEST_UPD_ARGS"; fi
case "$1" in
  image)
    case "$scenario" in
      image_id) printf 'sha256:r36fixture\n'; exit 0 ;;
      image_yes|build_ok) exit 0 ;;
      *) exit 1 ;;
    esac
    ;;
  run)
    case "$scenario" in
      image_id) printf 'fake-anchor\n'; exit 0 ;;
      version_ok|image_yes|build_ok) printf 'omp/18.1.21\n'; exit 0 ;;
      version_empty) printf '\n'; exit 0 ;;
      *) exit 1 ;;
    esac
    ;;
  network)
    # The image test only needs the network create to succeed.
    exit 0 ;;
  build)
    case "$scenario" in
      build_ok) exit 0 ;;
      *) printf '%s\n' 'boom build' >&2; exit 1 ;;
    esac
    ;;
esac
exit 1
"#;

    async fn upd_driver() {
        let bin = std::env::var(BIN_ENV).expect("SDB_COVTEST_UPD_BIN");
        let work = std::path::PathBuf::from(std::env::var(WORK_ENV).expect("WORK"));
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", bin, old_path));
        let home = work.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);

        // Engine image presence.
        std::env::set_var(SCENARIO_ENV, "image_yes");
        assert!(check_image_exists(), "exit 0 means the image exists");
        std::env::set_var(SCENARIO_ENV, "image_no");
        assert!(!check_image_exists(), "exit 1 means no image");
        std::env::set_var(SCENARIO_ENV, "image_id");
        assert_eq!(
            sandbox_image_id().as_deref(),
            Some("sha256:r36fixture"),
            "the run stamp records the image ID"
        );

        // R36 F1: a real eval-only pipeline run records the image ID in its
        // manifest while the fake docker is on PATH.
        let pipe_dir = work.join("pipeline-image");
        std::fs::create_dir_all(&pipe_dir).unwrap();
        struct PipeLogger;
        impl crate::pipeline::PipelineLogger for PipeLogger {
            fn log(&self, _msg: &str) {}
        }
        let cfg = crate::pipeline::BenchmarkConfig {
            model: "image-test".to_string(),
            task: crate::config::TaskType::Sql,
            effort: "low".to_string(),
            harness: crate::config::Harness::Omp,
            max_turns: 1,
            budget_usd: 0.01,
            timeout_min: 1,
            api_key: None,
            workdir: pipe_dir,
            eval_only: true,
            run_id: Some("r36_image_test".to_string()),
            save_results: false,
            review_model: None,
            worker: 0,
            feedback_rounds: 0,
            campaign_stage: None,
            baseline_rps: None,
        };
        let out =
            crate::pipeline::BenchmarkPipeline::execute(cfg, std::sync::Arc::new(PipeLogger), None)
                .await
                .expect("the eval-only image run completes");
        assert_eq!(
            out.manifest.sandbox_image.as_deref(),
            Some("sha256:r36fixture"),
            "the pipeline manifest carries the image ID"
        );

        // Installed version parsing off `docker run ... omp --version`.
        std::env::set_var(SCENARIO_ENV, "version_ok");
        assert_eq!(get_installed_sandbox_version().as_deref(), Some("18.1.21"));
        std::env::set_var(SCENARIO_ENV, "version_empty");
        assert_eq!(
            get_installed_sandbox_version(),
            None,
            "empty stdout is None"
        );
        std::env::set_var(SCENARIO_ENV, "version_fail");
        assert_eq!(
            get_installed_sandbox_version(),
            None,
            "non-zero docker run is None"
        );

        // docker build failure surfaces the stderr.
        let repo = work.join("repo-with-dockerfile");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("Dockerfile.sandbox"), "FROM scratch\n").unwrap();
        std::env::set_var("SDB_HOME", &repo);
        std::env::set_var(SCENARIO_ENV, "build_fail");
        let err = update_sandbox_image(Some("9.9.9"))
            .await
            .expect_err("build failure must propagate");
        std::env::remove_var("SDB_HOME");
        let msg = err.to_string();
        assert!(msg.contains("docker build failed"), "{msg}");
        assert!(msg.contains("boom build"), "{msg}");

        // Missing Dockerfile: fail before touching the engine.
        let empty_repo = work.join("repo-empty");
        std::fs::create_dir_all(&empty_repo).unwrap();
        std::env::set_var("SDB_HOME", &empty_repo);
        let err = update_sandbox_image(Some("1.0.0"))
            .await
            .expect_err("missing Dockerfile must fail cleanly");
        std::env::remove_var("SDB_HOME");
        assert!(
            err.to_string().contains("Dockerfile.sandbox not found"),
            "{err}"
        );

        // Concurrent update guard.
        IS_UPDATING.store(true, Ordering::SeqCst);
        let err = update_sandbox_image(Some("1.0.0"))
            .await
            .expect_err("second updater must be rejected");
        IS_UPDATING.store(false, Ordering::SeqCst);
        assert!(err.to_string().contains("already in progress"), "{err}");

        // Cached status: served without touching docker or the registry.
        let cached = OmpVersionStatus {
            installed_version: Some("1.2.3".to_string()),
            latest_version: Some("1.2.4".to_string()),
            up_to_date: false,
            image_exists: true,
            is_updating: false,
            last_error: None,
        };
        *STATUS_CACHE.write().await = Some((cached.clone(), std::time::Instant::now()));
        let got = check_status(false).await;
        assert_eq!(got.installed_version.as_deref(), Some("1.2.3"));
        assert_eq!(got.latest_version.as_deref(), Some("1.2.4"));
        assert!(!got.up_to_date);
        assert!(got.image_exists);
        // is_updating is refreshed from the live flag on cache hits.
        IS_UPDATING.store(true, Ordering::SeqCst);
        let got2 = check_status(false).await;
        IS_UPDATING.store(false, Ordering::SeqCst);
        assert!(
            got2.is_updating,
            "live updating flag must ride the cache hit"
        );

        // No-cache refresh path: the HTTPS registry fetch is pointed at a
        // closed loopback port, so it fails instantly without leaving the
        // host; docker stays faked.
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9");
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:9");
        std::env::set_var(SCENARIO_ENV, "image_yes");
        *STATUS_CACHE.write().await = None;
        let fresh = check_status(true).await;
        assert!(fresh.image_exists);
        assert_eq!(fresh.installed_version.as_deref(), Some("18.1.21"));
        assert!(fresh.latest_version.is_none());
        assert!(!fresh.up_to_date);
        assert!(
            fresh.last_error.is_some(),
            "registry failure must be recorded"
        );
        // The fresh result is cached immediately.
        let again = check_status(false).await;
        assert_eq!(again.last_error, fresh.last_error);
        // Cache expiry: a stale timestamp forces the refresh path.
        *STATUS_CACHE.write().await = Some((
            cached.clone(),
            std::time::Instant::now() - Duration::from_secs(700),
        ));
        let expired = check_status(false).await;
        assert!(
            expired.last_error.is_some(),
            "stale cache must be replaced by a fresh probe"
        );
        assert_eq!(expired.installed_version.as_deref(), Some("18.1.21"));
        // Image absent: the installed version is never probed.
        std::env::set_var(SCENARIO_ENV, "image_no");
        *STATUS_CACHE.write().await = None;
        let missing = check_status(true).await;
        assert!(!missing.image_exists);
        assert_eq!(missing.installed_version, None);

        // Successful build refresh (fake build + failed loopback fetch).
        std::env::set_var(SCENARIO_ENV, "build_ok");
        std::env::set_var("SDB_HOME", &repo);
        let args_file = work.join("build-args.txt");
        std::env::set_var(ARGS_ENV, &args_file);
        let built = update_sandbox_image(Some("2.0.0"))
            .await
            .expect("build ok must converge");
        let args = std::fs::read_to_string(&args_file).unwrap_or_default();
        std::env::remove_var(ARGS_ENV);
        assert!(
            args.contains("SANDBOX_UID=") && args.contains("SANDBOX_GID="),
            "the build must pass the bench user's UID/GID; args:\n{args}"
        );
        assert!(built.image_exists);
        // Target None: the latest fetch fails on the loopback proxy, so the
        // build falls back to the literal "latest" pin.
        let built_latest = update_sandbox_image(None)
            .await
            .expect("target None must fall back to latest");
        assert!(built_latest.image_exists);
        std::env::remove_var("SDB_HOME");

        // auto_update_if_needed with the opt-in off: cached status, no work.
        std::env::set_var("SUBDOLLAR_AUTO_UPDATE_OMP", "false");
        *STATUS_CACHE.write().await = Some((cached.clone(), std::time::Instant::now()));
        auto_update_if_needed()
            .await
            .expect("auto update off must be a no-op");
        // Opt-in on with installed == latest: also a no-op.
        std::env::set_var("SUBDOLLAR_AUTO_UPDATE_OMP", "true");
        auto_update_if_needed()
            .await
            .expect("up-to-date status must be a no-op");
        // Opt-in on and behind: the build path runs (fake engine).
        std::env::set_var("SDB_HOME", &repo);
        std::env::set_var(SCENARIO_ENV, "build_ok");
        let behind = OmpVersionStatus {
            installed_version: Some("1.0.0".to_string()),
            latest_version: Some("2.0.0".to_string()),
            up_to_date: false,
            image_exists: true,
            is_updating: false,
            last_error: None,
        };
        *STATUS_CACHE.write().await = Some((behind, std::time::Instant::now()));
        auto_update_if_needed()
            .await
            .expect("behind status must trigger the fake build");
        // Opt-in on but the rebuild fails: warn and continue, never fatal.
        std::env::set_var("SUBDOLLAR_AUTO_UPDATE_OMP", "true");
        std::env::set_var(SCENARIO_ENV, "build_fail");
        let behind_fail = OmpVersionStatus {
            installed_version: Some("1.0.0".to_string()),
            latest_version: Some("2.0.0".to_string()),
            up_to_date: false,
            image_exists: true,
            is_updating: false,
            last_error: None,
        };
        *STATUS_CACHE.write().await = Some((behind_fail, std::time::Instant::now()));
        auto_update_if_needed()
            .await
            .expect("a failed rebuild must warn and continue");
        // Image missing (auto-update off): bootstrap build path.
        std::env::set_var("SUBDOLLAR_AUTO_UPDATE_OMP", "false");
        std::env::set_var(SCENARIO_ENV, "build_ok");
        let no_image = OmpVersionStatus {
            installed_version: None,
            latest_version: None,
            up_to_date: false,
            image_exists: false,
            is_updating: false,
            last_error: None,
        };
        *STATUS_CACHE.write().await = Some((no_image, std::time::Instant::now()));
        auto_update_if_needed()
            .await
            .expect("missing image must bootstrap-build");
        std::env::remove_var("SDB_HOME");
        std::env::remove_var("SUBDOLLAR_AUTO_UPDATE_OMP");
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("HTTP_PROXY");
        *STATUS_CACHE.write().await = None;

        println!("fake-docker updater scenarios passed");
    }

    /// Re-exec'd child entry point: the parent sets DRIVER_ENV, so a normal
    /// suite run is a no-op.
    #[tokio::test]
    async fn fake_docker_driver() {
        if std::env::var(DRIVER_ENV).ok().as_deref() != Some("1") {
            return;
        }
        upd_driver().await;
    }

    /// Parent-side scenario launcher: writes the fake `docker` and runs the
    /// driver in a child process so PATH/SDB_HOME mutations never leak into
    /// the parallel test suite.
    #[test]
    fn test_updater_fake_docker_scenarios() {
        let root = std::env::temp_dir().join(format!("covtest-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let docker = root.join("bin/docker");
        std::fs::write(&docker, FAKE_DOCKER).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::omp_updater::tests::fake_docker_driver",
                "--nocapture",
            ])
            .env(DRIVER_ENV, "1")
            .env(BIN_ENV, root.join("bin"))
            .env(WORK_ENV, &root)
            .output()
            .expect("spawn fake-docker driver child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "fake-docker updater driver failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("fake-docker updater scenarios passed"),
            "driver must report its scenarios; got:\n{stdout}\n{stderr}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
