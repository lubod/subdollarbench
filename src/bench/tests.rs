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
