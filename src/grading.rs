//! R48 F2: one host-wide grading lock and the fast-core pinning.
//!
//! A real cell grades in Docker: it starts the candidate, runs the
//! differential suites and their timed scenarios, and may shrink a failure.
//! Two cells doing that at once make each other's timings (ticket 11's 5 ms
//! medians, crash recovery, disk-full and replication) unreliable. The
//! pipeline takes an exclusive `flock` on `<runs>/.grading.lock` for the
//! whole verification, while the model's session and builds stay parallel.
//!
//! `flock` is released when the descriptor closes, including on a panic or
//! a kill, so a stale file never blocks the next cell. The lock file is
//! never removed: deleting a lock file while a waiter holds it would split
//! the lock.
//!
//! The graded containers (the candidate and the SQL reference) run on the
//! host's fast cores; the agent sandbox runs on the rest. The sets are
//! environment overrides, one per side:
//! `SDB_GRADING_CPUSET` (default `0-3,12-15`) and
//! `SDB_SESSION_CPUSET` (default `4-11,16-23`). A host with fewer cores
//! sets its own.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// An acquired grading lock. Dropping it releases the `flock`.
pub struct GradingLock {
    file: File,
    /// How long this acquisition waited for the previous holder.
    pub wait: Duration,
}

/// Take the exclusive grading lock, blocking until it is free. Call from a
/// blocking context (`spawn_blocking` in the async pipeline).
pub fn acquire(path: &Path) -> std::io::Result<GradingLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let started = Instant::now();
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(GradingLock {
        file,
        wait: started.elapsed(),
    })
}

impl Drop for GradingLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still open; LOCK_UN only releases our
        // lock. Closing the file would release it anyway.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Docker arguments for a set; empty when no set is configured.
pub fn cpuset_args(set: &str) -> Vec<String> {
    crate::sandbox::docker::cpuset_args(set)
}

/// `<SDB_HOME>/runs/.grading.lock` (or `SDB_RUNS_DIR/.grading.lock`).
pub fn lock_path() -> PathBuf {
    crate::report::archive::RunArchiver::resolve_runs_dir().join(".grading.lock")
}

/// The bench host's two sets, only on a host with enough cores. A CI
/// runner or a laptop leaves them empty (Docker then schedules freely);
/// the operator sets the env overrides on a different big host.
fn default_cpusets() -> Option<(String, String)> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    (cores >= 24).then(|| ("0-3,12-15".to_string(), "4-11,16-23".to_string()))
}

/// The cores graded containers are pinned to.
pub fn fast_cpuset() -> String {
    std::env::var("SDB_GRADING_CPUSET")
        .ok()
        .or_else(|| default_cpusets().map(|(fast, _)| fast))
        .unwrap_or_default()
}

/// The cores the agent's sandbox and its builds are pinned to.
pub fn session_cpuset() -> String {
    std::env::var("SDB_SESSION_CPUSET")
        .ok()
        .or_else(|| default_cpusets().map(|(_, slow)| slow))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    fn temp_lock(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sdb-lock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir.join(".grading.lock")
    }

    fn test_exe() -> PathBuf {
        std::env::current_exe().expect("the test binary path")
    }

    /// Child-process arm of the exclusivity/kill tests. It only acts when
    /// the parent sets the env marker, so a normal test run is a no-op.
    #[test]
    fn r48_f2_lock_helper() {
        let Ok(path) = std::env::var("SDB_LOCK_HELPER_PATH") else {
            return;
        };
        let hold_ms = std::env::var("SDB_LOCK_HELPER_HOLD_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100u64);
        let lock = acquire(Path::new(&path)).expect("the child acquires the lock");
        println!("ACQUIRED {}", lock.wait.as_millis());
        std::thread::sleep(Duration::from_millis(hold_ms));
    }

    fn spawn_helper(path: &Path, hold_ms: u64) -> Child {
        Command::new(test_exe())
            .args([
                "--exact",
                "grading::tests::r48_f2_lock_helper",
                "--nocapture",
            ])
            .env("SDB_LOCK_HELPER_PATH", path)
            .env("SDB_LOCK_HELPER_HOLD_MS", hold_ms.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the lock helper")
    }

    fn acquired_ms(child: &mut Child) -> u128 {
        let stdout = child.stdout.take().expect("child stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Some(rest) = line.trim().strip_prefix("ACQUIRED ") {
                return rest.parse().expect("milliseconds");
            }
            line.clear();
        }
        panic!("the lock helper never reported ACQUIRED");
    }

    /// Two processes grade one after the other: the second's acquisition
    /// waits for the first holder's release.
    #[test]
    fn r48_f2_grading_is_exclusive() {
        let path = temp_lock("exclusive");
        let mut first = spawn_helper(&path, 700);
        assert_eq!(acquired_ms(&mut first), 0, "the first holder waits");
        let mut second = spawn_helper(&path, 100);
        let waited = acquired_ms(&mut second);
        assert!(
            waited >= 350,
            "the second process acquired after {waited} ms while the first held for 700 ms"
        );
        let _ = first.wait();
        let _ = second.wait();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A SIGKILLed holder releases the kernel lock; the next acquisition
    /// does not hang on a stale file.
    #[test]
    fn r48_f2_killed_holder_releases() {
        let path = temp_lock("killed");
        let mut child = spawn_helper(&path, 60_000);
        assert_eq!(acquired_ms(&mut child), 0);
        child.kill().expect("kill the holder");
        let _ = child.wait();
        let (tx, rx) = std::sync::mpsc::channel();
        let path2 = path.clone();
        std::thread::spawn(move || {
            let lock = acquire(&path2).expect("acquire after the kill");
            let _ = tx.send(lock.wait);
        });
        let waited = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the killed holder still blocks the lock");
        assert!(
            waited < Duration::from_secs(1),
            "the next acquirer waited {waited:?}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The two default sets are non-empty and disjoint, and the cpuset
    /// argument is a separate `--cpuset-cpus <set>` pair.
    #[test]
    fn r48_f2_cpusets() {
        let fast = fast_cpuset();
        let slow = session_cpuset();
        let expand = |s: &str| -> std::collections::HashSet<u32> {
            let mut out = std::collections::HashSet::new();
            for part in s.split(',') {
                if let Some((a, b)) = part.split_once('-') {
                    let a: u32 = a.parse().unwrap();
                    let b: u32 = b.parse().unwrap();
                    out.extend(a..=b);
                } else {
                    out.insert(part.parse().unwrap());
                }
            }
            out
        };
        if !fast.is_empty() || !slow.is_empty() {
            let f = expand(&fast);
            let s = expand(&slow);
            assert!(!f.is_empty() && !s.is_empty(), "one set is empty");
            assert!(f.is_disjoint(&s), "fast {fast} and session {slow} overlap");
        }
        let args = crate::sandbox::docker::cpuset_args("0-3");
        assert_eq!(args, vec!["--cpuset-cpus".to_string(), "0-3".to_string()]);
        assert!(crate::sandbox::docker::cpuset_args("").is_empty());
        // R48-3: the agent's ref-sql carries the session set, not the
        // grading set.
        let ref_args =
            crate::sandbox::docker::reference_sql_args("i", "ref", "net", "/src:/srv:ro", 5481);
        if slow.is_empty() {
            assert!(!ref_args.iter().any(|a| a == "--cpuset-cpus"));
        } else {
            assert!(
                ref_args
                    .windows(2)
                    .any(|w| w[0] == "--cpuset-cpus" && w[1] == slow),
                "ref-sql must use the session set: {ref_args:?}"
            );
        }
    }
}
