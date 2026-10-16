//! Stage 6 (transactions) protocol helpers and invariants
//! (docs/PROPOSAL-sdb-sql.md, "6. Transactions"): cross-connection
//! isolation and a kill with a transaction open. Nested `BEGIN`, bare
//! `COMMIT`/`ROLLBACK` and errors inside a transaction are covered by the
//! generator's probes; these are the parts the differential script cannot
//! express.
//!
//! Lifecycle (kill, restart) arrives as callbacks, as in `crash`.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::time::Duration;

fn ok0(got: Response, what: &str) -> Result<(), String> {
    match got {
        Response::Ok(0) => Ok(()),
        other => Err(format!("{what} answered {other:?}, expected OK 0")),
    }
}

fn ok_n(got: Response, n: u64, what: &str) -> Result<(), String> {
    match got {
        Response::Ok(got) if got == n => Ok(()),
        other => Err(format!("{what} answered {other:?}, expected OK {n}")),
    }
}

/// The keys visible on a fresh connection, in order.
fn visible_keys(addr: &str, timeout: Duration, table: &str) -> Result<Vec<String>, String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("SELECT k FROM {table} ORDER BY k"))? {
        Response::Rows { rows, .. } => Ok(rows.into_iter().flatten().collect()),
        other => Err(format!("SELECT k answered {other:?}")),
    }
}

/// The window a peer read may take; a reader still waiting after it is
/// *blocked*, which a commit must release.
const PEER_WINDOW: Duration = Duration::from_millis(1_000);
/// How long a released reader gets to answer after COMMIT/ROLLBACK.
const RELEASE_WINDOW: Duration = Duration::from_secs(5);

/// What a peer connection saw while A held a transaction open.
enum PeerView {
    /// It answered before the transaction ended.
    Answered(Vec<String>),
    /// It did not answer within [`PEER_WINDOW`]: it is waiting for the
    /// transaction to end (a single global lock design; legal at stage 6,
    /// where a *non-blocking* reader is not required yet).
    Blocked(std::sync::mpsc::Receiver<Result<Vec<String>, String>>),
    /// It answered an error (a lock/busy rejection). The retry after the
    /// transaction ends must succeed.
    Errored,
}

/// Read the visible keys on a fresh connection *without* blocking this
/// thread: the answer comes back through a channel, or the reader turns
/// out to be blocked.
fn peer_read(addr: &str, timeout: Duration, table: &str) -> PeerView {
    let (tx, rx) = std::sync::mpsc::channel();
    let addr = addr.to_string();
    let table = table.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(visible_keys(&addr, timeout, &table));
    });
    match rx.recv_timeout(PEER_WINDOW) {
        Ok(Ok(keys)) => PeerView::Answered(keys),
        Ok(Err(_)) => PeerView::Errored,
        Err(_) => PeerView::Blocked(rx),
    }
}

/// The keys of a blocked reader, after the transaction ended: the commit
/// must release it. What it saw may be the pre-transaction snapshot or the
/// post-commit state; anything else is a failure. Returns the keys it saw.
fn released(
    blocked: std::sync::mpsc::Receiver<Result<Vec<String>, String>>,
) -> Result<Vec<String>, String> {
    match blocked.recv_timeout(RELEASE_WINDOW) {
        Ok(Ok(keys)) => Ok(keys),
        Ok(Err(e)) => Err(format!(
            "the blocked reader failed after the transaction ended: {e}"
        )),
        Err(_) => Err("a reader was still blocked after the transaction ended".to_string()),
    }
}

/// Two connections: uncommitted rows are invisible to the other one, a
/// `COMMIT` makes them visible, a `ROLLBACK` discards them. A peer that
/// blocks (a global lock) is released by the COMMIT/ROLLBACK; one that
/// answers a busy error must succeed on the retry.
pub fn isolation(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    let mut a = TcpCandidate::connect(addr, timeout)?;
    ok0(
        a.exec(&format!("CREATE TABLE {table} (k INTEGER, v TEXT)"))?,
        "CREATE TABLE",
    )?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (0, 'base')"))?,
        1,
        "baseline INSERT",
    )?;

    // BEGIN on A; the row must stay invisible to B until COMMIT.
    ok0(a.exec("BEGIN")?, "BEGIN")?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (1, 'uncommitted')"))?,
        1,
        "uncommitted INSERT",
    )?;
    let blocked = match peer_read(addr, timeout, table) {
        PeerView::Answered(seen) => {
            if seen.iter().any(|k| k == "1") {
                return Err(format!(
                    "an uncommitted row is visible to another connection: {seen:?}"
                ));
            }
            if seen != vec!["0".to_string()] {
                return Err(format!("an unexpected view mid-transaction: {seen:?}"));
            }
            None
        }
        PeerView::Errored => None, // a busy answer is retried after COMMIT
        PeerView::Blocked(rx) => Some(rx),
    };
    ok0(a.exec("COMMIT")?, "COMMIT")?;
    if let Some(rx) = blocked {
        let seen = released(rx)?;
        if seen != vec!["0".to_string()] && seen != vec!["0".to_string(), "1".to_string()] {
            return Err(format!("the released reader saw {seen:?} after COMMIT"));
        }
    }
    let seen = visible_keys(addr, timeout, table)?;
    if seen != vec!["0".to_string(), "1".to_string()] {
        return Err(format!("a committed row is missing: {seen:?}"));
    }

    // BEGIN on A; ROLLBACK must discard the row without any trace.
    ok0(a.exec("BEGIN")?, "second BEGIN")?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (2, 'rolled back')"))?,
        1,
        "second INSERT",
    )?;
    let blocked = match peer_read(addr, timeout, table) {
        PeerView::Answered(seen) => {
            if seen != vec!["0".to_string(), "1".to_string()] {
                return Err(format!("a rolled-back row is visible: {seen:?}"));
            }
            None
        }
        PeerView::Errored => None,
        PeerView::Blocked(rx) => Some(rx),
    };
    ok0(a.exec("ROLLBACK")?, "ROLLBACK")?;
    if let Some(rx) = blocked {
        let seen = released(rx)?;
        if seen != vec!["0".to_string(), "1".to_string()] {
            return Err(format!("the released reader saw {seen:?} after ROLLBACK"));
        }
    }
    let seen = visible_keys(addr, timeout, table)?;
    if seen != vec!["0".to_string(), "1".to_string()] {
        return Err(format!("a rolled-back row survived: {seen:?}"));
    }
    Ok(())
}

/// A kill with a transaction open: the committed baseline survives, the
/// uncommitted row is gone (never half-applied).
pub fn kill_with_open_transaction(
    addr: &str,
    timeout: Duration,
    table: &str,
    kill: &mut dyn FnMut() -> Result<(), String>,
    restart: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut a = TcpCandidate::connect(addr, timeout)?;
    ok0(
        a.exec(&format!("CREATE TABLE {table} (k INTEGER, v TEXT)"))?,
        "CREATE TABLE",
    )?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (0, 'committed')"))?,
        1,
        "committed INSERT",
    )?;
    ok0(a.exec("BEGIN")?, "BEGIN")?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (1, 'doomed')"))?,
        1,
        "doomed INSERT",
    )?;
    // The connection stays open across the kill: the transaction must be
    // discarded by *recovery*, not by a clean client disconnect.
    kill()?;
    restart()?;
    let seen = visible_keys(addr, timeout, table)?;
    if seen != vec!["0".to_string()] {
        return Err(format!(
            "after a kill with an open transaction: {seen:?} (expected the committed row only)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Condvar, Mutex};

    #[derive(Default)]
    struct Db {
        /// A transaction is open (the mock's single global lock).
        in_txn: bool,
        committed: Vec<String>,
        pending: Vec<String>,
    }

    /// A candidate built on one global lock: a reader blocks until the
    /// writer commits or rolls back. The isolation probe must not deadlock
    /// on it (the old synchronous reader did: the commit it waited for
    /// could only happen after the read returned).
    #[test]
    fn a_blocking_reader_is_released_by_the_commit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let db = Arc::new((Mutex::new(Db::default()), Condvar::new()));
        let db2 = db.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                let db = db2.clone();
                std::thread::spawn(move || {
                    let mut sock = sock;
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        let stmt = line.trim_end().to_string();
                        let low = stmt.to_lowercase();
                        let reply = if low.starts_with("create") {
                            "OK 0".to_string()
                        } else if low.starts_with("begin") {
                            let (lock, cv) = &*db;
                            let mut d = lock.lock().unwrap();
                            d.in_txn = true;
                            d.pending.clear();
                            cv.notify_all();
                            "OK 0".to_string()
                        } else if low.starts_with("insert") {
                            let key = stmt
                                .split('(')
                                .nth(1)
                                .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                                .unwrap_or_default()
                                .to_string();
                            let (lock, _) = &*db;
                            let mut d = lock.lock().unwrap();
                            if d.in_txn {
                                d.pending.push(key);
                            } else {
                                d.committed.push(key);
                            }
                            "OK 1".to_string()
                        } else if low.starts_with("select") {
                            let (lock, cv) = &*db;
                            let mut d = lock.lock().unwrap();
                            // The lock: wait for the writer to finish.
                            while d.in_txn {
                                d = cv.wait(d).unwrap();
                            }
                            let rows = d.committed.clone();
                            format!("ROWS 1\nk\n{}\nEND {}", rows.join("\n"), rows.len())
                        } else if low.starts_with("commit") {
                            let (lock, cv) = &*db;
                            let mut d = lock.lock().unwrap();
                            let done: Vec<String> = d.pending.drain(..).collect();
                            d.committed.extend(done);
                            d.in_txn = false;
                            cv.notify_all();
                            "OK 0".to_string()
                        } else if low.starts_with("rollback") {
                            let (lock, cv) = &*db;
                            let mut d = lock.lock().unwrap();
                            d.pending.clear();
                            d.in_txn = false;
                            cv.notify_all();
                            "OK 0".to_string()
                        } else {
                            "ERR syntax".to_string()
                        };
                        if sock.write_all(format!("{reply}\n").as_bytes()).is_err() {
                            return;
                        }
                        line.clear();
                    }
                });
            }
        });
        isolation(&addr, Duration::from_secs(10), "lock_t").expect("isolation through a lock");
    }
}
