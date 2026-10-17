//! Stage 11's perf gate (docs/PROPOSAL-sdb-sql.md, "11. Indexes").
//!
//! The differential suite proves an index changes no answer; this proves it
//! changes *speed*. Two identical tables, one indexed: the same 500 point
//! lookups alternate between the scan and the index, so drift in the host or
//! the candidate moves both medians together. The index must stay under the
//! p50 budget and beat the scan by the ratio. The full variant gates range
//! predicates the same way; the lite variant ("equality lookups only")
//! skips them. A restart between two measurement rounds pins the proposal's
//! "indexes survive restart": a candidate that kept its index only in
//! memory degrades to scan speed and fails the second round.
//!
//! The caller owns the bench lock: these are wall-clock numbers on a shared
//! host.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::time::{Duration, Instant};

/// Rows per table. A point lookup without an index scans all of them.
pub const ROWS: u64 = 200_000;
/// Rows per `INSERT` statement (`ROWS / BATCH` statements per table).
const BATCH: u64 = 500;
/// Point lookups per table per round.
const POINT_LOOKUPS: usize = 500;
/// Range windows per table (full variant).
const RANGES: usize = 100;
/// Rows per range window.
const RANGE_ROWS: u64 = 64;

/// The index's median point lookup must be under this.
pub const POINT_BUDGET_MS: f64 = 5.0;
/// The index's median range lookup must be under this (full variant).
pub const RANGE_BUDGET_MS: f64 = 5.0;
/// ... and at least this much faster than the scan's median.
pub const MIN_SPEEDUP: f64 = 10.0;

fn as_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The median of the durations, in milliseconds. `v` must not be empty.
fn p50(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

struct Measured {
    scan: f64,
    indexed: f64,
}

impl Measured {
    fn speedup(&self) -> f64 {
        self.scan / self.indexed.max(0.001)
    }
}

fn check(m: &Measured, what: &str, budget_ms: f64) -> Result<(), String> {
    if m.indexed > budget_ms {
        return Err(format!(
            "{what}: indexed p50 {:.2} ms is over the {budget_ms:.0} ms budget \
             (scan p50 {:.1} ms, {:.1}x)",
            m.indexed,
            m.scan,
            m.speedup()
        ));
    }
    if m.speedup() < MIN_SPEEDUP {
        return Err(format!(
            "{what}: indexed p50 {:.2} ms is only {:.1}x faster than the scan's \
             {:.1} ms (want {MIN_SPEEDUP:.0}x, budget {budget_ms:.0} ms)",
            m.indexed,
            m.speedup(),
            m.scan
        ));
    }
    Ok(())
}

/// Run the gate against the server at `addr`. `lite` drops the range
/// windows; `restart` (the pipeline's kill/start, keeping the store) adds a
/// second point round that proves the index survived it. `Err` is a gate
/// failure (or an unreachable server); `Ok` carries the measured numbers
/// for the log.
pub fn perf_gate(
    addr: &str,
    timeout: Duration,
    seed: u64,
    lite: bool,
    restart: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<String, String> {
    let mut c = TcpCandidate::connect(addr, timeout).map_err(|e| format!("connect: {e}"))?;

    // Unique names: a re-run with the same seed (a repair attempt on the
    // same store) bumps the suffix until the creates stick.
    let mut tag = format!("zzp{seed:x}");
    let (scan, idx) = 'naming: loop {
        let scan = format!("{tag}s");
        let idx = format!("{tag}f");
        let mut fresh = true;
        for t in [&scan, &idx] {
            let sql = format!("CREATE TABLE {t} (k INTEGER, v TEXT)");
            match c.exec(&sql).map_err(|e| format!("{sql}: {e}"))? {
                Response::Ok(0) => {}
                Response::Err { ref class, .. } if class == "exists" => {
                    fresh = false;
                    break;
                }
                other => return Err(format!("{sql}: {other:?}")),
            }
        }
        if fresh {
            break 'naming (scan, idx);
        }
        tag.push('x');
        if tag.len() > 32 {
            return Err("no free perf table name after 24 tries".to_string());
        }
    };

    for t in [&scan, &idx] {
        let mut k = 0u64;
        while k < ROWS {
            let end = (k + BATCH).min(ROWS);
            let mut vals = String::with_capacity((end - k) as usize * 12);
            for i in k..end {
                if i > k {
                    vals.push(',');
                }
                vals.push_str(&format!("({i},'r{i}')"));
            }
            let sql = format!("INSERT INTO {t} (k, v) VALUES {vals}");
            match c.exec(&sql).map_err(|e| format!("insert into {t}: {e}"))? {
                Response::Ok(n) if n == end - k => {}
                other => {
                    return Err(format!(
                        "insert into {t}: expected {} rows, got {other:?}",
                        end - k
                    ))
                }
            }
            k = end;
        }
    }
    let isql = format!("CREATE INDEX {idx}_k ON {idx} (k)");
    match c.exec(&isql).map_err(|e| format!("{isql}: {e}"))? {
        Response::Ok(0) => {}
        other => return Err(format!("{isql}: {other:?}")),
    }

    // The queries are fixed (a stride coprime with `ROWS`), so the
    // measurement does not depend on an RNG version.
    let points = |c: &mut TcpCandidate| -> Result<Measured, String> {
        let mut scan_ms = Vec::with_capacity(POINT_LOOKUPS);
        let mut idx_ms = Vec::with_capacity(POINT_LOOKUPS);
        for i in 0..POINT_LOOKUPS {
            let k = (i as u64 * 7919) % ROWS;
            let want = format!("r{k}");
            for (t, out) in [(&scan, &mut scan_ms), (&idx, &mut idx_ms)] {
                let sql = format!("SELECT v FROM {t} WHERE k = {k}");
                let t0 = Instant::now();
                let r = c.exec(&sql).map_err(|e| format!("{sql}: {e}"))?;
                out.push(as_ms(t0.elapsed()));
                match &r {
                    Response::Rows { rows, .. }
                        if rows.len() == 1 && rows[0].first() == Some(&want) => {}
                    other => {
                        return Err(format!("{sql}: expected one row {want:?}, got {other:?}"))
                    }
                }
            }
        }
        Ok(Measured {
            scan: p50(&mut scan_ms),
            indexed: p50(&mut idx_ms),
        })
    };
    let ranges = |c: &mut TcpCandidate| -> Result<Measured, String> {
        let mut scan_ms = Vec::with_capacity(RANGES);
        let mut idx_ms = Vec::with_capacity(RANGES);
        for i in 0..RANGES {
            let a = (i as u64 * 1993) % (ROWS - RANGE_ROWS);
            let sql = |t: &str| {
                format!(
                    "SELECT v FROM {t} WHERE k >= {a} AND k < {} ORDER BY k",
                    a + RANGE_ROWS
                )
            };
            for (t, out) in [(&scan, &mut scan_ms), (&idx, &mut idx_ms)] {
                let q = sql(t);
                let t0 = Instant::now();
                let r = c.exec(&q).map_err(|e| format!("{q}: {e}"))?;
                out.push(as_ms(t0.elapsed()));
                match &r {
                    Response::Rows { rows, .. }
                        if rows.len() == RANGE_ROWS as usize
                            && rows[0].first() == Some(&format!("r{a}"))
                            && rows[rows.len() - 1].first()
                                == Some(&format!("r{}", a + RANGE_ROWS - 1)) => {}
                    other => {
                        return Err(format!(
                            "{q}: expected {RANGE_ROWS} rows r{a}..r{}, got {other:?}",
                            a + RANGE_ROWS - 1
                        ))
                    }
                }
            }
        }
        Ok(Measured {
            scan: p50(&mut scan_ms),
            indexed: p50(&mut idx_ms),
        })
    };

    let mut notes: Vec<String> = Vec::new();
    let p = points(&mut c)?;
    check(&p, "point lookups", POINT_BUDGET_MS)?;
    notes.push(format!(
        "point p50 {:.1} ms scan vs {:.2} ms index ({:.0}x)",
        p.scan,
        p.indexed,
        p.speedup()
    ));
    if !lite {
        let r = ranges(&mut c)?;
        check(&r, "range lookups", RANGE_BUDGET_MS)?;
        notes.push(format!(
            "range p50 {:.1} ms vs {:.2} ms ({:.0}x)",
            r.scan,
            r.indexed,
            r.speedup()
        ));
    }
    if let Some(restart) = restart {
        restart()?;
        // The old connection died with the process: reconnect before the
        // second round (a stale socket reads as a candidate failure).
        c = TcpCandidate::connect(addr, timeout)
            .map_err(|e| format!("reconnect after the restart: {e}"))?;
        let p2 = points(&mut c)?;
        check(&p2, "point lookups after the restart", POINT_BUDGET_MS)?;
        notes.push(format!(
            "after restart {:.1} ms vs {:.2} ms ({:.0}x)",
            p2.scan,
            p2.indexed,
            p2.speedup()
        ));
    }
    Ok(notes.join("; "))
}
