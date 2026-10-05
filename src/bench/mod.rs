pub mod batch_ref;

use crate::sandbox::docker::{BENCH_REDIS_CLIENT_IMAGE, BENCH_WRK_IMAGE};

use anyhow::{anyhow, Result};
use regex::Regex;
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

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
///
/// flock-based (LOCK_EX|LOCK_NB on a lockfile fd held for the guard's life):
/// a dead holder's fd closes with the process, so the kernel releases the
/// lock — no mtime guessing, no stale-file eviction race. The old
/// create_new+mtime scheme wedged the whole engine (dead holder, 180s
/// threshold over the 90s fail-open, sync sleep-retry starving the tokio
/// runtime). A `blocking` guard variant exists for async callers: never
/// sleep-retry on a tokio worker; park the wait on spawn_blocking instead.
pub struct BenchLock {
    file: Option<std::fs::File>,
    owned: bool,
}

impl BenchLock {
    /// False when we proceeded without the lock (fail-open): contention
    /// possible, callers log it. True means the window is serialized.
    pub fn is_owned(&self) -> bool {
        self.owned
    }
}

impl Drop for BenchLock {
    fn drop(&mut self) {
        // fd close releases LOCK_EX; file itself persists (name is stable).
        drop(self.file.take());
    }
}

/// Acquire the benchmark lock, fail-open after 90s (a run must never deadlock
/// on a stale lock; callers log the fallback). flock means a crashed holder
/// frees the lock with its fd — waiting is bounded by live holders only.
///
/// Sync version: blocks the calling thread (100ms park). NEVER call from a
/// tokio worker: the sleep-retry starves the runtime — gdb showed every
/// worker parked and the whole API hung with empty bodies (2026-10-02 and
/// again 2026-10-03, when a wedged redis-benchmark client held a worker
/// inside Command::output for 5h). Async callers use acquire_bench_lock_async.
pub fn acquire_bench_lock() -> BenchLock {
    acquire_bench_lock_sync(Duration::from_secs(90))
}

fn acquire_bench_lock_sync(limit: Duration) -> BenchLock {
    let path = std::env::temp_dir().join("subdollar-bench.lock");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            // LOUD: an unwritable lock file silently unserializes every
            // window (20:06 root-owned file: whole wave unserialized, zero
            // log lines). Fail-open stays (a run must never deadlock), but
            // the operator must see it.
            eprintln!(
                "[BENCH] lock file {} not openable ({}); windows UNSERIALIZED",
                path.display(),
                e
            );
            return BenchLock {
                file: None,
                owned: false,
            };
        }
    };
    let start = Instant::now();
    loop {
        // LOCK_NB: bounded park between tries.
        let locked = flock_try_lock(&file);
        if locked {
            return BenchLock {
                file: Some(file),
                owned: true,
            };
        }
        if start.elapsed() > limit {
            eprintln!(
                "[BENCH] lock wait exceeded {}s; proceeding without lock (contention possible)",
                limit.as_secs()
            );
            return BenchLock {
                file: None,
                owned: false,
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Async acquisition: parks the sync wait on spawn_blocking so no tokio
/// worker ever sleeps holding the runtime hostage. Same fd/flock semantics;
/// the guard still releases on drop.
pub async fn acquire_bench_lock_async() -> BenchLock {
    match tokio::task::spawn_blocking(|| acquire_bench_lock_sync(Duration::from_secs(90))).await {
        Ok(g) => g,
        Err(_) => BenchLock {
            file: None,
            owned: false,
        },
    }
}

#[cfg(unix)]
fn flock_try_lock(f: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    // LOCK_EX = 2, LOCK_NB = 4. No new dep: libc already in tree.
    unsafe { libc::flock(f.as_raw_fd(), 2 | 4) == 0 }
}

#[cfg(not(unix))]
fn flock_try_lock(_f: &std::fs::File) -> bool {
    true
}

/// Single non-blocking attempt: true when the lock was free. Test hook and
/// the primitive a blocking-with-deadline caller would loop on.
pub fn acquire_bench_lock_failfast() -> bool {
    let path = std::env::temp_dir().join("subdollar-bench.lock");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    if flock_try_lock(&file) {
        // Hold until end of statement: drop releases immediately — the bool
        // reports freeness, not ownership. Callers needing ownership use
        // acquire_bench_lock (fd held in the guard).
        true
    } else {
        false
    }
}

/// Stable client tag for bench-client container names: same (net, kind)
/// always maps to the same name, so a stale wedged client is reaped
/// pre-start instead of colliding on `--name` reuse.
fn bench_client_tag(net: &str, kind: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    net.hash(&mut h);
    kind.hash(&mut h);
    h.finish()
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
    /// Sync core runs on block_in_place (never parks the runtime event loop
    /// in sleep, but a wedged client still pins a blocking thread — the
    /// async shell below is the path async pipelines must take).
    pub fn run_redis_benchmark_reps(net: &str, host: &str, port: u16, reps: usize) -> Result<f64> {
        let net = net.to_string();
        let host = host.to_string();
        tokio::task::block_in_place(|| {
            let _lock = acquire_bench_lock();
            if !_lock.is_owned() {
                eprintln!("[BENCH] redis window proceeding UNSERIALIZED (lock fail-open)");
            }
            Self::redis_reps_blocking(&net, &host, port, reps)
        })
    }

    /// Async shell: lock wait + every blocking `docker run` window parks on
    /// the blocking pool. A wedged client (5h redis-benchmark, 2026-10-03)
    /// stalls one blocking thread, not a tokio worker serving the API.
    pub async fn run_redis_benchmark_reps_async(
        net: &str,
        host: &str,
        port: u16,
        reps: usize,
    ) -> Result<f64> {
        let net = net.to_string();
        let host = host.to_string();
        tokio::task::spawn_blocking(move || {
            let _lock = acquire_bench_lock();
            if !_lock.is_owned() {
                eprintln!("[BENCH] redis window proceeding UNSERIALIZED (lock fail-open)");
            }
            Self::redis_reps_blocking(&net, &host, port, reps)
        })
        .await
        .map_err(|e| anyhow!("bench blocking pool join failed: {}", e))?
    }

    pub async fn run_redis_benchmark_async(net: &str, host: &str, port: u16) -> Result<f64> {
        Self::run_redis_benchmark_reps_async(net, host, port, BENCH_REPS).await
    }

    fn redis_reps_blocking(net: &str, host: &str, port: u16, reps: usize) -> Result<f64> {
        let mut ok = Vec::new();
        let mut last_err = anyhow!("no benchmark repetitions attempted");
        for _ in 0..reps.max(1) {
            // Named + labeled client: an unnamed --rm client that never
            // exits (5h wizardly_herschel) is invisible to reap_run/prune/
            // orphan-sweep (no label) and --rm never fires. The name below
            // is the exact manual recovery (docker kill + rm -f), mechanized.
            // timeout -k 5 15: SIGTERM demonstrably did not end the docker
            // CLI; KILL after 5s grace bounds every window at ~20s.
            let client = format!("subdollar-benchclient-{:x}", bench_client_tag(net, "redis"));
            let _ = Command::new("docker").args(["rm", "-f", &client]).output();
            let output = Command::new("timeout")
                .args([
                    "-k",
                    "5",
                    "15",
                    "docker",
                    "run",
                    "--rm",
                    "--name",
                    &client,
                    "--label",
                    "sdb.run=bench-client",
                    "--label",
                    "sdb.role=bench-client",
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
            // Reap regardless of outcome: a killed-but-lingering container
            // holds its trial net alive and defeats the orphan rule.
            let _ = Command::new("docker").args(["rm", "-f", &client]).output();

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

    /// Same sync/async split as redis above: sync core on block_in_place,
    /// async shell on spawn_blocking for async pipelines.
    pub fn run_wrk_benchmark_reps(net: &str, host: &str, port: u16, reps: usize) -> Result<f64> {
        let net = net.to_string();
        tokio::task::block_in_place(|| {
            let _lock = acquire_bench_lock();
            if !_lock.is_owned() {
                eprintln!("[BENCH] wrk window proceeding UNSERIALIZED (lock fail-open)");
            }
            Self::wrk_reps_blocking(&net, host, port, reps)
        })
    }

    pub async fn run_wrk_benchmark_reps_async(
        net: &str,
        host: &str,
        port: u16,
        reps: usize,
    ) -> Result<f64> {
        let net = net.to_string();
        let host = host.to_string();
        tokio::task::spawn_blocking(move || {
            let _lock = acquire_bench_lock();
            if !_lock.is_owned() {
                eprintln!("[BENCH] wrk window proceeding UNSERIALIZED (lock fail-open)");
            }
            Self::wrk_reps_blocking(&net, &host, port, reps)
        })
        .await
        .map_err(|e| anyhow!("bench blocking pool join failed: {}", e))?
    }

    pub async fn run_wrk_benchmark_async(net: &str, host: &str, port: u16) -> Result<f64> {
        Self::run_wrk_benchmark_reps_async(net, host, port, BENCH_REPS).await
    }

    fn wrk_reps_blocking(net: &str, host: &str, port: u16, reps: usize) -> Result<f64> {
        let url = format!("http://{host}:{port}/");
        let mut ok = Vec::new();
        let mut last_err = anyhow!("no benchmark repetitions attempted");
        for _ in 0..reps.max(1) {
            let client = format!("subdollar-benchclient-{:x}", bench_client_tag(net, "wrk"));
            let _ = Command::new("docker").args(["rm", "-f", &client]).output();
            let output = Command::new("timeout")
                .args([
                    "-k",
                    "5",
                    "15",
                    "docker",
                    "run",
                    "--rm",
                    "--name",
                    &client,
                    "--label",
                    "sdb.run=bench-client",
                    "--label",
                    "sdb.role=bench-client",
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
            let _ = Command::new("docker").args(["rm", "-f", &client]).output();

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
    // Lock tests share one kernel flock: parallel threads race (holder vs
    // failfast), so a green-alone/red-together flake. Serialize like
    // HIST_SERIAL in batch_ref.
    static LOCK_SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
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
        // 70000 -> 80000 is +14.3%: clean at 0.25, drift at 0.10.
        assert!(!ref_drifted_with_tol(70000.0, 80000.0, 0.25));
        assert!(ref_drifted_with_tol(70000.0, 80000.0, 0.10));
        // +5% is inside the default band and must stay clean, or every wave
        // gets quarantined on ordinary host jitter.
        assert!(!ref_drifted_with_tol(70000.0, 73500.0, 0.10));
        // The boundary is strict: exactly at tolerance is not drift.
        assert!(!ref_drifted_with_tol(100.0, 110.0, 0.10));
    }

    #[test]
    fn test_bench_client_name_is_deterministic() {
        // Same (net, kind) always maps to the same --name, so a stale wedged
        // client is reaped pre-start instead of colliding on reuse; distinct
        // nets never share a name (no cross-trial kill).
        assert_eq!(
            bench_client_tag("net-a", "redis"),
            bench_client_tag("net-a", "redis")
        );
        assert_ne!(
            bench_client_tag("net-a", "redis"),
            bench_client_tag("net-b", "redis")
        );
        assert_ne!(
            bench_client_tag("net-a", "redis"),
            bench_client_tag("net-a", "wrk")
        );
    }

    #[test]
    fn test_bench_lock_acquire_release() {
        let _g = LOCK_SERIAL.lock();
        {
            let _g = acquire_bench_lock();
            let path = std::env::temp_dir().join("subdollar-bench.lock");
            assert!(path.exists() || !_g.owned);
        }
        // Drop released: a fresh acquire must own the lock promptly.
        let g = acquire_bench_lock();
        assert!(g.owned);
    }

    #[test]
    fn test_bench_lock_mutual_exclusion() {
        let _g = LOCK_SERIAL.lock();
        // Two guards cannot both own: second acquire reports unowned quickly
        // (failfast), never blocks 90s, never mtime-races.
        let _g = acquire_bench_lock();
        assert!(_g.owned, "first acquire must own an uncontended lock");
        let start = std::time::Instant::now();
        let second = acquire_bench_lock_failfast();
        assert!(!second, "second holder must not own while first lives");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn test_bench_lock_released_on_drop() {
        let _g = LOCK_SERIAL.lock();
        {
            let _g = acquire_bench_lock();
            assert!(_g.owned);
        }
        // fd close releases LOCK_EX: re-acquire must own immediately.
        let g2 = acquire_bench_lock_failfast();
        assert!(g2, "released lock must be re-acquirable");
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
