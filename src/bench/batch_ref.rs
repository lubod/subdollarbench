//! Batch-level reference calibration.
//!
//! One measured reference per multi-trial batch, shared by all trials.
//! Per-trial ref containers still start (the agent keeps its black-box oracle),
//! but trials no longer *measure* the ref: every trial scores against the same
//! denominator, so ref-side jitter cannot reshuffle trial order.
//!
//! Lifecycle: [`BatchRef::start`] before trial 1 (warmup + median on an
//! isolated batch port), [`BatchRef::finish`] after the last trial (post
//! bracket, drift flag, history append, container cleanup).

use anyhow::{anyhow, Result};
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::bench::BenchmarkRunner;
use crate::config::{get_repo_root, TaskType};
use crate::sandbox::docker::{SandboxManager, BENCH_REDIS_CLIENT_IMAGE, REF_NGINX_IMAGE, REF_REDIS_IMAGE};

/// Batch-port assignments. Deliberately offset from the per-trial ref ports
/// (6380/8081) so the batch ref can live across the whole batch while trial
/// sandboxes churn their own refs underneath.
pub fn batch_port(task: &TaskType) -> Option<u16> {
    match task {
        TaskType::Redis => Some(6389),
        TaskType::Http | TaskType::Proxy => Some(8089),
        TaskType::Dns | TaskType::Pdf | TaskType::Wal | TaskType::Pubsub => None,
    }
}

fn ref_image(task: &TaskType) -> &'static str {
    match task {
        TaskType::Redis => REF_REDIS_IMAGE,
        TaskType::Http | TaskType::Proxy => REF_NGINX_IMAGE,
        _ => "none",
    }
}

fn wait_redis_ready(port: u16) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "host",
                BENCH_REDIS_CLIENT_IMAGE,
                "redis-cli",
                "-h",
                "127.0.0.1",
                "-p",
                &port.to_string(),
                "ping",
            ])
            .output();
        if let Ok(o) = out {
            if String::from_utf8_lossy(&o.stdout).contains("PONG") {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

fn wait_http_ready(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok() {
            // TCP accept means nginx is up (stock image serves / immediately).
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

fn measure(task: &TaskType, port: u16) -> Result<f64> {
    match task {
        TaskType::Redis => BenchmarkRunner::run_redis_benchmark(port),
        TaskType::Http | TaskType::Proxy => BenchmarkRunner::run_wrk_benchmark(port),
        _ => Err(anyhow!("no external reference for {:?}", task)),
    }
}

pub struct BatchRef {
    sandbox: SandboxManager,
    task: TaskType,
    port: u16,
    pub pre: f64,
}

pub struct BatchRefReport {
    pub pre: f64,
    pub post: Option<f64>,
    pub drift: bool,
}

impl BatchRef {
    /// Start the batch ref, warm it up (one discarded window), and return the
    /// median-of-3 pre-batch value. `None` task support (DNS/PDF) yields `Ok(None)`.
    pub fn start(task: &TaskType, parent_run_id: &str, log: &dyn Fn(&str)) -> Result<Option<Self>> {
        let port = match batch_port(task) {
            Some(p) => p,
            None => return Ok(None),
        };
        let sandbox = SandboxManager::with_id(&format!("{}_batchref", parent_run_id));
        sandbox.ensure_network()?;
        let start_res = match task {
            TaskType::Redis => sandbox.start_reference_redis(port),
            TaskType::Http | TaskType::Proxy => sandbox.start_reference_http(port),
            _ => return Ok(None),
        };
        if let Err(e) = start_res {
            log(&format!("[BATCH-REF] Warning: batch reference unavailable: {}. Trials fall back to live per-trial refs.", e));
            return Ok(None);
        }
        let ready = match task {
            TaskType::Redis => wait_redis_ready(port),
            TaskType::Http | TaskType::Proxy => wait_http_ready(port),
            _ => false,
        };
        if !ready {
            log("[BATCH-REF] Warning: batch reference never became ready. Trials fall back to live per-trial refs.");
            sandbox.cleanup();
            return Ok(None);
        }
        // Warmup window (discarded): settles page cache / JIT so measured
        // windows reflect steady state, not cold start.
        let _ = measure(task, port);
        match measure(task, port) {
            Ok(pre) => {
                log(&format!(
                    "[BATCH-REF] Batch reference {} median: {:.0} req/sec on port {} (shared by all trials)",
                    ref_image(task),
                    pre,
                    port
                ));
                Ok(Some(BatchRef { sandbox, task: task.clone(), port, pre }))
            }
            Err(e) => {
                log(&format!("[BATCH-REF] Warning: batch calibration failed: {}. Trials fall back to live per-trial refs.", e));
                sandbox.cleanup();
                Ok(None)
            }
        }
    }

    pub fn reference(&self) -> f64 {
        self.pre
    }

    /// Post-batch bracket: re-measure, flag drift, append history, clean up.
    /// Never fails the batch: worst case the report carries `post: None`.
    pub fn finish(self, log: &dyn Fn(&str)) -> BatchRefReport {
        let post = match measure(&self.task, self.port) {
            Ok(v) => Some(v),
            Err(e) => {
                log(&format!("[BATCH-REF] Warning: post-batch re-measure failed: {}", e));
                None
            }
        };
        let drift = match post {
            Some(p) => {
                let d = crate::bench::ref_drifted(self.pre, p);
                log(&format!(
                    "[BATCH-REF] Post-batch reference: {:.0} req/sec (pre {:.0}, drift {})",
                    p,
                    self.pre,
                    if d { "FLAGGED >10%" } else { "ok" }
                ));
                d
            }
            None => false,
        };
        append_history(&self.task, self.pre, post, drift);
        self.sandbox.cleanup();
        BatchRefReport { pre: self.pre, post, drift }
    }
}

fn append_history(task: &TaskType, pre: f64, post: Option<f64>, drift: bool) {
    let line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "task": task.to_string(),
        "image": ref_image(task),
        "ref_pre": pre,
        "ref_post": post,
        "drift": drift,
        "host": hostname(),
    });
    let path = get_repo_root().join("results").join("ref_history.jsonl");
    let _ = fs::create_dir_all(path.parent().unwrap());
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", line);
    }
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_port_assignment() {
        assert_eq!(batch_port(&TaskType::Redis), Some(6389));
        assert_eq!(batch_port(&TaskType::Http), Some(8089));
        assert_eq!(batch_port(&TaskType::Dns), None);
        assert_eq!(batch_port(&TaskType::Pdf), None);
        // Offset from per-trial ref ports: no collision with trial churn.
        assert_ne!(batch_port(&TaskType::Redis), Some(6380));
        assert_ne!(batch_port(&TaskType::Http), Some(8081));
    }
}
