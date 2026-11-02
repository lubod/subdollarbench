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
