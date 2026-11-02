//! A focused mock of the candidate's wire: enough SQL for the three
//! checks, with the lock models the stage distinguishes. It is not an
//! engine (no parser beyond these forms); it pins the scenario's
//! meaning in both directions.

use super::super::compare::render_response;
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Condvar;
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Serializable: a transaction takes the writer lock at BEGIN; a
    /// bare SELECT reads committed state without the lock, so readers
    /// never block (the full ticket passes).
    SnapshotReaders,
    /// Serializable, but the first UPDATE of every transaction answers
    /// `ERR busy`: the harness must retry.
    BusyFirstUpdate,
    /// One global lock: even bare SELECTs take it, so a reader blocks
    /// while a transaction is open (the lite variant's behavior).
    GlobalLock,
    /// BEGIN/COMMIT are accepted but ignored: read-modify-write races
    /// lose updates.
    Naive,
    /// R58 F1: the first connection's first UPDATE of each transaction
    /// answers busy for its first `BUSY_STREAK` transactions; the
    /// other connections are normal.
    BusyStreak,
    /// R58 F1: every UPDATE answers busy forever.
    AlwaysBusy,
}

/// R59 F1: more than the old 400-try limit, so the old loop still
/// fails, and about half the 10 s budget, so a slow runner passes.
const BUSY_STREAK: usize = 600;

struct Mock {
    mode: Mode,
    data: Mutex<std::collections::HashMap<String, Vec<Vec<i64>>>>,
    held: Mutex<bool>,
    cv: Condvar,
}

impl Mock {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            data: Mutex::new(Default::default()),
            held: Mutex::new(false),
            cv: Condvar::new(),
        }
    }
    fn acquire(&self) {
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.cv.wait(held).unwrap();
        }
        *held = true;
    }
    fn release(&self) {
        let mut held = self.held.lock().unwrap();
        *held = false;
        self.cv.notify_all();
    }
    fn apply(&self, pending: &[(String, i64, i64)]) {
        let mut data = self.data.lock().unwrap();
        for (table, id, value) in pending {
            if let Some(rows) = data.get_mut(table) {
                for row in rows.iter_mut() {
                    if row.first() == Some(id) && row.len() > 1 {
                        row[1] = *value;
                    }
                }
            }
        }
    }
}

struct Conn {
    tx: bool,
    lock_held: bool,
    pending: Vec<(String, i64, i64)>,
    busy_fired: bool,
    busy_left: usize,
}

impl Conn {
    fn new(busy_left: usize) -> Self {
        Self {
            tx: false,
            lock_held: false,
            pending: Vec::new(),
            busy_fired: false,
            busy_left,
        }
    }
}

fn rows_resp(headers: &[&str], rows: Vec<Vec<String>>) -> Response {
    Response::Rows {
        ncols: headers.len(),
        headers: headers.iter().map(|s| s.to_string()).collect(),
        end_count: rows.len() as u64,
        rows,
    }
}

fn err(class: &str, text: &str) -> Response {
    Response::Err {
        class: class.to_string(),
        text: text.to_string(),
    }
}

fn exec(mock: &Mock, conn: &mut Conn, line: &str) -> Response {
    let t = line.trim();
    let low = t.to_lowercase();
    if low == "begin" {
        if conn.tx {
            return err("txn", "nested BEGIN");
        }
        conn.tx = true;
        conn.pending.clear();
        // `busy_fired` is per connection, not per transaction: the first
        // UPDATE of the connection is busy, so a retried transaction
        // succeeds — exactly what the harness's bounded retry must
        // handle.
        if mock.mode != Mode::Naive {
            mock.acquire();
            conn.lock_held = true;
        }
        return Response::Ok(0);
    }
    if low == "commit" || low == "rollback" {
        if low == "commit" {
            mock.apply(&conn.pending);
        }
        conn.pending.clear();
        if conn.lock_held {
            mock.release();
            conn.lock_held = false;
        }
        conn.tx = false;
        return Response::Ok(0);
    }
    if low.starts_with("create table ") {
        let name = t.split_whitespace().nth(2).unwrap_or("t").to_string();
        mock.data.lock().unwrap().insert(name, Vec::new());
        return Response::Ok(0);
    }
    if low.starts_with("insert into ") {
        let name = t.split_whitespace().nth(2).unwrap_or("t").to_string();
        let nums: Vec<i64> = t
            .to_lowercase()
            .split_once("values")
            .map(|(_, rest)| {
                rest.split(|c: char| !(c.is_ascii_digit() || c == '-'))
                    .filter(|s| !s.is_empty() && *s != "-")
                    .filter_map(|s| s.parse::<i64>().ok())
                    .collect()
            })
            .unwrap_or_default();
        let mut data = mock.data.lock().unwrap();
        let table = data.entry(name).or_default();
        let mut added = 0;
        for chunk in nums.chunks(2) {
            if chunk.len() == 2 {
                table.push(chunk.to_vec());
                added += 1;
            }
        }
        return Response::Ok(added);
    }
    if low.starts_with("update ") {
        let parts: Vec<&str> = t.split_whitespace().collect();
        // UPDATE <table> SET <col> = <val> WHERE id = <id>
        let (Some(table), Some(val), Some(id)) = (
            parts.get(1),
            parts.get(5).and_then(|v| v.parse::<i64>().ok()),
            parts.get(9).and_then(|v| v.parse::<i64>().ok()),
        ) else {
            return err("syntax", "mock: unsupported UPDATE");
        };
        if mock.mode == Mode::Naive || !conn.tx {
            mock.apply(&[((*table).to_string(), id, val)]);
            return Response::Ok(1);
        }
        if mock.mode == Mode::BusyFirstUpdate && !conn.busy_fired {
            conn.busy_fired = true;
            return err("busy", "conflict");
        }
        if mock.mode == Mode::BusyStreak && conn.busy_left > 0 {
            conn.busy_left -= 1;
            return err("busy", "conflict");
        }
        if mock.mode == Mode::AlwaysBusy {
            return err("busy", "conflict");
        }
        conn.pending.push(((*table).to_string(), id, val));
        return Response::Ok(1);
    }
    if low.starts_with("select ") {
        // A transaction that already holds the global lock reads under
        // it; a bare reader (no transaction) takes it and blocks while
        // another transaction is open — the lite variant's behavior.
        let locked = mock.mode == Mode::GlobalLock && !conn.lock_held;
        if locked {
            mock.acquire();
        }
        let resp = {
            let data = mock.data.lock().unwrap();
            if low.contains("select id, bal from ") {
                let name = t.split_whitespace().nth(4).unwrap_or("t");
                let mut rows: Vec<Vec<String>> = data
                    .get(name)
                    .map(|rows| {
                        rows.iter()
                            .map(|r| vec![r[0].to_string(), r[1].to_string()])
                            .collect()
                    })
                    .unwrap_or_default();
                rows.sort_by_key(|r| r[0].parse::<i64>().unwrap_or(0));
                rows_resp(&["id", "bal"], rows)
            } else if low.contains(" where id = ") {
                let col = t.split_whitespace().nth(1).unwrap_or("v");
                let name = t.split_whitespace().nth(3).unwrap_or("t");
                let id: i64 = t
                    .split_whitespace()
                    .last()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(-1);
                let value = data
                    .get(name)
                    .and_then(|rows| rows.iter().find(|r| r[0] == id))
                    .map(|r| r[1]);
                match value {
                    Some(v) => rows_resp(&[col], vec![vec![v.to_string()]]),
                    None => rows_resp(&[col], Vec::new()),
                }
            } else {
                err("syntax", "mock: unsupported SELECT")
            }
        };
        if locked {
            mock.release();
        }
        return resp;
    }
    err("syntax", "mock: unsupported statement")
}

/// The mock in a thread, one connection per socket, the given lock model.
fn serve(mode: Mode) -> String {
    let mock = Arc::new(Mock::new(mode));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (next_id, stream) in listener.incoming().enumerate() {
            let Ok(sock) = stream else { break };
            let mock = mock.clone();
            // Connection 0 is the setup connection; the first client
            // is 1. Give that client the BUSY_STREAK.
            let busy_left = if mode == Mode::BusyStreak && next_id == 1 {
                BUSY_STREAK
            } else {
                0
            };
            std::thread::spawn(move || {
                let mut conn = Conn::new(busy_left);
                let mut sock = sock;
                sock.set_nodelay(true).unwrap();
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 {
                    let resp = exec(&mock, &mut conn, &line);
                    let text = render_response(&Ok(resp));
                    if sock.write_all(text.as_bytes()).is_err() || sock.write_all(b"\n").is_err() {
                        break;
                    }
                    line.clear();
                }
                if conn.lock_held {
                    mock.release();
                }
            });
        }
    });
    addr.to_string()
}

fn timeout() -> Duration {
    Duration::from_secs(10)
}

#[test]
fn lost_update_passes_on_a_serializable_engine() {
    let addr = serve(Mode::SnapshotReaders);
    lost_update(&addr, timeout(), "t_lost").unwrap();
}

#[test]
fn lost_update_retries_busy_transactions() {
    let addr = serve(Mode::BusyFirstUpdate);
    lost_update(&addr, timeout(), "t_busy").unwrap();
}

#[test]
fn lost_update_catches_a_naive_engine() {
    let addr = serve(Mode::Naive);
    let err = lost_update(&addr, timeout(), "t_naive").unwrap_err();
    assert!(err.contains("lost update"), "{err}");
}

#[test]
fn transfers_hold_the_invariant() {
    let addr = serve(Mode::SnapshotReaders);
    transfers(&addr, timeout(), "t_xfer").unwrap();
}

#[test]
fn readers_do_not_block_on_a_snapshot_engine() {
    let addr = serve(Mode::SnapshotReaders);
    reader_not_blocked(&addr, timeout(), "t_read").unwrap();
}

#[test]
fn a_global_lock_fails_the_reader_check() {
    let addr = serve(Mode::GlobalLock);
    let err = reader_not_blocked(&addr, timeout(), "t_glock").unwrap_err();
    assert!(err.contains("blocked"), "{err}");
}

#[test]
fn the_lite_variant_passes_a_global_lock_and_skips_reader_checks() {
    let addr = serve(Mode::GlobalLock);
    // Lite: (a) and (b) still hold.
    scenario(&addr, timeout(), 7, true).unwrap();
    // Full: the same engine must fail (c).
    let err = scenario(&addr, timeout(), 8, false).unwrap_err();
    assert!(err.contains("blocked"), "{err}");
}

/// R58 F1: the full-jitter caps are 2, 4, 8, 16, 16 ms; each draw
/// stays within 1 ms and its cap.
#[test]
fn r58_f1_busy_pause_caps() {
    for (tries, cap_ms) in [(1u32, 2u64), (2, 4), (3, 8), (4, 16), (5, 16)] {
        for _ in 0..200 {
            let d = busy_pause(tries);
            assert!(d >= Duration::from_millis(1), "tries {tries}: {d:?}");
            assert!(
                d <= Duration::from_millis(cap_ms),
                "tries {tries}: {d:?} > {cap_ms} ms"
            );
        }
    }
}

/// R58 F1/R59 F1: a 600-transaction busy streak on one connection is
/// retried inside the 10 s budget and the increments still land.
#[test]
fn r58_f1_long_busy_streak_passes() {
    let addr = serve(Mode::BusyStreak);
    lost_update(&addr, timeout(), "t_streak").unwrap();
}

/// R58 F1: a wedged engine still fails, about one budget in.
#[test]
fn r58_f1_wedged_engine_still_fails() {
    let addr = serve(Mode::AlwaysBusy);
    let started = std::time::Instant::now();
    let err =
        lost_update_with_budget(&addr, timeout(), "t_wedged", Duration::from_secs(1)).unwrap_err();
    assert!(err.contains("still busy"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "lost update took {:?}",
        started.elapsed()
    );

    let started = std::time::Instant::now();
    let err =
        transfers_with_budget(&addr, timeout(), "t_wedged_x", Duration::from_secs(1)).unwrap_err();
    assert!(err.contains("still busy"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "transfers took {:?}",
        started.elapsed()
    );
}

/// The soak-shaped repeat: the checks are deterministic, so a handful of
/// repeats in the suite plus the 20/20 manual gate covers flakiness.
#[test]
fn the_scenario_repeats_cleanly() {
    let addr = serve(Mode::SnapshotReaders);
    for i in 0..5 {
        scenario(&addr, timeout(), 100 + i, false).unwrap_or_else(|e| panic!("repeat {i}: {e}"));
    }
}
