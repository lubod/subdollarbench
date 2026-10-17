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
use std::collections::BTreeMap;
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

/// What a crash probe observed: how many statements were acked, and
/// whether the never-read final statement answered before the kill landed.
/// `completed` is a stronger fact than "in flight": the server sent `OK`,
/// so crash safety demands that row survived — the caller checks exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    pub acked: usize,
    pub completed: bool,
}

/// How long the *post-kill* read waits: an answer the server had already
/// written is in the socket buffer and arrives at once, and the kill closes
/// the connection, so 250 ms only matters for a server that is somehow
/// still alive.
const POST_KILL_WINDOW: Duration = Duration::from_millis(250);

/// Send `inserts` acked single-row INSERTs, then one *more* INSERT without
/// reading its answer, and SIGKILL. The connection stays open: the kill
/// must land while that last statement is unanswered.
pub fn ack_kill(
    addr: &str,
    timeout: Duration,
    table: &str,
    inserts: usize,
    kill: &mut dyn FnMut() -> Result<(), String>,
) -> Result<InFlight, String> {
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
    // Sent, and the connection stays open *through* the kill: a server
    // that answers first and fsyncs on disconnect must not get the close
    // (plus the kill latency) to make the row durable.
    c.send(&format!(
        "INSERT INTO {table} VALUES ({inserts}, 'in-flight')"
    ))
    .map_err(|e| format!("in-flight INSERT: {e}"))?;
    kill()?;
    // Read after the kill: an `OK` that is already in the socket buffer was
    // written before the server died, so the statement completed and the
    // row is owed. EOF, a reset or a timeout means it was unanswered when
    // the kill landed — the rep really tested the un-acked tail.
    c.set_read_timeout(POST_KILL_WINDOW)?;
    let completed = matches!(c.recv(), Ok(Response::Ok(_)));
    drop(c);
    Ok(InFlight { acked, completed })
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

/// After a torn tail (or a double restart): every key in `must` exists,
/// every key in `maybe` exists or not, and no other key does. Keys are
/// distinct by construction (the harness inserts each once).
pub fn check_keys(
    addr: &str,
    timeout: Duration,
    table: &str,
    must: &[usize],
    maybe: &[usize],
) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("SELECT k FROM {table} ORDER BY k"))? {
        Response::Rows { rows, .. } => {
            let got: Vec<u128> = rows
                .iter()
                .flatten()
                .map(|v| {
                    v.parse::<u128>()
                        .map_err(|_| format!("key {v:?} is not a number"))
                })
                .collect::<Result<_, _>>()?;
            let want = |k: &usize| *k as u128;
            for k in must {
                if !got.contains(&want(k)) {
                    return Err(format!(
                        "key {k} is missing (committed before the tear and owed)"
                    ));
                }
            }
            for k in &got {
                if !must.iter().any(|m| want(m) == *k) && !maybe.iter().any(|m| want(m) == *k) {
                    return Err(format!("unexpected key {k} came back"));
                }
            }
            // Duplicates would mean two rows with one key.
            let mut sorted = got.clone();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted.len() != got.len() {
                return Err(format!("duplicate keys came back: {got:?}"));
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
/// bulk `UPDATE ... SET c = c + 1` **without reading its answer** and
/// SIGKILL. Returns whether the update was still unanswered when the kill
/// landed (a `false` means the server finished it first, so that rep did
/// not test mid-statement atomicity).
pub fn bulk_update_and_kill(
    addr: &str,
    timeout: Duration,
    table: &str,
    rows: usize,
    chunk: usize,
    kill: &mut dyn FnMut() -> Result<(), String>,
) -> Result<bool, String> {
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
    // Sent, and the socket stays open through the kill (see `ack_kill`).
    c.send(&format!("UPDATE {table} SET c = c + 1"))
        .map_err(|e| format!("UPDATE send: {e}"))?;
    kill()?;
    // An `OK` already in the buffer means the update completed before the
    // kill: it is owed, and the caller must see it applied. EOF/reset is
    // the kill landing mid-statement.
    c.set_read_timeout(POST_KILL_WINDOW)?;
    let completed = matches!(c.recv(), Ok(Response::Ok(_)));
    drop(c);
    Ok(completed)
}

/// After the kill and a restart the update (`c = c + 1`, starting at 0)
/// must be all-or-nothing: one distinct value for `c`, never a mixture, and
/// every row still there. `owed` is the probe's verdict: an `OK` that
/// arrived before the kill means the update was acknowledged, so the new
/// value must be there — a uniform *old* column would be an acked update
/// that was lost.
pub fn check_uniform(
    addr: &str,
    timeout: Duration,
    table: &str,
    rows: usize,
    owed: bool,
) -> Result<String, String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    let value;
    match c.exec(&format!("SELECT DISTINCT c FROM {table} ORDER BY c"))? {
        Response::Rows { rows: distinct, .. } => {
            if distinct.len() != 1 {
                return Err(format!(
                    "{} distinct values after the kill: the update applied partially",
                    distinct.len()
                ));
            }
            value = distinct
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next())
                .unwrap_or_default();
            if owed && value != "1" {
                return Err(format!(
                    "the update was acknowledged before the kill but the column is {value}, \
                     not 1: an acked update was lost"
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
            Ok(value)
        }
        other => Err(format!("SELECT k answered {other:?}")),
    }
}

/// The keys present in `table`, ascending.
pub fn read_keys(addr: &str, timeout: Duration, table: &str) -> Result<Vec<usize>, String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("SELECT k FROM {table} ORDER BY k"))? {
        Response::Rows { rows, .. } => rows
            .iter()
            .flatten()
            .map(|v| {
                v.parse::<usize>()
                    .map_err(|_| format!("key {v:?} is not a number"))
            })
            .collect(),
        other => Err(format!("SELECT k answered {other:?}")),
    }
}

/// Recursively copy `src` to `dst`, regular files and directories only:
/// the source is candidate-controlled, so symlinks are never followed
/// (a link to a host file must not leak it into a snapshot), and FIFOs or
/// devices are skipped instead of blocking a reader. Returns the number of
/// entries skipped.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let mut skipped = 0usize;
    let entries = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = match std::fs::symlink_metadata(&from) {
            Ok(m) => m,
            Err(e) => return Err(format!("stat {}: {e}", from.display())),
        };
        if meta.file_type().is_symlink() {
            skipped += 1;
            continue;
        }
        if meta.is_dir() {
            skipped += copy_tree(&from, &to)?;
        } else if meta.is_file() {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        } else {
            skipped += 1;
        }
    }
    Ok(skipped)
}

/// What a store file looked like before the victim commit: size and mtime.
/// Size alone is not enough — SQLite's WAL reuses its high-water mark and
/// rewrites frames in place, so a commit can grow nothing while still
/// touching the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub mtime: Option<std::time::SystemTime>,
}

/// Stamps of every regular file under `dir` (recursive, symlinks skipped):
/// the baseline for finding out what an acked commit actually wrote.
pub fn snapshot_files(dir: &Path) -> Result<BTreeMap<PathBuf, FileStamp>, String> {
    let mut out = BTreeMap::new();
    if dir.exists() {
        collect_stamps(dir, &mut out)?;
    }
    Ok(out)
}

fn collect_stamps(dir: &Path, out: &mut BTreeMap<PathBuf, FileStamp>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_stamps(&path, out)?;
        } else if meta.is_file() {
            out.insert(
                path,
                FileStamp {
                    len: meta.len(),
                    mtime: meta.modified().ok(),
                },
            );
        }
    }
    Ok(())
}

/// The store file the torn tail should hit: the *log*, never the database
/// itself (truncating a database's tail destroys committed pages; truncating
/// a write-ahead log's tail destroys at most the newest transaction).
/// Preference order:
/// 1. a changed file whose name marks it as a log (`*wal*`, `*log*`,
///    `*journal*`, `*aof*`), newest mtime first;
/// 2. the changed file that grew the most (an append-only store);
/// 3. with several changed files and no growth, the newest-mtime one that
///    is not the largest (a checkpointed database plus a wrapped log).
///
/// `None` means no safe victim: an acked commit changed nothing the harness
/// can reasonably tear, and the caller retries.
pub fn changed_file(
    dir: &Path,
    before: &BTreeMap<PathBuf, FileStamp>,
) -> Result<Option<PathBuf>, String> {
    let mut now = BTreeMap::new();
    collect_stamps(dir, &mut now)?;
    let changed: Vec<(PathBuf, FileStamp, u64)> = now
        .into_iter()
        .filter(|(path, stamp)| before.get(path) != Some(stamp))
        .filter(|(path, _)| {
            !path
                .file_name()
                .map(|n| n.to_string_lossy().ends_with("-shm"))
                .unwrap_or(false)
        })
        .map(|(path, stamp)| {
            let was = before.get(&path).map(|s| s.len).unwrap_or(0);
            let grew = stamp.len.saturating_sub(was);
            (path, stamp, grew)
        })
        .collect();
    let newest = |mut v: Vec<(PathBuf, FileStamp, u64)>| -> Option<PathBuf> {
        v.sort_by(|a, b| {
            b.1.mtime
                .cmp(&a.1.mtime)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        v.into_iter().next().map(|(p, _, _)| p)
    };
    let is_log = |p: &Path| {
        let n = p
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        ["wal", "log", "journal", "aof"]
            .iter()
            .any(|k| n.contains(k))
    };
    let logs: Vec<_> = changed
        .iter()
        .filter(|(p, _, _)| is_log(p))
        .cloned()
        .collect();
    if let Some(p) = newest(logs) {
        return Ok(Some(p));
    }
    let mut grown: Vec<_> = changed.iter().filter(|(_, _, g)| *g > 0).cloned().collect();
    grown.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    if let Some((p, _, _)) = grown.into_iter().next() {
        return Ok(Some(p));
    }
    if changed.len() >= 2 {
        let largest = changed
            .iter()
            .max_by_key(|(_, s, _)| s.len)
            .map(|(p, _, _)| p.clone());
        let rest: Vec<_> = changed
            .iter()
            .filter(|(p, _, _)| Some(p) != largest.as_ref())
            .cloned()
            .collect();
        if let Some(p) = newest(rest) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// Truncate `path` as a torn tail: cut up to `bytes`, but at most half the
/// file, so even a tiny store keeps its older records. Returns the bytes
/// cut (0 when there is nothing safe to cut). The caller must have killed
/// the server first — a running one may rewrite or heal the file. For an
/// append-only log the last record is damaged; for a reset WAL the bytes
/// past the write offset are stale anyway, and recovery must ignore them
/// either way.
pub fn torn_tail(path: &Path, bytes: u64) -> Result<u64, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    let cut = bytes.min(len / 2);
    if cut == 0 {
        return Ok(0);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
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
        let out = ack_kill(&addr, Duration::from_secs(5), "ack_t", 12, &mut kill).unwrap();
        assert_eq!(out.acked, 12);
        // The oracle answers the in-flight INSERT, so it completed and is
        // owed: exactly 13 rows.
        assert!(out.completed, "the oracle answers in time");
        check_acked_rows(&addr, Duration::from_secs(5), "ack_t", 13).expect("all 13 rows");
        // A completed statement that was answered and then lost is caught.
        let err = check_acked_rows(&addr, Duration::from_secs(5), "ack_t", 12).unwrap_err();
        assert!(err.contains("13 rows came back"), "{err}");
        // Before the in-flight statement it would be a prefix of 12.
        check_acked_prefix(&addr, Duration::from_secs(5), "ack_t", 12, 1).expect("prefix");
    }

    /// After a torn tail only the newest victim may be missing: an older
    /// committed victim that vanished is caught.
    #[test]
    fn only_the_newest_victim_may_vanish() {
        let addr = oracle_server().to_string();
        let mut c = TcpCandidate::connect(&addr, Duration::from_secs(5)).unwrap();
        c.exec("CREATE TABLE torn_t (k INTEGER, v TEXT)").unwrap();
        for k in 0..4 {
            c.exec(&format!("INSERT INTO torn_t VALUES ({k}, 'v{k}')"))
                .unwrap();
        }
        let must = [0usize, 1, 2];
        let maybe = [3usize];
        check_keys(&addr, Duration::from_secs(5), "torn_t", &must, &maybe).expect("all present");
        // The newest (key 3) may vanish.
        c.exec("DELETE FROM torn_t WHERE k = 3").unwrap();
        check_keys(&addr, Duration::from_secs(5), "torn_t", &must, &maybe).expect("victim lost");
        // An older committed key may not.
        c.exec("DELETE FROM torn_t WHERE k = 1").unwrap();
        let err = check_keys(&addr, Duration::from_secs(5), "torn_t", &must, &maybe).unwrap_err();
        assert!(err.contains("committed before the tear"), "{err}");
        // Nor may an unexpected key appear.
        c.exec("INSERT INTO torn_t VALUES (1, 'v1')").unwrap();
        c.exec("INSERT INTO torn_t VALUES (9, 'x')").unwrap();
        let err = check_keys(&addr, Duration::from_secs(5), "torn_t", &must, &maybe).unwrap_err();
        assert!(err.contains("unexpected key"), "{err}");
    }

    /// The `owed` rule: an UPDATE whose `OK` arrived before the kill must
    /// survive — a uniform *old* column is an acked update that was lost.
    #[test]
    fn an_owed_update_must_be_applied() {
        let addr = oracle_server().to_string();
        let mut c = TcpCandidate::connect(&addr, Duration::from_secs(5)).unwrap();
        c.exec("CREATE TABLE owed_t (k INTEGER, c INTEGER)")
            .unwrap();
        c.exec("INSERT INTO owed_t VALUES (0, 0), (1, 0)").unwrap();
        // Before the update: owed => the new value is required (it fails),
        // not owed => either value is fine.
        let err = check_uniform(&addr, Duration::from_secs(5), "owed_t", 2, true).unwrap_err();
        assert!(err.contains("acked update was lost"), "{err}");
        check_uniform(&addr, Duration::from_secs(5), "owed_t", 2, false).expect("either value");
        c.exec("UPDATE owed_t SET c = c + 1").unwrap();
        check_uniform(&addr, Duration::from_secs(5), "owed_t", 2, true).expect("applied");
    }

    /// A kill that closes the connection before the statement's answer is
    /// the in-flight case: the post-kill read sees EOF, so nothing is owed.
    #[test]
    fn a_kill_that_closes_the_answer_is_in_flight() {
        use std::sync::mpsc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (close_tx, close_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { break };
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                // Answer the CREATE, then hold the in-flight statement and
                // close without answering it when the kill arrives.
                if matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    let _ = sock.write_all(b"OK 0\n");
                }
                line.clear();
                if matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    let _ = close_rx.recv();
                }
            }
        });
        let mut kill = || {
            let _ = close_tx.send(());
            Ok(())
        };
        let out = ack_kill(&addr, Duration::from_secs(5), "closing_t", 0, &mut kill).unwrap();
        assert!(
            !out.completed,
            "an answer that never arrived must not be owed"
        );
    }

    /// A kill that leaves an already-written answer in the buffer is the
    /// completed case (the row is owed and must survive).
    #[test]
    fn an_answer_written_before_the_kill_is_owed() {
        let addr = oracle_server().to_string();
        let mut kill = no_kill();
        let out = ack_kill(&addr, Duration::from_secs(5), "answered_t", 1, &mut kill).unwrap();
        assert!(out.completed, "the oracle answered before the kill");
        check_acked_rows(&addr, Duration::from_secs(5), "answered_t", out.acked + 1)
            .expect("the owed row survives");
    }

    #[test]
    fn changed_file_finds_the_commit_and_torn_tail_cuts_safely() {
        let dir = std::env::temp_dir().join(format!("sdb-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("old.dat"), vec![0u8; 64]).unwrap();
        let nested = dir.join("sub").join("new.dat");
        std::fs::write(&nested, vec![0u8; 100]).unwrap();
        // The sqlite shared-memory sidecar changes on every commit and must
        // never be picked as the victim.
        std::fs::write(dir.join("x-shm"), vec![0u8; 10]).unwrap();
        let before = snapshot_files(&dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // The "commit": 30 bytes appended to the nested file; the -shm is
        // touched too, more recently.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&nested)
            .unwrap()
            .write_all(&[7u8; 30])
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("x-shm"), vec![0u8; 11]).unwrap();
        let path = changed_file(&dir, &before)
            .unwrap()
            .expect("the commit changed a file");
        assert_eq!(path, nested, "a nested store is found, -shm is skipped");
        // At most half the file, never empty.
        assert_eq!(torn_tail(&path, 256).unwrap(), 65);
        assert_eq!(std::fs::metadata(&nested).unwrap().len(), 65);
        assert_eq!(
            std::fs::metadata(dir.join("old.dat")).unwrap().len(),
            64,
            "only the changed file is cut"
        );
        // A database + WAL store: the log is preferred even when the
        // database was touched more recently and is not bigger.
        let db2 = dir.join("store.sqlite");
        let wal2 = dir.join("store.sqlite-wal");
        std::fs::write(&db2, vec![0u8; 4000]).unwrap();
        std::fs::write(&wal2, vec![0u8; 200]).unwrap();
        let before2 = snapshot_files(&dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&wal2, vec![1u8; 200]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&db2, vec![0u8; 4000]).unwrap();
        let picked = changed_file(&dir, &before2).unwrap().expect("a victim");
        assert_eq!(picked, wal2, "the log is torn, not the database");
        // A commit that changed nothing is a failure the caller sees.
        let now = snapshot_files(&dir).unwrap();
        assert!(changed_file(&dir, &now).unwrap().is_none());
        // Files too small to tear report 0 (the caller retries).
        let tiny = dir.join("tiny.dat");
        std::fs::write(&tiny, vec![0u8; 1]).unwrap();
        assert_eq!(torn_tail(&tiny, 4096).unwrap(), 0);
        assert_eq!(std::fs::metadata(&tiny).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The snapshot/copy never follows a symlink out of the store (a
    /// candidate must not be able to leak or snapshot host files), and it
    /// skips specials (a FIFO in the store must not block the reader).
    #[test]
    fn copy_tree_skips_symlinks_and_specials() {
        let root = std::env::temp_dir().join(format!("sdb-copytree-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("sdb-copytree-dst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/f1"), b"one").unwrap();
        std::fs::write(root.join("a/b/f2"), b"two").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", root.join("a/link")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("a/dirlink")).unwrap();
        let fifo = root.join("a/fifo");
        let made_fifo = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        let skipped = copy_tree(&root, &dst).unwrap();
        assert_eq!(skipped, if made_fifo { 3 } else { 2 });
        assert_eq!(std::fs::read(dst.join("a/f1")).unwrap(), b"one");
        assert_eq!(std::fs::read(dst.join("a/b/f2")).unwrap(), b"two");
        assert!(!dst.join("a/link").exists(), "no symlink is recreated");
        assert!(!dst.join("a/dirlink").exists());
        if made_fifo {
            assert!(!dst.join("a/fifo").exists());
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// A server that answers slowly is *unanswered* when the kill lands: the
    /// send-only path reports `in_flight`, the old exec-then-kill path would
    /// have waited out the answer first (and reported `false`).
    #[test]
    fn slow_answers_are_in_flight_when_killed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                std::thread::spawn(move || {
                    let mut sock = sock;
                    sock.set_nodelay(true).unwrap();
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        let slow =
                            line.to_lowercase().contains("update") || line.contains("'in-flight'");
                        if slow {
                            std::thread::sleep(Duration::from_millis(300));
                        }
                        let reply = if line.to_lowercase().starts_with("create") {
                            "OK 0\n".to_string()
                        } else if line.to_lowercase().starts_with("select") {
                            "ROWS 1\nk\n0\n1\nEND 2\n".to_string()
                        } else {
                            format!("OK {}\n", line.matches('(').count().max(1))
                        };
                        if sock.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                        line.clear();
                    }
                });
            }
        });
        let mut kill = no_kill();
        let out = ack_kill(&addr, Duration::from_secs(5), "slow_t", 2, &mut kill).unwrap();
        assert!(
            !out.completed,
            "the slow-answering INSERT must still be unanswered"
        );
        let mut kill = no_kill();
        let completed =
            bulk_update_and_kill(&addr, Duration::from_secs(5), "slow_b", 4, 2, &mut kill).unwrap();
        assert!(!completed, "the bulk UPDATE must be unanswered");
    }
}
