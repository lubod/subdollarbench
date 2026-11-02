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
//! A *second* write while the volume is full is not required: an engine
//! that preallocates its journal can persist a small row legitimately.
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
            fail_len: 64 * 1024,
            retry_len: 64 * 1024,
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

fn read_all(c: &mut TcpCandidate, what: &str) -> Result<Vec<Vec<String>>, String> {
    expect_rows(c.exec("SELECT id, v FROM t")?, what)
}

fn row_count(rows: &[Vec<String>]) -> usize {
    rows.len()
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
        c.exec("CREATE TABLE t (id INTEGER, v TEXT)")?,
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
    let seeded = row_count(&read_all(&mut c, "seed readback")?);
    if seeded != cfg.seed_rows as usize {
        return Err(format!(
            "seed row count is {seeded}, expected {}",
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

    // The failed write must not be visible, and the connection must stay
    // usable for reads. (A second write while full is deliberately not
    // required: an engine with a preallocated journal can persist a small
    // row legitimately — round-15 M4/LOW — so the contract only says the
    // write that cannot fit is refused.)
    let after = row_count(&read_all(&mut c, "rows after ENOSPC")?);
    if after != seeded {
        return Err(format!(
            "the rejected write changed the row count: {seeded} -> {after}"
        ));
    }
    // Every acked seed row still reads back byte-for-byte.
    let mut rows = expect_rows(c.exec("SELECT id, v FROM t")?, "seed readback")?;
    rows.sort_by_key(|r| r.first().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0));
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
    let rows = read_all(&mut c, "rows after retry")?;
    if rows.len() != cfg.seed_rows as usize + 1 {
        return Err(format!(
            "the retry after freeing space did not persist ({} rows, expected {})",
            rows.len(),
            cfg.seed_rows + 1
        ));
    }
    let want = "r".repeat(cfg.retry_len);
    let got = rows
        .iter()
        .find(|r| r.first().map(String::as_str) == Some("1000"))
        .and_then(|r| r.get(1));
    if got != Some(&want) {
        return Err(format!(
            "the retried row is not readable exactly ({})",
            got.map(|v| format!("{} bytes", v.len()))
                .unwrap_or_else(|| "missing".to_string())
        ));
    }

    Ok(format!(
        "seed {} rows; ENOSPC refused the write and never applied it; \
         reads stayed intact; the retry succeeded after the filler was removed",
        cfg.seed_rows
    ))
}

#[cfg(test)]
mod tests;
