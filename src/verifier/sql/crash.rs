//! Stage 5 (crash safety) protocol helpers and invariants
//! (docs/PROPOSAL-sdb-sql.md, "5. Crash safety"): ack-counted SIGKILL, a
//! mid-flight bulk UPDATE, a torn data file and double restarts.
//!
//! Lifecycle (kill, restart, truncate) belongs to whoever owns the
//! process: the pipeline passes callbacks for the candidate container, the
//! reference tests use the host process. The protocol and the invariants
//! live here so both paths test the same thing.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Rows built for the bulk-update probe, in [`BULK_CHUNK`]-row inserts
/// (the spec's "an UPDATE touching 50k rows is killed mid-flight").
pub const BULK_ROWS: usize = 50_000;
pub const BULK_CHUNK: usize = 1_000;

fn expect_ok0(got: Response, what: &str) -> Result<(), String> {
    match got {
        Response::Ok(0) => Ok(()),
        other => Err(format!("{what} answered {other:?}, expected OK 0")),
    }
}

/// Send `inserts` single-row autocommit INSERTs, then SIGKILL right after
/// the ack of the last one. Returns how many rows were acked (a transport
/// error before the last ack just stops the count: those statements are
/// unacked and may be lost).
pub fn ack_kill(
    addr: &str,
    timeout: Duration,
    table: &str,
    inserts: usize,
    kill: &mut dyn FnMut() -> Result<(), String>,
) -> Result<usize, String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    expect_ok0(
        c.exec(&format!("CREATE TABLE {table} (k INTEGER, v TEXT)"))?,
        "CREATE TABLE",
    )?;
    let mut acked = 0usize;
    for k in 0..inserts {
        match c.exec(&format!("INSERT INTO {table} VALUES ({k}, 'v{k}')")) {
            Ok(Response::Ok(1)) => acked += 1,
            Ok(other) => return Err(format!("INSERT {k} answered {other:?}")),
            Err(_) => break,
        }
    }
    drop(c);
    kill()?;
    Ok(acked)
}

/// Commit one more row (the torn-tail victim).
pub fn append_row(addr: &str, timeout: Duration, table: &str, k: usize) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("INSERT INTO {table} VALUES ({k}, 'v{k}')"))? {
        Response::Ok(1) => Ok(()),
        other => Err(format!("victim INSERT answered {other:?}")),
    }
}

/// A torn tail may cost the most recent statement, never older ones: keys
/// `0..keep` must exist in order, with at most `extra` newer keys after
/// them and nothing else missing.
pub fn check_acked_prefix(
    addr: &str,
    timeout: Duration,
    table: &str,
    keep: usize,
    extra: usize,
) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("SELECT k FROM {table} ORDER BY k"))? {
        Response::Rows { rows, .. } => {
            let got: Vec<String> = rows.into_iter().flatten().collect();
            let want: Vec<String> = (0..keep).map(|k| k.to_string()).collect();
            if got.len() < keep || got[..keep] != want[..] {
                return Err(format!(
                    "{} rows came back, the first {keep} were committed (first gap at row {})",
                    got.len(),
                    got.iter()
                        .zip(&want)
                        .position(|(a, b)| a != b)
                        .unwrap_or(keep)
                ));
            }
            if got.len() > keep + extra {
                return Err(format!(
                    "{} rows came back; at most {} were possible",
                    got.len(),
                    keep + extra
                ));
            }
            Ok(())
        }
        other => Err(format!("SELECT k answered {other:?}")),
    }
}

/// Every acked key exists and no un-acked row does: the keys read back in
/// order must be exactly `0..acked`.
pub fn check_acked_rows(
    addr: &str,
    timeout: Duration,
    table: &str,
    acked: usize,
) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("SELECT k FROM {table} ORDER BY k"))? {
        Response::Rows { rows, .. } => {
            let got: Vec<String> = rows.into_iter().flatten().collect();
            let want: Vec<String> = (0..acked).map(|k| k.to_string()).collect();
            if got != want {
                return Err(format!(
                    "{} rows came back, {acked} were acked (first gap at row {})",
                    got.len(),
                    got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(0)
                ));
            }
            Ok(())
        }
        other => Err(format!("SELECT k answered {other:?}")),
    }
}

/// Fill `table` with `rows` rows (`c = 0`) in multi-row INSERTs, send a
/// bulk `UPDATE ... SET c = c + 1` and SIGKILL without waiting for its
/// ack.
pub fn bulk_update_and_kill(
    addr: &str,
    timeout: Duration,
    table: &str,
    rows: usize,
    chunk: usize,
    kill: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    expect_ok0(
        c.exec(&format!("CREATE TABLE {table} (k INTEGER, c INTEGER)"))?,
        "CREATE TABLE",
    )?;
    let mut k = 0usize;
    while k < rows {
        let vals: Vec<String> = (k..(k + chunk).min(rows))
            .map(|i| format!("({i}, 0)"))
            .collect();
        match c.exec(&format!("INSERT INTO {table} VALUES {}", vals.join(", ")))? {
            Response::Ok(n) if n as usize == vals.len() => {}
            other => return Err(format!("bulk INSERT answered {other:?}")),
        }
        k += chunk;
    }
    // In flight when the kill lands; the ack is deliberately not read.
    let _ = c.exec(&format!("UPDATE {table} SET c = c + 1"));
    kill()
}

/// After the kill and a restart the update must be all-or-nothing: one
/// distinct value for `c` (old or new), never a mixture, and every row
/// still there.
pub fn check_uniform(
    addr: &str,
    timeout: Duration,
    table: &str,
    rows: usize,
) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("SELECT DISTINCT c FROM {table} ORDER BY c"))? {
        Response::Rows { rows: distinct, .. } => {
            if distinct.len() != 1 {
                return Err(format!(
                    "{} distinct values after the kill: the update applied partially",
                    distinct.len()
                ));
            }
        }
        other => return Err(format!("SELECT DISTINCT answered {other:?}")),
    }
    match c.exec(&format!("SELECT k FROM {table} ORDER BY k"))? {
        Response::Rows { rows: all, .. } => {
            if all.len() != rows {
                return Err(format!("{} of {rows} rows survived the kill", all.len()));
            }
            Ok(())
        }
        other => Err(format!("SELECT k answered {other:?}")),
    }
}

/// Truncate the newest regular file under `dir` by up to `bytes` — a torn
/// tail, i.e. a partial trailing record. Cut a *small* bound (a few hundred
/// bytes): cutting a whole commit would destroy data that was really
/// flushed, which no design can recover from a truncated file; the check
/// then allows the most recent statement to be lost and requires every
/// older row to be intact. Never empties the file. Returns the bytes cut.
pub fn torn_tail(dir: &Path, bytes: u64) -> Result<u64, String> {
    let newest: Option<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t);
    let Some((_, path)) = newest else {
        return Err(format!("no data file under {}", dir.display()));
    };
    let len = std::fs::metadata(&path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    let cut = bytes.min(len / 2);
    if cut == 0 {
        return Ok(0);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_len(len - cut))
        .map_err(|e| format!("truncate {}: {e}", path.display()))?;
    Ok(cut)
}

#[cfg(test)]
mod tests {
    use super::super::compare::render_expected;
    use super::super::oracle::Oracle;
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// One oracle-backed server with a shared database.
    fn oracle_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let oracle = std::sync::Arc::new(std::sync::Mutex::new(Oracle::new().unwrap()));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                let oracle = oracle.clone();
                std::thread::spawn(move || {
                    let mut sock = sock;
                    sock.set_nodelay(true).unwrap();
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        let out = oracle.lock().unwrap().exec(line.trim_end());
                        let text = if out.is_discard() {
                            "ERR syntax".to_string()
                        } else {
                            render_expected(&out)
                        };
                        if sock.write_all(format!("{text}\n").as_bytes()).is_err() {
                            return;
                        }
                        line.clear();
                    }
                });
            }
        });
        addr
    }

    /// The kill callback the oracle tests use: the server is a thread, so
    /// "killed" means "the next connection is the restarted one" — the
    /// invariants are what matter here.
    fn no_kill() -> impl FnMut() -> Result<(), String> {
        || Ok(())
    }

    #[test]
    fn acked_rows_survive_and_extra_rows_are_caught() {
        let addr = oracle_server().to_string();
        let mut kill = no_kill();
        let acked = ack_kill(&addr, Duration::from_secs(5), "ack_t", 12, &mut kill).unwrap();
        assert_eq!(acked, 12);
        check_acked_rows(&addr, Duration::from_secs(5), "ack_t", 12).expect("all acked rows");
        // A candidate that lost a row, or kept an un-acked one, is caught.
        let err = check_acked_rows(&addr, Duration::from_secs(5), "ack_t", 13).unwrap_err();
        assert!(err.contains("13 were acked"), "{err}");
        let err = check_acked_rows(&addr, Duration::from_secs(5), "ack_t", 11).unwrap_err();
        assert!(err.contains("12 rows came back"), "{err}");
    }

    #[test]
    fn bulk_update_is_uniform_and_keeps_every_row() {
        let addr = oracle_server().to_string();
        let mut kill = no_kill();
        bulk_update_and_kill(
            &addr,
            Duration::from_secs(5),
            "bulk_t",
            2_000,
            500,
            &mut kill,
        )
        .unwrap();
        check_uniform(&addr, Duration::from_secs(5), "bulk_t", 2_000).expect("uniform");
    }

    #[test]
    fn torn_tail_cuts_the_newest_file_and_never_empties_it() {
        let dir = std::env::temp_dir().join(format!("sdb-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.dat");
        std::fs::write(&old, vec![0u8; 64]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let new = dir.join("new.dat");
        std::fs::write(&new, vec![0u8; 100]).unwrap();
        let cut = torn_tail(&dir, 30).unwrap();
        assert_eq!(cut, 30);
        assert_eq!(std::fs::metadata(&new).unwrap().len(), 70);
        assert_eq!(
            std::fs::metadata(&old).unwrap().len(),
            64,
            "only the newest"
        );
        // Tiny files are cut down to at most half, never to zero.
        let tiny = dir.join("tiny.dat");
        std::fs::write(&tiny, vec![0u8; 1]).unwrap();
        assert_eq!(torn_tail(&dir, 4096).unwrap(), 0);
        assert_eq!(std::fs::metadata(&tiny).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
