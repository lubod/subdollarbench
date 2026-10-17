//! Stage 11's perf gate (docs/PROPOSAL-sdb-sql.md, "11. Indexes").
//!
//! The differential suite proves an index changes no answer; this proves it
//! changes *speed*. Two identical tables, one indexed: the same point
//! lookups alternate between the scan and the index, so drift in the host or
//! the candidate moves both medians together. The index must stay under the
//! p50 budget and either beat the scan by the ratio **or** sit at the
//! round-trip floor: an indexed lookup can never be faster than one TCP
//! round trip, and a compiled engine can scan 200k rows faster than ten of
//! them, so a working index on a fast scan would otherwise fail. The full
//! variant gates range predicates the same way; the lite variant
//! ("equality lookups only") skips them. A restart between two measurement
//! rounds pins the proposal's "indexes survive restart": a candidate that
//! kept its index only in memory degrades to scan speed and fails the
//! second round.
//!
//! The caller owns the bench lock (wall-clock numbers on a shared host) and
//! the scratch store: the gate writes two `ROWS`-row tables and the dialect
//! has no `DROP TABLE`.

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
/// An indexed median within this multiple of the round-trip floor passes
/// without the ratio: an index that is as fast as an empty query is an
/// index, and the network, not the candidate, is the limit.
pub const FLOOR_FACTOR: f64 = 2.0;

/// The gate's sizes, so the unit tests can run a fake engine cheaply.
#[derive(Clone, Copy)]
struct Params {
    rows: u64,
    batch: u64,
    lookups: usize,
    ranges: usize,
    range_rows: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            rows: ROWS,
            batch: BATCH,
            lookups: POINT_LOOKUPS,
            ranges: RANGES,
            range_rows: RANGE_ROWS,
        }
    }
}

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
    /// The round-trip floor, measured on a one-row table on the same
    /// connection (`SELECT v FROM <tiny> WHERE k = 0`).
    floor: f64,
}

impl Measured {
    fn speedup(&self) -> f64 {
        self.scan / self.indexed.max(0.001)
    }

    fn at_floor(&self) -> bool {
        self.indexed <= FLOOR_FACTOR * self.floor
    }

    /// The ratio, labelled when it passed on the floor rule instead.
    fn ratio_note(&self) -> String {
        if self.speedup() >= MIN_SPEEDUP {
            format!("{:.0}x", self.speedup())
        } else if self.at_floor() {
            format!("{:.1}x, floor {:.2} ms", self.speedup(), self.floor)
        } else {
            format!("{:.1}x", self.speedup())
        }
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
    if m.speedup() < MIN_SPEEDUP && !m.at_floor() {
        return Err(format!(
            "{what}: indexed p50 {:.2} ms is only {:.1}x faster than the scan's \
             {:.1} ms and above {FLOOR_FACTOR:.0}x the round-trip floor {:.2} ms \
             (want {MIN_SPEEDUP:.0}x or <= {FLOOR_FACTOR:.0}x floor, budget {budget_ms:.0} ms)",
            m.indexed,
            m.speedup(),
            m.scan,
            m.floor
        ));
    }
    Ok(())
}

/// Run the gate against the server at `addr`. `lite` drops the range
/// windows; `restart` (the pipeline's graceful stop/start, keeping the
/// store) adds a second point round that proves the index survived it.
/// `Err` is a gate failure (or an unreachable server); `Ok` carries the
/// measured numbers for the log.
pub fn perf_gate(
    addr: &str,
    timeout: Duration,
    seed: u64,
    lite: bool,
    restart: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<String, String> {
    perf_gate_with(Params::default(), addr, timeout, seed, lite, restart)
}

fn perf_gate_with(
    params: Params,
    addr: &str,
    timeout: Duration,
    seed: u64,
    lite: bool,
    restart: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<String, String> {
    let mut c = TcpCandidate::connect(addr, timeout).map_err(|e| format!("connect: {e}"))?;

    // Unique names: a re-run with the same seed (a repair attempt on the
    // same scratch store) bumps the suffix until the creates stick.
    let mut tag = format!("zzp{seed:x}");
    let (scan, idx, floor_t) = 'naming: loop {
        let scan = format!("{tag}s");
        let idx = format!("{tag}f");
        let floor_t = format!("{tag}o");
        let mut fresh = true;
        for t in [&scan, &idx, &floor_t] {
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
            break 'naming (scan, idx, floor_t);
        }
        tag.push('x');
        if tag.len() > 32 {
            return Err("no free perf table name after 24 tries".to_string());
        }
    };

    for t in [&scan, &idx] {
        let mut k = 0u64;
        while k < params.rows {
            let end = (k + params.batch).min(params.rows);
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
    let fsql = format!("INSERT INTO {floor_t} (k, v) VALUES (0,'r0')");
    match c.exec(&fsql).map_err(|e| format!("{fsql}: {e}"))? {
        Response::Ok(1) => {}
        other => return Err(format!("{fsql}: {other:?}")),
    }
    let isql = format!("CREATE INDEX {idx}_k ON {idx} (k)");
    match c.exec(&isql).map_err(|e| format!("{isql}: {e}"))? {
        Response::Ok(0) => {}
        other => return Err(format!("{isql}: {other:?}")),
    }

    // The queries are fixed (a stride coprime with `rows`), so the
    // measurement does not depend on an RNG version. The floor query runs
    // interleaved on the same connection.
    let points = |c: &mut TcpCandidate| -> Result<Measured, String> {
        let mut scan_ms = Vec::with_capacity(params.lookups);
        let mut idx_ms = Vec::with_capacity(params.lookups);
        let mut floor_ms = Vec::with_capacity(params.lookups);
        for i in 0..params.lookups {
            let k = (i as u64 * 7919) % params.rows;
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
            let sql = format!("SELECT v FROM {floor_t} WHERE k = 0");
            let t0 = Instant::now();
            let r = c.exec(&sql).map_err(|e| format!("{sql}: {e}"))?;
            floor_ms.push(as_ms(t0.elapsed()));
            match &r {
                Response::Rows { rows, .. }
                    if rows.len() == 1 && rows[0].first() == Some(&"r0".to_string()) => {}
                other => return Err(format!("{sql}: expected the one row, got {other:?}")),
            }
        }
        Ok(Measured {
            scan: p50(&mut scan_ms),
            indexed: p50(&mut idx_ms),
            floor: p50(&mut floor_ms),
        })
    };
    let ranges = |c: &mut TcpCandidate| -> Result<Measured, String> {
        let mut scan_ms = Vec::with_capacity(params.ranges);
        let mut idx_ms = Vec::with_capacity(params.ranges);
        let mut floor_ms = Vec::with_capacity(params.ranges);
        for i in 0..params.ranges {
            let a = (i as u64 * 1993) % (params.rows - params.range_rows);
            let sql = |t: &str| {
                format!(
                    "SELECT v FROM {t} WHERE k >= {a} AND k < {} ORDER BY k",
                    a + params.range_rows
                )
            };
            for (t, out) in [(&scan, &mut scan_ms), (&idx, &mut idx_ms)] {
                let q = sql(t);
                let t0 = Instant::now();
                let r = c.exec(&q).map_err(|e| format!("{q}: {e}"))?;
                out.push(as_ms(t0.elapsed()));
                match &r {
                    Response::Rows { rows, .. }
                        if rows.len() == params.range_rows as usize
                            && rows[0].first() == Some(&format!("r{a}"))
                            && rows[rows.len() - 1].first()
                                == Some(&format!("r{}", a + params.range_rows - 1)) => {}
                    other => {
                        return Err(format!(
                            "{q}: expected {} rows r{a}..r{}, got {other:?}",
                            params.range_rows,
                            a + params.range_rows - 1
                        ))
                    }
                }
            }
            let q = format!("SELECT v FROM {floor_t} WHERE k = 0");
            let t0 = Instant::now();
            let r = c.exec(&q).map_err(|e| format!("{q}: {e}"))?;
            floor_ms.push(as_ms(t0.elapsed()));
            match &r {
                Response::Rows { rows, .. }
                    if rows.len() == 1 && rows[0].first() == Some(&"r0".to_string()) => {}
                other => return Err(format!("{q}: expected the one row, got {other:?}")),
            }
        }
        Ok(Measured {
            scan: p50(&mut scan_ms),
            indexed: p50(&mut idx_ms),
            floor: p50(&mut floor_ms),
        })
    };

    let mut notes: Vec<String> = Vec::new();
    let p = points(&mut c)?;
    check(&p, "point lookups", POINT_BUDGET_MS)?;
    notes.push(format!(
        "point p50 {:.1} ms scan vs {:.2} ms index ({})",
        p.scan,
        p.indexed,
        p.ratio_note()
    ));
    if !lite {
        let r = ranges(&mut c)?;
        check(&r, "range lookups", RANGE_BUDGET_MS)?;
        notes.push(format!(
            "range p50 {:.1} ms vs {:.2} ms ({})",
            r.scan,
            r.indexed,
            r.ratio_note()
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
            "after restart {:.1} ms vs {:.2} ms ({})",
            p2.scan,
            p2.indexed,
            p2.ratio_note()
        ));
    }
    Ok(notes.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A fake engine: answers the protocol without any storage. The scan
    /// table sleeps `scan_delay` per lookup, the indexed table
    /// `index_delay`, the one-row floor table `floor_delay` — the table
    /// name's last letter picks the behaviour (…s / …f / …o), which is how
    /// the gate names them.
    fn fake_engine(
        scan_delay: Duration,
        index_delay: Duration,
        floor_delay: Duration,
    ) -> std::net::SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            // Without nodelay the small response writes pay a delayed-ACK
            // stall (~40 ms) per statement, which swamps the sleeps.
            sock.set_nodelay(true).unwrap();
            let mut w = sock.try_clone().unwrap();
            let mut r = BufReader::new(sock);
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let sql = line.trim_end();
                let reply = if sql.starts_with("CREATE TABLE") || sql.starts_with("CREATE INDEX") {
                    "OK 0".to_string()
                } else if sql.starts_with("INSERT INTO") {
                    format!("OK {}", sql.matches("),(").count() + 1)
                } else if let Some(rest) = sql.strip_prefix("SELECT v FROM ") {
                    let table = rest.split(' ').next().unwrap_or("");
                    let k = rest.rsplit(" = ").next().unwrap_or("0");
                    let delay = if table.ends_with('s') {
                        scan_delay
                    } else if table.ends_with('o') {
                        floor_delay
                    } else {
                        index_delay
                    };
                    std::thread::sleep(delay);
                    format!("ROWS 1\nv\nr{k}\nEND 1")
                } else {
                    "ERR syntax fake".to_string()
                };
                if w.write_all(reply.as_bytes())
                    .and_then(|()| w.write_all(b"\n"))
                    .is_err()
                {
                    return;
                }
            }
        });
        addr
    }

    fn small_params() -> Params {
        Params {
            rows: 200,
            batch: 100,
            lookups: 50,
            ranges: 0,
            range_rows: 4,
        }
    }

    /// A scan too fast to show 10x (4 ms vs 0.5 ms is ~8x) passes when the
    /// index sits at the round-trip floor: the rule must not punish a
    /// compiled engine for scanning quickly.
    #[test]
    fn a_fast_scan_with_an_index_at_the_floor_passes() {
        let addr = fake_engine(
            Duration::from_micros(4_000),
            Duration::from_micros(500),
            Duration::from_micros(500),
        );
        let report = perf_gate_with(
            small_params(),
            &addr.to_string(),
            Duration::from_secs(5),
            7,
            true,
            None,
        )
        .expect("the floor rule keeps a fast scan passing");
        assert!(report.contains("floor"), "{report}");
    }

    /// Without a working index the "indexed" side is as slow as the scan:
    /// the gate must still fail (the floor rule is not a free pass).
    #[test]
    fn an_index_as_slow_as_the_scan_still_fails() {
        let addr = fake_engine(
            Duration::from_micros(4_000),
            Duration::from_micros(4_000),
            Duration::from_micros(500),
        );
        let err = perf_gate_with(
            small_params(),
            &addr.to_string(),
            Duration::from_secs(5),
            7,
            true,
            None,
        )
        .unwrap_err();
        assert!(err.contains("floor"), "{err}");
    }
}
