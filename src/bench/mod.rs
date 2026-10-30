//! The host-global benchmark lock.
//!
//! sdb-sql's stage-11 perf gate takes this lock so a timing window is
//! serialized across processes. The retired load-test runner (redis-benchmark
//! and wrk clients) that lived here was deleted in round 43; the lock stays
//! because the perf gate still uses it.

use std::time::{Duration, Instant};

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

/// Stable bench-lock path (holder serialization across processes).
fn bench_lock_path() -> std::path::PathBuf {
    std::env::temp_dir().join("subdollar-bench.lock")
}

fn acquire_bench_lock_sync(limit: Duration) -> BenchLock {
    acquire_bench_lock_sync_at(&bench_lock_path(), limit)
}

/// Same semantics on an explicit path. Tests use a private path so a
/// release/re-acquire property check cannot race a concurrent bench window
/// from another test (that race made `test_bench_lock_released_on_drop`
/// flake under full-suite load).
pub(crate) fn acquire_bench_lock_sync_at(path: &std::path::Path, limit: Duration) -> BenchLock {
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
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
    flock_probe_at(&bench_lock_path())
}

/// Single non-blocking probe on an explicit path (tests).
pub(crate) fn flock_probe_at(path: &std::path::Path) -> bool {
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
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

#[cfg(test)]
mod tests {
    // Lock tests share one kernel flock: parallel threads race (holder vs
    // failfast), so a green-alone/red-together flake. Serialize like
    // HIST_SERIAL in batch_ref.
    static LOCK_SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    use super::*;

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
        // Private lock path: the shared bench lock is legitimately held by
        // live pipeline tests running in parallel; the property under test
        // is release mechanics, not global freeness.
        let path = std::env::temp_dir().join(format!("sdb-lock-test-{}.lock", std::process::id()));
        {
            let _g = super::acquire_bench_lock_sync_at(&path, Duration::from_secs(2));
            assert!(_g.owned);
        }
        // fd close releases LOCK_EX: re-acquire must own immediately.
        let g2 = super::flock_probe_at(&path);
        assert!(g2, "released lock must be re-acquirable");
        let _ = std::fs::remove_file(&path);
    }
}
