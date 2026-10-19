//! Stage 17 (incident: disk full): the candidate's `--data` directory lives
//! on a small tmpfs (docs/PROPOSAL-sdb-sql.md, "17. Disk full").
//!
//! The pipeline starts the candidate on the tmpfs, then this scenario:
//!
//! 1. seeds a few acked rows;
//! 2. asks the harness to **fill** the volume with a foreign filler file
//!    (the candidate must ignore it);
//! 3. sends a write far larger than the free space: it must answer
//!    `ERR full`, must not be acked, and must leave the connection usable;
//! 4. re-reads every seed row (no corruption / no crash);
//! 5. asks the harness to **free** the filler, then retries the same write:
//!    it must succeed **without a restart**, and the new row must read back
//!    exactly.
//!
//! The harness owns the tmpfs, so "free space" is deterministic rather than
//! dependent on the engine shrinking its files.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::time::Duration;

/// What the harness must do to the candidate's data volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskOp {
    /// Create a filler file that consumes the remaining space.
    Fill,
    /// Remove the filler file (freeing the space again).
    Free,
}

/// Scenario shape; the live incident uses `full()`, the unit tests shrink it.
#[derive(Debug, Clone)]
pub struct DiskConfig {
    pub seed_rows: u32,
    pub seed_len: usize,
    pub fail_len: usize,
    pub retry_len: usize,
    pub timeout: Duration,
}

impl DiskConfig {
    pub fn full() -> Self {
        Self {
            seed_rows: 40,
            seed_len: 64 * 1024,
            fail_len: 2 * 1024 * 1024,
            retry_len: 1024 * 1024,
            timeout: Duration::from_secs(60),
        }
    }

    /// The seed value for a row (kept in memory for the corruption check).
    fn seed_value(&self, i: u32) -> String {
        let mut v = format!("seed-{i}-");
        let want = self.seed_len.max(v.len());
        v.push_str(&"s".repeat(want - v.len()));
        v
    }
}

fn expect_ok(resp: Response, what: &str) -> Result<(), String> {
    match resp {
        Response::Ok(_) => Ok(()),
        other => Err(format!("{what}: expected OK, got {other:?}")),
    }
}

fn expect_rows(resp: Response, what: &str) -> Result<Vec<Vec<String>>, String> {
    match resp {
        Response::Rows { rows, .. } => Ok(rows),
        other => Err(format!("{what}: expected ROWS, got {other:?}")),
    }
}

fn one_int(resp: Response, what: &str) -> Result<i64, String> {
    let rows = expect_rows(resp, what)?;
    let cell = rows
        .first()
        .and_then(|r| r.first())
        .ok_or_else(|| format!("{what}: no scalar row"))?;
    cell.parse::<i64>()
        .map_err(|e| format!("{what}: {cell:?} is not an integer: {e}"))
}

/// Run the incident. `control` must fill/free the candidate's data volume;
/// the pipeline implements it with `docker exec` on the tmpfs.
pub fn incident(
    addr: &str,
    cfg: &DiskConfig,
    control: &mut dyn FnMut(DiskOp) -> Result<(), String>,
) -> Result<String, String> {
    let mut c = TcpCandidate::connect(addr, cfg.timeout)?;
    expect_ok(
        c.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")?,
        "CREATE TABLE",
    )?;
    for i in 0..cfg.seed_rows {
        expect_ok(
            c.exec(&format!(
                "INSERT INTO t VALUES ({i}, '{}')",
                cfg.seed_value(i)
            ))?,
            &format!("seed INSERT {i}"),
        )?;
    }
    let seeded = one_int(c.exec("SELECT COUNT(*) FROM t")?, "seed COUNT")?;
    if seeded != cfg.seed_rows as i64 {
        return Err(format!(
            "seed COUNT is {seeded}, expected {}",
            cfg.seed_rows
        ));
    }

    control(DiskOp::Fill)?;
    let failed = c.exec(&format!(
        "INSERT INTO t VALUES (1000, '{}')",
        "f".repeat(cfg.fail_len)
    ))?;
    match failed {
        Response::Err { class, .. } if class == "full" => {}
        Response::Ok(_) => {
            return Err(
                "a write larger than the free space was acked (it cannot have been persisted)"
                    .to_string(),
            )
        }
        other => {
            return Err(format!(
                "the write with a full volume answered {other:?}; expected ERR full"
            ))
        }
    }

    // The failed write must not be visible, and the connection must still
    // work: a small write while full is also refused (no silent ack).
    let after = one_int(c.exec("SELECT COUNT(*) FROM t")?, "COUNT after ENOSPC")?;
    if after != cfg.seed_rows as i64 {
        return Err(format!(
            "the rejected write changed the row count: {seeded} -> {after}"
        ));
    }
    match c.exec("INSERT INTO t VALUES (1001, 'g')")? {
        Response::Err { class, .. } if class == "full" => {}
        other => {
            return Err(format!(
                "a write while full answered {other:?}; expected ERR full (the connection must stay usable)"
            ))
        }
    }
    // Every acked seed row still reads back byte-for-byte.
    let rows = expect_rows(c.exec("SELECT id, v FROM t ORDER BY id")?, "seed readback")?;
    if rows.len() != cfg.seed_rows as usize {
        return Err(format!(
            "seed readback returned {} rows, expected {}",
            rows.len(),
            cfg.seed_rows
        ));
    }
    for (i, row) in rows.iter().enumerate() {
        let want = cfg.seed_value(i as u32);
        if row.get(1) != Some(&want) {
            return Err(format!(
                "seed row {i} was corrupted while the volume was full (got {:?}, expected {} bytes)",
                row.get(1).map(|v| v.len()),
                want.len()
            ));
        }
    }

    control(DiskOp::Free)?;
    let retry = format!(
        "INSERT INTO t VALUES (1000, '{}')",
        "r".repeat(cfg.retry_len)
    );
    expect_ok(c.exec(&retry)?, "retry after freeing space")?;
    let after = one_int(c.exec("SELECT COUNT(*) FROM t")?, "COUNT after retry")?;
    if after != cfg.seed_rows as i64 + 1 {
        return Err(format!(
            "the retry after freeing space did not persist (COUNT {after}, expected {})",
            cfg.seed_rows + 1
        ));
    }
    let got = expect_rows(c.exec("SELECT v FROM t WHERE id = 1000")?, "retry readback")?;
    let want = "r".repeat(cfg.retry_len);
    if got.len() != 1 || got[0].first() != Some(&want) {
        return Err(format!(
            "the retried row is not readable exactly ({} rows, {})",
            got.len(),
            got.first()
                .and_then(|r| r.first())
                .map(|v| format!("{} bytes", v.len()))
                .unwrap_or_else(|| "no value".to_string())
        ));
    }

    Ok(format!(
        "seed {} rows; ENOSPC refused the write and never applied it; \
         reads stayed intact; the retry succeeded after the filler was removed",
        cfg.seed_rows
    ))
}

#[cfg(test)]
mod tests {
    //! A tiny in-memory mock of the scenario's SQL. Failure modes corrupt
    //! one incident property at a time: acking an unpersisted write,
    //! applying a refused write, or damaging a seed row under ENOSPC.

    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Good,
        AcksFull,
        AppliesRefused,
        CorruptsSeed,
    }

    struct State {
        rows: HashMap<i64, String>,
        full: bool,
    }

    fn rows_response(rows: Vec<Vec<String>>) -> String {
        let mut out = format!("ROWS {}\n", rows.first().map(|r| r.len()).unwrap_or(1));
        out.push_str("value\n");
        for r in &rows {
            out.push_str(&r.join("\t"));
            out.push('\n');
        }
        out.push_str(&format!("END {}\n", rows.len()));
        out
    }

    fn handle(state: &mut State, mode: Mode, sql: &str) -> String {
        let low = sql.to_ascii_lowercase();
        if low.starts_with("create table") {
            return "OK 0\n".to_string();
        }
        if low.starts_with("insert into t values (") {
            let tail = sql
                .trim_end_matches(')')
                .rsplit_once('(')
                .map(|(_, t)| t)
                .unwrap_or_default();
            let id = tail
                .split(',')
                .next()
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(-1);
            let value = tail
                .split_once('\'')
                .and_then(|(_, rest)| rest.strip_suffix('\''))
                .unwrap_or("")
                .to_string();
            if state.full {
                if mode == Mode::AcksFull {
                    state.rows.insert(id, value);
                    return "OK 1\n".to_string();
                }
                if mode == Mode::AppliesRefused {
                    state.rows.insert(id, value);
                }
                if mode == Mode::CorruptsSeed {
                    state.rows.insert(0, "corrupted".to_string());
                }
                return "ERR full no space left on device\n".to_string();
            }
            state.rows.insert(id, value);
            return "OK 1\n".to_string();
        }
        if low.starts_with("select count(*)") {
            return rows_response(vec![vec![state.rows.len().to_string()]]);
        }
        if low.starts_with("select v from t where id = ") {
            let id = low
                .split("id = ")
                .nth(1)
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(-1);
            let rows = state
                .rows
                .get(&id)
                .map(|v| vec![vec![v.clone()]])
                .unwrap_or_default();
            return rows_response(rows);
        }
        if low.starts_with("select id, v from t order by id") {
            let mut rows: Vec<Vec<String>> = state
                .rows
                .iter()
                .map(|(id, v)| vec![id.to_string(), v.clone()])
                .collect();
            rows.sort_by_key(|r| r[0].parse::<i64>().unwrap_or(0));
            return rows_response(rows);
        }
        format!("ERR syntax mock does not understand {sql:.80}\n")
    }

    fn spawn(mode: Mode) -> (String, Arc<AtomicBool>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let full = Arc::new(AtomicBool::new(false));
        let full2 = full.clone();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut state = State {
                rows: HashMap::new(),
                full: false,
            };
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                state.full = full2.load(Ordering::SeqCst);
                let response = handle(&mut state, mode, line.trim_end());
                if writer.write_all(response.as_bytes()).is_err() {
                    break;
                }
                line.clear();
            }
        });
        (addr, full, handle)
    }

    fn cfg() -> DiskConfig {
        DiskConfig {
            seed_rows: 5,
            seed_len: 16,
            fail_len: 64,
            retry_len: 32,
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn the_incident_passes_against_a_faithful_engine() {
        let (addr, full, h) = spawn(Mode::Good);
        let mut control = |op: DiskOp| {
            full.store(op == DiskOp::Fill, Ordering::SeqCst);
            Ok(())
        };
        let report = incident(&addr, &cfg(), &mut control).expect("faithful mock");
        assert!(report.contains("retry succeeded"), "{report}");
        assert!(!full.load(Ordering::SeqCst));
        h.join().unwrap();
    }

    #[test]
    fn acking_an_unpersisted_write_fails() {
        let (addr, full, h) = spawn(Mode::AcksFull);
        let mut control = |op: DiskOp| {
            full.store(op == DiskOp::Fill, Ordering::SeqCst);
            Ok(())
        };
        let err = incident(&addr, &cfg(), &mut control).unwrap_err();
        assert!(err.contains("acked"), "{err}");
        h.join().unwrap();
    }

    #[test]
    fn applying_a_refused_write_fails() {
        let (addr, full, h) = spawn(Mode::AppliesRefused);
        let mut control = |op: DiskOp| {
            full.store(op == DiskOp::Fill, Ordering::SeqCst);
            Ok(())
        };
        let err = incident(&addr, &cfg(), &mut control).unwrap_err();
        assert!(err.contains("changed the row count"), "{err}");
        h.join().unwrap();
    }

    #[test]
    fn corrupting_a_seed_row_fails() {
        let (addr, full, h) = spawn(Mode::CorruptsSeed);
        let mut control = |op: DiskOp| {
            full.store(op == DiskOp::Fill, Ordering::SeqCst);
            Ok(())
        };
        let err = incident(&addr, &cfg(), &mut control).unwrap_err();
        assert!(err.contains("corrupted"), "{err}");
        h.join().unwrap();
    }
}
