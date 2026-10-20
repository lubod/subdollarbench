//! Stage 12 (concurrency and isolation): behavior the differential
//! generator cannot express (docs/PROPOSAL-sdb-sql.md, "12. Concurrency and
//! isolation").
//!
//! Required outcome: serializable. A conflicting transaction may answer
//! `ERR busy`; the harness retries it (bounded). No operation may hang.
//!
//! - (a) lost update: 16 clients × 50 `BEGIN; SELECT; UPDATE v = <read+1>;
//!   COMMIT` leave the counter at exactly 800.
//! - (b) transfers: concurrent balance moves keep the sum constant in every
//!   snapshot a reader takes.
//! - (c) full only: with a write transaction open on A, a `SELECT` on B
//!   returns the pre-transaction value within 1 s (readers do not block on
//!   writers). The lite variant (one global lock) passes (a) and (b), not
//!   (c).

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// (a): the proposal's shape.
pub const CLIENTS: usize = 16;
pub const INCREMENTS: usize = 50;
/// (b): four accounts, four writers, three snapshot readers.
const ACCOUNTS: i64 = 4;
const START_BALANCE: i64 = 1_000;
const WRITERS: usize = 4;
const TRANSFERS_PER_WRITER: usize = 40;
const TRANSFER: i64 = 10;
const READERS: usize = 3;
const MIN_SNAPSHOTS: usize = 10;
/// A busy transaction is retried; a wedged server is not.
const RETRIES: usize = 400;
const RETRY_PAUSE: Duration = Duration::from_millis(2);
/// A writer pauses after each transfer so the readers' snapshots overlap
/// the transfers however fast the engine is (round-18 N4): without it a
/// fast engine finished all 160 before the readers took 10 snapshots.
const WRITER_PAUSE: Duration = Duration::from_millis(1);
/// (c): the reader's window while a writer holds its transaction open.
const READER_WINDOW: Duration = Duration::from_secs(1);

enum TxErr {
    /// `ERR busy`: the transaction conflicted and may be retried.
    Busy,
    /// Anything else: a real failure.
    Fail(String),
}

fn begin(c: &mut TcpCandidate) -> Result<(), TxErr> {
    match c.exec("BEGIN") {
        Ok(Response::Ok(0)) => Ok(()),
        Ok(Response::Err { class, .. }) if class == "busy" => Err(TxErr::Busy),
        Ok(other) => Err(TxErr::Fail(format!("BEGIN answered {other:?}"))),
        Err(e) => Err(TxErr::Fail(e)),
    }
}

fn commit(c: &mut TcpCandidate) -> Result<(), TxErr> {
    match c.exec("COMMIT") {
        Ok(Response::Ok(0)) => Ok(()),
        Ok(Response::Err { class, .. }) if class == "busy" => Err(TxErr::Busy),
        Ok(other) => Err(TxErr::Fail(format!("COMMIT answered {other:?}"))),
        Err(e) => Err(TxErr::Fail(e)),
    }
}

fn rollback(c: &mut TcpCandidate) {
    let _ = c.exec("ROLLBACK");
}

fn select_one_int(c: &mut TcpCandidate, sql: &str) -> Result<i64, TxErr> {
    match c.exec(sql) {
        Ok(Response::Rows { rows, .. }) if rows.len() == 1 && rows[0].len() == 1 => rows[0][0]
            .parse::<i64>()
            .map_err(|e| TxErr::Fail(format!("{sql}: non-integer cell {:?} ({e})", rows[0][0]))),
        Ok(Response::Rows { rows, .. }) => Err(TxErr::Fail(format!(
            "{sql}: expected exactly one row, got {rows:?}"
        ))),
        Ok(Response::Err { class, .. }) if class == "busy" => Err(TxErr::Busy),
        Ok(other) => Err(TxErr::Fail(format!("{sql} answered {other:?}"))),
        Err(e) => Err(TxErr::Fail(e)),
    }
}

/// The balances of every account, in the order the server returned them.
fn select_balances(c: &mut TcpCandidate, sql: &str) -> Result<Vec<i64>, TxErr> {
    match c.exec(sql) {
        Ok(Response::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                let cell = r
                    .last()
                    .ok_or_else(|| TxErr::Fail(format!("{sql}: empty row")))?;
                cell.parse::<i64>()
                    .map_err(|e| TxErr::Fail(format!("{sql}: non-integer cell {cell:?} ({e})")))
            })
            .collect(),
        Ok(Response::Err { class, .. }) if class == "busy" => Err(TxErr::Busy),
        Ok(other) => Err(TxErr::Fail(format!("{sql} answered {other:?}"))),
        Err(e) => Err(TxErr::Fail(e)),
    }
}

fn update_expect_one(c: &mut TcpCandidate, sql: &str) -> Result<(), TxErr> {
    match c.exec(sql) {
        Ok(Response::Ok(1)) => Ok(()),
        Ok(Response::Err { class, .. }) if class == "busy" => Err(TxErr::Busy),
        Ok(other) => Err(TxErr::Fail(format!(
            "{sql} answered {other:?}, expected OK 1"
        ))),
        Err(e) => Err(TxErr::Fail(e)),
    }
}

fn expect_ok(c: &mut TcpCandidate, sql: &str, n: u64) -> Result<(), String> {
    match c.exec(sql) {
        Ok(Response::Ok(got)) if got == n => Ok(()),
        other => Err(format!("{sql} answered {other:?}, expected OK {n}")),
    }
}

fn busy_text(e: TxErr) -> String {
    match e {
        TxErr::Busy => "ERR busy".to_string(),
        TxErr::Fail(s) => s,
    }
}

/// (a) One read-modify-write transaction. The read happens *inside* the
/// transaction and the write uses the value read, so an engine that does
/// not serialize (or detect the conflict) overwrites a concurrent update.
fn increment(c: &mut TcpCandidate, table: &str) -> Result<(), TxErr> {
    begin(c)?;
    let v = match select_one_int(c, &format!("SELECT v FROM {table} WHERE id = 1")) {
        Ok(v) => v,
        Err(e) => {
            rollback(c);
            return Err(e);
        }
    };
    if let Err(e) = update_expect_one(c, &format!("UPDATE {table} SET v = {} WHERE id = 1", v + 1))
    {
        rollback(c);
        return Err(e);
    }
    match commit(c) {
        Ok(()) => Ok(()),
        Err(e) => {
            rollback(c);
            Err(e)
        }
    }
}

/// (a) 16 clients × 50 increments must leave exactly 800.
pub fn lost_update(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    let mut setup = TcpCandidate::connect(addr, timeout)?;
    expect_ok(
        &mut setup,
        &format!("CREATE TABLE {table} (id INTEGER, v INTEGER)"),
        0,
    )?;
    expect_ok(&mut setup, &format!("INSERT INTO {table} VALUES (1, 0)"), 1)?;
    drop(setup);

    let mut handles = Vec::new();
    for _ in 0..CLIENTS {
        let addr = addr.to_string();
        let table = table.to_string();
        handles.push(std::thread::spawn(move || -> Result<(), String> {
            let mut c = TcpCandidate::connect(&addr, timeout)
                .map_err(|e| format!("client connect: {e}"))?;
            for i in 0..INCREMENTS {
                let mut tries = 0;
                loop {
                    tries += 1;
                    match increment(&mut c, &table) {
                        Ok(()) => break,
                        Err(TxErr::Busy) if tries < RETRIES => {
                            std::thread::sleep(RETRY_PAUSE);
                        }
                        Err(TxErr::Busy) => {
                            return Err(format!(
                                "increment {i}: still busy after {RETRIES} retries"
                            ))
                        }
                        Err(TxErr::Fail(e)) => return Err(format!("increment {i}: {e}")),
                    }
                }
            }
            Ok(())
        }));
    }
    for h in handles {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("a lost-update client panicked".to_string()),
        }
    }

    let mut c = TcpCandidate::connect(addr, timeout)?;
    let v = select_one_int(&mut c, &format!("SELECT v FROM {table} WHERE id = 1"))
        .map_err(busy_text)?;
    let want = (CLIENTS * INCREMENTS) as i64;
    if v != want {
        return Err(format!(
            "lost update: final v = {v}, expected {want} ({} increments were overwritten)",
            want - v
        ));
    }
    Ok(())
}

/// (b) Concurrent transfers keep `sum(bal)` at 4000 in every snapshot.
pub fn transfers(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    let mut setup = TcpCandidate::connect(addr, timeout)?;
    expect_ok(
        &mut setup,
        &format!("CREATE TABLE {table} (id INTEGER, bal INTEGER)"),
        0,
    )?;
    let rows: Vec<String> = (0..ACCOUNTS)
        .map(|i| format!("({i}, {START_BALANCE})"))
        .collect();
    expect_ok(
        &mut setup,
        &format!("INSERT INTO {table} VALUES {}", rows.join(", ")),
        ACCOUNTS as u64,
    )?;
    drop(setup);

    let done = Arc::new(AtomicBool::new(false));
    let violations = Arc::new(AtomicUsize::new(0));
    let snapshots = Arc::new(AtomicUsize::new(0));

    let mut readers = Vec::new();
    for _ in 0..READERS {
        let addr = addr.to_string();
        let table = table.to_string();
        let done = done.clone();
        let violations = violations.clone();
        let snapshots = snapshots.clone();
        readers.push(std::thread::spawn(move || -> Result<(), String> {
            let mut c = TcpCandidate::connect(&addr, timeout)
                .map_err(|e| format!("reader connect: {e}"))?;
            while !done.load(Ordering::SeqCst) {
                match begin(&mut c) {
                    Ok(()) => {}
                    Err(TxErr::Busy) => {
                        std::thread::sleep(RETRY_PAUSE);
                        continue;
                    }
                    Err(TxErr::Fail(e)) => return Err(format!("reader BEGIN: {e}")),
                }
                let sum =
                    select_balances(&mut c, &format!("SELECT id, bal FROM {table} ORDER BY id"))
                        .map(|b| b.iter().sum::<i64>());
                match sum {
                    Ok(s) => {
                        if s != ACCOUNTS * START_BALANCE {
                            violations.fetch_add(1, Ordering::SeqCst);
                        }
                        snapshots.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(TxErr::Busy) => {}
                    Err(TxErr::Fail(e)) => return Err(format!("reader SELECT: {e}")),
                }
                // A read-only transaction: commit it (or discard on busy).
                match commit(&mut c) {
                    Ok(()) | Err(TxErr::Busy) => {}
                    Err(TxErr::Fail(e)) => return Err(format!("reader COMMIT: {e}")),
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }));
    }

    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let addr = addr.to_string();
        let table = table.to_string();
        writers.push(std::thread::spawn(move || -> Result<(), String> {
            let mut c = TcpCandidate::connect(&addr, timeout)
                .map_err(|e| format!("writer connect: {e}"))?;
            for i in 0..TRANSFERS_PER_WRITER {
                let from = (w + i) % ACCOUNTS as usize;
                let to = (from + 1 + (i % (ACCOUNTS as usize - 1))) % ACCOUNTS as usize;
                let mut tries = 0;
                loop {
                    tries += 1;
                    match transfer_once(&mut c, &table, from as i64, to as i64) {
                        Ok(()) => {
                            std::thread::sleep(WRITER_PAUSE);
                            break;
                        }
                        Err(TxErr::Busy) if tries < RETRIES => {
                            std::thread::sleep(RETRY_PAUSE);
                        }
                        Err(TxErr::Busy) => {
                            return Err(format!("transfer {i}: still busy after {RETRIES} retries"))
                        }
                        Err(TxErr::Fail(e)) => return Err(format!("transfer {i}: {e}")),
                    }
                }
            }
            Ok(())
        }));
    }
    for h in writers {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                done.store(true, Ordering::SeqCst);
                return Err(e);
            }
            Err(_) => return Err("a transfer writer panicked".to_string()),
        }
    }
    done.store(true, Ordering::SeqCst);
    for h in readers {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("a snapshot reader panicked".to_string()),
        }
    }

    let snaps = snapshots.load(Ordering::SeqCst);
    let bad = violations.load(Ordering::SeqCst);
    if bad > 0 {
        return Err(format!(
            "{bad} of {snaps} snapshots broke the transfer invariant (sum != {})",
            ACCOUNTS * START_BALANCE
        ));
    }
    if snaps < MIN_SNAPSHOTS {
        return Err(format!(
            "only {snaps} snapshots were taken (expected at least {MIN_SNAPSHOTS}); a reader could not make progress"
        ));
    }
    let mut c = TcpCandidate::connect(addr, timeout)?;
    let total: i64 = select_balances(&mut c, &format!("SELECT id, bal FROM {table} ORDER BY id"))
        .map_err(busy_text)?
        .iter()
        .sum();
    if total != ACCOUNTS * START_BALANCE {
        return Err(format!(
            "the final total is {total}, expected {}",
            ACCOUNTS * START_BALANCE
        ));
    }
    Ok(())
}

fn transfer_once(c: &mut TcpCandidate, table: &str, from: i64, to: i64) -> Result<(), TxErr> {
    begin(c)?;
    let read = |c: &mut TcpCandidate, id: i64| {
        select_one_int(c, &format!("SELECT bal FROM {table} WHERE id = {id}"))
    };
    let a = match read(c, from) {
        Ok(v) => v,
        Err(e) => {
            rollback(c);
            return Err(e);
        }
    };
    let b = match read(c, to) {
        Ok(v) => v,
        Err(e) => {
            rollback(c);
            return Err(e);
        }
    };
    if let Err(e) = update_expect_one(
        c,
        &format!(
            "UPDATE {table} SET bal = {} WHERE id = {from}",
            a - TRANSFER
        ),
    ) {
        rollback(c);
        return Err(e);
    }
    if let Err(e) = update_expect_one(
        c,
        &format!("UPDATE {table} SET bal = {} WHERE id = {to}", b + TRANSFER),
    ) {
        rollback(c);
        return Err(e);
    }
    match commit(c) {
        Ok(()) => Ok(()),
        Err(e) => {
            rollback(c);
            Err(e)
        }
    }
}

/// (c) Full only: a reader answers the pre-transaction value within 1 s
/// while a writer holds its transaction open. A global-lock engine (the
/// lite variant) blocks the reader, which is exactly what this rejects.
pub fn reader_not_blocked(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    let mut setup = TcpCandidate::connect(addr, timeout)?;
    expect_ok(
        &mut setup,
        &format!("CREATE TABLE {table} (id INTEGER, v INTEGER)"),
        0,
    )?;
    expect_ok(&mut setup, &format!("INSERT INTO {table} VALUES (1, 0)"), 1)?;
    drop(setup);

    let mut writer = TcpCandidate::connect(addr, timeout)?;
    begin(&mut writer).map_err(busy_text)?;
    if let Err(e) = update_expect_one(
        &mut writer,
        &format!("UPDATE {table} SET v = 1 WHERE id = 1"),
    ) {
        rollback(&mut writer);
        return Err(busy_text(e));
    }

    // The peer read runs in a thread so a blocked reader is a timeout, not a
    // hung harness.
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let addr = addr.to_string();
        let table = table.to_string();
        std::thread::spawn(move || {
            let res = TcpCandidate::connect(&addr, timeout)
                .map_err(|e| e.to_string())
                .and_then(|mut c| {
                    c.exec(&format!("SELECT v FROM {table} WHERE id = 1"))
                        .map_err(|e| e.to_string())
                });
            let _ = tx.send(res);
        });
    }
    let answer = match rx.recv_timeout(READER_WINDOW) {
        Ok(Ok(Response::Rows { rows, .. })) if rows.len() == 1 && rows[0].len() == 1 => {
            rows[0][0].clone()
        }
        Ok(Ok(Response::Err { class, text })) => {
            rollback(&mut writer);
            return Err(format!(
                "a reader was refused while a writer held its transaction open: ERR {class} {text}"
            ));
        }
        Ok(Ok(other)) => {
            rollback(&mut writer);
            return Err(format!("a reader answered {other:?}"));
        }
        Ok(Err(e)) => {
            rollback(&mut writer);
            return Err(format!("a reader failed: {e}"));
        }
        Err(_) => {
            rollback(&mut writer);
            return Err(format!(
                "a reader blocked for more than {READER_WINDOW:?} on an open write transaction"
            ));
        }
    };
    rollback(&mut writer);
    if answer != "0" {
        return Err(format!(
            "a reader saw {answer:?} while the write was uncommitted; expected the pre-transaction 0"
        ));
    }
    let mut c = TcpCandidate::connect(addr, timeout)?;
    let v = select_one_int(&mut c, &format!("SELECT v FROM {table} WHERE id = 1"))
        .map_err(busy_text)?;
    if v != 0 {
        return Err(format!("after ROLLBACK a reader saw {v}, expected 0"));
    }
    Ok(())
}

/// The whole stage-12 contract. `lite` (one global lock) skips (c).
pub fn scenario(addr: &str, timeout: Duration, seed: u64, lite: bool) -> Result<(), String> {
    let ns = format!("c{seed:x}");
    lost_update(addr, timeout, &format!("{ns}_lost"))?;
    transfers(addr, timeout, &format!("{ns}_xfer"))?;
    if !lite {
        reader_not_blocked(addr, timeout, &format!("{ns}_read"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
    }

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
    }

    impl Conn {
        fn new() -> Self {
            Self {
                tx: false,
                lock_held: false,
                pending: Vec::new(),
                busy_fired: false,
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
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                let mock = mock.clone();
                std::thread::spawn(move || {
                    let mut conn = Conn::new();
                    let mut sock = sock;
                    sock.set_nodelay(true).unwrap();
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap() > 0 {
                        let resp = exec(&mock, &mut conn, &line);
                        let text = render_response(&Ok(resp));
                        if sock.write_all(text.as_bytes()).is_err()
                            || sock.write_all(b"\n").is_err()
                        {
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

    /// The soak-shaped repeat: the checks are deterministic, so a handful of
    /// repeats in the suite plus the 20/20 manual gate covers flakiness.
    #[test]
    fn the_scenario_repeats_cleanly() {
        let addr = serve(Mode::SnapshotReaders);
        for i in 0..5 {
            scenario(&addr, timeout(), 100 + i, false)
                .unwrap_or_else(|e| panic!("repeat {i}: {e}"));
        }
    }
}
