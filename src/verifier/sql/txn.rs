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
fn visible_keys(addr: &str, timeout: Duration, table: &str) -> Result<Vec<String>, PeerErr> {
    let mut c = TcpCandidate::connect(addr, timeout).map_err(|e| PeerErr::Other(e.to_string()))?;
    let got = c
        .exec(&format!("SELECT k FROM {table} ORDER BY k"))
        .map_err(PeerErr::Other)?;
    match got {
        Response::Rows { rows, .. } => Ok(rows.into_iter().flatten().collect()),
        // The *class* decides retryability, never the text.
        Response::Err { class, .. } if class == "busy" => Err(PeerErr::Busy),
        other => Err(PeerErr::Other(format!("SELECT k answered {other:?}"))),
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
    Blocked(std::sync::mpsc::Receiver<Result<Vec<String>, PeerErr>>),
    /// It answered `ERR busy` (a lock rejection). The retry after the
    /// transaction ends must succeed.
    Errored,
    /// Any other error: a syntax answer, a dropped connection, a crash.
    /// That is a failure, not something to retry.
    Failed(String),
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
        Ok(Err(PeerErr::Busy)) => PeerView::Errored,
        Ok(Err(PeerErr::Other(e))) => PeerView::Failed(e),
        Err(_) => PeerView::Blocked(rx),
    }
}

/// `visible_keys` for the synchronous reads: a busy answer is not special
/// there, it is just an error string.
fn keys_or_err(addr: &str, timeout: Duration, table: &str) -> Result<Vec<String>, String> {
    visible_keys(addr, timeout, table).map_err(|e| match e {
        PeerErr::Busy => "SELECT k answered ERR busy".to_string(),
        PeerErr::Other(s) => s,
    })
}

/// Why a peer read did not answer: only `Busy` is retryable.
enum PeerErr {
    Busy,
    Other(String),
}

/// The keys of a blocked reader, after the transaction ended: the commit
/// must release it. What it saw may be the pre-transaction snapshot or the
/// post-commit state; anything else is a failure. Returns the keys it saw.
fn released(
    blocked: std::sync::mpsc::Receiver<Result<Vec<String>, PeerErr>>,
) -> Result<Option<Vec<String>>, String> {
    match blocked.recv_timeout(RELEASE_WINDOW) {
        Ok(Ok(keys)) => Ok(Some(keys)),
        // A busy answer after the release is retried with a fresh read.
        Ok(Err(PeerErr::Busy)) => Ok(None),
        Ok(Err(PeerErr::Other(e))) => Err(format!(
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
        PeerView::Failed(e) => {
            return Err(format!(
                "the second connection failed while a transaction was open: {e}"
            ))
        }
        PeerView::Blocked(rx) => Some(rx),
    };
    ok0(a.exec("COMMIT")?, "COMMIT")?;
    if let Some(rx) = blocked {
        if let Some(seen) = released(rx)? {
            if seen != vec!["0".to_string()] && seen != vec!["0".to_string(), "1".to_string()] {
                return Err(format!("the released reader saw {seen:?} after COMMIT"));
            }
        }
    }
    let seen = keys_or_err(addr, timeout, table)?;
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
        PeerView::Failed(e) => {
            return Err(format!(
                "the second connection failed while a transaction was open: {e}"
            ))
        }
        PeerView::Blocked(rx) => Some(rx),
    };
    ok0(a.exec("ROLLBACK")?, "ROLLBACK")?;
    if let Some(rx) = blocked {
        if let Some(seen) = released(rx)? {
            if seen != vec!["0".to_string(), "1".to_string()] {
                return Err(format!("the released reader saw {seen:?} after ROLLBACK"));
            }
        }
    }
    let seen = keys_or_err(addr, timeout, table)?;
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
    let seen = keys_or_err(addr, timeout, table)?;
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

    /// A peer that answers anything but `ERR busy` while a transaction is
    /// open fails the check: a syntax answer or a dropped connection is not
    /// something to retry (finding: every error used to count as busy).
    #[test]
    fn a_non_busy_peer_error_fails_the_isolation_check() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // The transaction state is shared across connections, like a real
        // server's.
        let open = Arc::new(Mutex::new(false));
        let open2 = open.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                let open = open2.clone();
                std::thread::spawn(move || {
                    let mut sock = sock;
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        let low = line.trim().to_ascii_lowercase();
                        let reply = if low.starts_with("begin") {
                            *open.lock().unwrap() = true;
                            "OK 0".to_string()
                        } else if low.starts_with("create") {
                            "OK 0".to_string()
                        } else if low.starts_with("commit") || low.starts_with("rollback") {
                            *open.lock().unwrap() = false;
                            "OK 0".to_string()
                        } else if low.starts_with("select") {
                            if *open.lock().unwrap() {
                                // The bug under test: not a busy answer.
                                "ERR syntax".to_string()
                            } else {
                                "ROWS 1\nk\n0\nEND 1".to_string()
                            }
                        } else {
                            "OK 1".to_string()
                        };
                        if sock.write_all(format!("{reply}\n").as_bytes()).is_err() {
                            return;
                        }
                        line.clear();
                    }
                });
            }
        });
        let err = isolation(&addr, Duration::from_secs(5), "bad_t").unwrap_err();
        assert!(err.contains("failed while a transaction was open"), "{err}");
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
