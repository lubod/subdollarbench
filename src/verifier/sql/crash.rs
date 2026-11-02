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
mod tests;
