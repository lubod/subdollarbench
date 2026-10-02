pub mod batch_ref;

use crate::sandbox::docker::{BENCH_REDIS_CLIENT_IMAGE, BENCH_WRK_IMAGE};

use anyhow::{anyhow, Result};
use regex::Regex;
use std::fs;
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Live CoreDNS baseline on an ephemeral host port, probed with the same
/// `measure_throughput` path as candidates. Starts the pinned CoreDNS image
/// (forwarding Corefile like the trial oracle), warms up, measures, cleans up.
/// Returns None instead of a constant so refless runs stay unscored.
pub async fn measure_dns_reference(seed: &str) -> Option<f64> {
    use crate::sandbox::docker::SandboxManager;
    use crate::verifier::DnsVerifier;
    let sandbox = SandboxManager::with_id(&format!("dnsref-{}", seed));
    // Ephemeral port avoids colliding with trial refs or candidates.
    let port: u16 = 15353 + (rand::random::<u16>() % 4000);
    if sandbox.start_reference_dns(port).is_err() {
        return None;
    }
    let v = DnsVerifier::new(port, Some(seed));
    if !v.wait_for_ready(20).await {
        sandbox.cleanup();
        return None;
    }
    // Warmup window (discarded), then the scored measurement.
    let _ = v.measure_throughput().await;
    let tp = v.measure_throughput().await;
    sandbox.cleanup();
    tp
}

/// Load-test shape. Part of the measurement apparatus like BENCH_REPS:
/// changing these moves every recorded denominator, so they are constants,
/// not flags. `-q` keeps redis-benchmark output to the parseable summary
/// lines the regex depends on.
pub const REDIS_BENCH_N: &str = "5000";
pub const REDIS_BENCH_C: &str = "10";
pub const WRK_THREADS: &str = "2";
pub const WRK_CONNS: &str = "20";
pub const WRK_DURATION: &str = "3s";

pub struct BenchmarkRunner;

static REDIS_BENCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d+\.?\d*)\s+requests per second").expect("valid redis benchmark regex")
});

static WRK_BENCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Requests/sec:\s+(\d+\.?\d*)").expect("valid wrk benchmark regex")
});

/// Median of measured samples. Empty input yields 0.0 (callers only pass
/// non-empty success sets; all-failure runs return Err before reaching here).
pub fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = xs.len() / 2;
    if xs.len() % 2 == 1 {
        xs[mid]
    } else {
        (xs[mid - 1] + xs[mid]) / 2.0
    }
}

/// Number of benchmark repetitions per window; the median is reported.
///
/// Calibrated 2026-10-02 (n=12 idle + n=12 loaded interleaved probe,
/// pinned redis ref, same client/path as scoring): idle windows spread
/// 2.1x (98k..210k, median 175k), loaded windows 1.2x (54k..66k, median
/// 61k), idle/loaded medians 2.86x apart. A single window cannot separate
/// host-load noise from candidate quality — and the 10% drift bracket is an
/// order of magnitude tighter than what one window resolves. 5 reps keeps
/// the median honest at ~2x the wall cost of 3; the BenchLock serializes
/// windows against sibling workers, so the cost is contention, not just
/// seconds.
pub const BENCH_REPS: usize = 5;

/// True when the `pre`/`post` batch reference brackets are not mutually
/// trustworthy. A zero, negative, or non-finite reading on either side means
/// the reference itself failed (container gone, port lost, probe returned
/// nothing) — that is the *strongest* reason to distrust it, not a reason to
/// report "no drift". `finish()` quarantines any batch this returns true for.
pub fn ref_drifted_with_tol(pre: f64, post: f64, tol: f64) -> bool {
    if !(pre.is_finite() && post.is_finite()) || pre <= 0.0 || post <= 0.0 {
        return true;
    }
    ((post - pre) / pre).abs() > tol
}

/// Default-tolerance wrapper around [`ref_drifted_with_tol`].
pub fn ref_drifted(pre: f64, post: f64) -> bool {
    ref_drifted_with_tol(pre, post, 0.10)
}

/// RAII guard for the host-global benchmark lock. Only one load-test window
/// runs at a time across processes; agent work is unaffected. Drop releases.
pub struct BenchLock {
    path: std::path::PathBuf,
    owned: bool,
}

impl Drop for BenchLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Acquire the benchmark lock, fail-open after 90s (a run must never deadlock
/// on a stale lock; callers log the fallback). Stale holders are treated as
/// crashed and evicted.
///
/// LIVE 2026-10-02: a 180s stale threshold wedged the whole engine — a dead
/// holder's lock file sat 7min old while BatchRef::start_on_worker blocked
/// the tokio worker in std::thread::sleep(500ms) retry, starving the runtime
/// (all API hung with empty bodies, gdb showed every worker parked). The
/// threshold must stay well under the 90s fail-open: 30s evicts a crashed
/// holder while a live window (seconds per rep, lock held briefly) never
/// trips it. Lock I/O itself must never block an async worker (see below).
pub const BENCH_LOCK_STALE_SECS: u64 = 30;
pub fn acquire_bench_lock() -> BenchLock {
    let path = std::env::temp_dir().join("subdollar-bench.lock");
    let start = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "pid={}", std::process::id());
                return BenchLock { path, owned: true };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or(Duration::ZERO) > Duration::from_secs(BENCH_LOCK_STALE_SECS))
                    .unwrap_or(true);
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if start.elapsed() > Duration::from_secs(90) {
                    eprintln!(
                        "[BENCH] lock wait exceeded 90s; proceeding without lock (contention possible)"
                    );
                    return BenchLock { path, owned: false };
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => {
                return BenchLock { path, owned: false };
            }
        }
    }
}

impl BenchmarkRunner {
    pub fn parse_redis_benchmark_output(stdout: &str) -> Result<f64> {
        let mut rates = Vec::new();
        for cap in REDIS_BENCH_RE.captures_iter(stdout) {
            if let Some(m) = cap.get(1) {
                if let Ok(val) = m.as_str().parse::<f64>() {
                    rates.push(val);
                }
            }
        }

        if rates.is_empty() {
            return Err(anyhow!(
                "Could not parse redis-benchmark output. stdout: '{}'",
                stdout
            ));
        }

        let avg = rates.iter().sum::<f64>() / rates.len() as f64;
        Ok(avg)
    }

    pub fn parse_wrk_output(stdout: &str) -> Result<f64> {
        if let Some(cap) = WRK_BENCH_RE.captures(stdout) {
            if let Some(m) = cap.get(1) {
                if let Ok(val) = m.as_str().parse::<f64>() {
                    return Ok(val);
                }
            }
        }

        Err(anyhow!("Could not parse wrk output. stdout: '{}'", stdout))
    }

    pub fn run_redis_benchmark(net: &str, host: &str, port: u16) -> Result<f64> {
        Self::run_redis_benchmark_reps(net, host, port, BENCH_REPS)
    }

    /// Median over `reps` windows under the host-global lock. Partial success
    /// still yields a median; total failure returns the last error.
    pub fn run_redis_benchmark_reps(net: &str, host: &str, port: u16, reps: usize) -> Result<f64> {
        let _lock = acquire_bench_lock();
        let mut ok = Vec::new();
        let mut last_err = anyhow!("no benchmark repetitions attempted");
        for _ in 0..reps.max(1) {
            let output = Command::new("timeout")
                .args([
                    "15",
                    "docker",
                    "run",
                    "--rm",
                    "--network",
                    net,
                    BENCH_REDIS_CLIENT_IMAGE,
                    "redis-benchmark",
                    "-h",
                    host,
                    "-p",
                    &port.to_string(),
                    "-n",
                    REDIS_BENCH_N,
                    "-c",
                    REDIS_BENCH_C,
                    "-t",
                    "get,set",
                    "-q",
                ])
                .output()
                .map_err(|e| anyhow!("Failed to execute docker redis-benchmark: {}", e))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            match Self::parse_redis_benchmark_output(&stdout) {
                Ok(v) => ok.push(v),
                Err(e) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    last_err = anyhow!("{}, stderr: '{}'", e, stderr);
                }
            }
        }
        if ok.is_empty() {
            return Err(last_err);
        }
        Ok(median(ok))
    }

    pub fn run_wrk_benchmark(net: &str, host: &str, port: u16) -> Result<f64> {
        Self::run_wrk_benchmark_reps(net, host, port, BENCH_REPS)
    }

    /// Median over `reps` windows under the host-global lock. Partial success
    /// still yields a median; total failure returns the last error.
    pub fn run_wrk_benchmark_reps(net: &str, host: &str, port: u16, reps: usize) -> Result<f64> {
        let _lock = acquire_bench_lock();
        let url = format!("http://{host}:{port}/");
        let mut ok = Vec::new();
        let mut last_err = anyhow!("no benchmark repetitions attempted");
        for _ in 0..reps.max(1) {
            let output = Command::new("timeout")
                .args([
                    "15",
                    "docker",
                    "run",
                    "--rm",
                    "--network",
                    net,
                    BENCH_WRK_IMAGE,
                    &format!("-t{}", WRK_THREADS),
                    &format!("-c{}", WRK_CONNS),
                    &format!("-d{}", WRK_DURATION),
                    &url,
                ])
                .output()
                .map_err(|e| anyhow!("Failed to execute docker wrk: {}", e))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            match Self::parse_wrk_output(&stdout) {
                Ok(v) => ok.push(v),
                Err(e) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    last_err = anyhow!("{}, stderr: '{}'", e, stderr);
                }
            }
        }
        if ok.is_empty() {
            return Err(last_err);
        }
        Ok(median(ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_redis_benchmark_output() {
        let output = "====== SET ======\n  5000 requests completed in 0.07 seconds\n  10 parallel clients\n  SET: 74215.3 requests per second\n====== GET ======\n  5000 requests completed in 0.06 seconds\n  GET: 85112.7 requests per second\n";
        let res = BenchmarkRunner::parse_redis_benchmark_output(output).unwrap();
        assert!((res - 79664.0).abs() < 1.0);

        let err = BenchmarkRunner::parse_redis_benchmark_output(
            "Could not connect to Redis at 127.0.0.1:6379: Connection refused",
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_parse_wrk_output() {
        let output = "Running 3s test @ http://127.0.0.1:8080/\n  2 threads and 20 connections\n  Thread Stats   Avg      Stdev     Max   +/- Stdev\n    Latency     1.24ms    1.12ms  15.22ms   88.42%\n    Req/Sec    12.45k     2.10k   18.20k    72.00%\n  74215 requests in 3.01s, 9.22MB read\nRequests/sec:  24656.12\nTransfer/sec:      3.06MB\n";
        let res = BenchmarkRunner::parse_wrk_output(output).unwrap();
        assert_eq!(res, 24656.12);

        let err = BenchmarkRunner::parse_wrk_output("Socket error: connect failed");
        assert!(err.is_err());
    }

    #[test]
    fn test_median() {
        assert_eq!(median(vec![]), 0.0);
        assert_eq!(median(vec![5.0]), 5.0);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![137218.0, 66251.0, 69565.0]), 69565.0);
        assert_eq!(median(vec![1.0, 4.0]), 2.5);
    }

    #[test]
    fn test_ref_drifted() {
        assert!(!ref_drifted(70000.0, 73500.0));
        assert!(ref_drifted(70000.0, 80000.0));
        // A dead reference is drift, not a clean bill of health: the bracket
        // cannot be trusted, so the batch must be quarantined.
        assert!(ref_drifted(0.0, 70000.0));
        assert!(ref_drifted(70000.0, 0.0));
        assert!(ref_drifted(0.0, 0.0));
        assert!(ref_drifted(f64::NAN, 70000.0));
        assert!(ref_drifted(70000.0, f64::INFINITY));
    }

    #[test]
    fn test_ref_drifted_tolerance_is_per_task() {
        // DNS carries a wider band (its denominator is RTT-bound, not
        // work-bound). 70000 -> 80000 is +14.3%: clean at 0.25, drift at 0.10.
        assert!(!ref_drifted_with_tol(70000.0, 80000.0, 0.25));
        assert!(ref_drifted_with_tol(70000.0, 80000.0, 0.10));
        // +5% is inside the default band and must stay clean, or every wave
        // gets quarantined on ordinary host jitter.
        assert!(!ref_drifted_with_tol(70000.0, 73500.0, 0.10));
        // The boundary is strict: exactly at tolerance is not drift.
        assert!(!ref_drifted_with_tol(100.0, 110.0, 0.10));
    }

    #[test]
    fn test_bench_lock_acquire_release() {
        {
            let _g = acquire_bench_lock();
            // Second create_new must fail while held (unless fail-open path).
            let path = std::env::temp_dir().join("subdollar-bench.lock");
            assert!(path.exists() || !_g.owned);
        }
        // Drop released: a fresh acquire must own the lock promptly.
        let g = acquire_bench_lock();
        assert!(g.owned);
    }

    #[test]
    fn test_run_redis_benchmark_failure_on_unbound_port() {
        // Port 59997 has no redis running, docker redis-benchmark should fail and return Err
        let res = BenchmarkRunner::run_redis_benchmark("net", "ref-redis", 6379);
        assert!(res.is_err());
    }

    #[test]
    fn test_run_wrk_benchmark_failure_on_unbound_port() {
        // Port 59996 has no http running, wrk should fail and return Err
        let res = BenchmarkRunner::run_wrk_benchmark("net", "ref-http", 80);
        assert!(res.is_err());
    }
}
