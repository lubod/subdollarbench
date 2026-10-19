//! Stage 16 (larger than memory): a 64 MiB candidate, ~400 MiB of rows
//! (docs/PROPOSAL-sdb-sql.md, "16. Larger than memory").
//!
//! The pipeline restarts the candidate on a **fresh scratch store** with a
//! 64 MiB cgroup cap, then runs this scenario:
//!
//! 1. load `big` (400k rows, ~900-byte text cells, ~400 MiB on disk) and a
//!    1k-row `dim`;
//! 2. a full-table aggregate (`COUNT/SUM/MIN/MAX`);
//! 3. an `ORDER BY t DESC, id DESC LIMIT 10` over the whole table, which
//!    must spill instead of buffering 400 MiB;
//! 4. an indexed join over `big.k`.
//!
//! Every answer is checked against values computed while the harness
//! generates the rows, so the scenario does not need a second 400 MiB
//! engine. A candidate that buffers the table or the sort dies under the
//! cap (the connection fails) and never reaches the checks.
//!
//! The small unit tests run the same scenario against a SQLite-backed mock;
//! the live proof runs the reference adapter under the real cap.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};

/// Rows in the full store (400k x ~900 B is about 400 MiB on disk).
pub const FULL_ROWS: u32 = 400_000;
/// Bytes in every `big.t` cell (the first 8 are the sort key).
pub const FULL_TEXT_LEN: usize = 900;
/// Rows in the joined dimension table.
pub const FULL_DIM_ROWS: u32 = 1_000;
/// How many top rows the order check asks for.
pub const TOP_N: usize = 10;
/// How many rows one load transaction carries.
const BATCH: u32 = 1_000;

/// The shape of one scenario run. The live proof uses [`Self::full`]; the
/// unit tests shrink every dimension so they stay fast.
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    pub rows: u32,
    pub text_len: usize,
    pub dim_rows: u32,
    pub timeout: Duration,
}

impl MemoryConfig {
    pub fn full() -> Self {
        Self {
            rows: FULL_ROWS,
            text_len: FULL_TEXT_LEN,
            dim_rows: FULL_DIM_ROWS,
            timeout: Duration::from_secs(300),
        }
    }
}

/// A deterministic 64-bit mix: spread the sort key over the whole id space
/// (so `ORDER BY t` cannot be answered from the id order).
fn mix64(id: u32) -> u64 {
    let mut x = (id as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The 8-hex-digit sort key plus filler, exactly `text_len` bytes.
fn row_text(id: u32, text_len: usize) -> String {
    let mut s = format!("{:016x}", mix64(id));
    s.truncate(8);
    s.push_str(&"x".repeat(text_len - 8));
    s
}

/// `big.v` for an id.
fn row_v(id: u32) -> i64 {
    ((id as i64) * 37 + 11) % 1_000_003
}

/// `dim.w` for a key.
fn dim_w(k: u32) -> i64 {
    ((k as i64) * 7) % 97 + 1
}

/// The answers the rows imply, accumulated while they are generated.
#[derive(Debug, Default)]
struct Expected {
    count: i64,
    sum_v: i64,
    min_k: i64,
    max_k: i64,
    per_k: Vec<i64>,
    top: Vec<u32>,
}

impl Expected {
    fn build(cfg: &MemoryConfig) -> Self {
        let mut e = Expected {
            min_k: i64::MAX,
            max_k: i64::MIN,
            per_k: vec![0; cfg.dim_rows as usize],
            ..Default::default()
        };
        let mut heap: BinaryHeap<Reverse<(String, u32)>> = BinaryHeap::new();
        for i in 0..cfg.rows {
            let id = i + 1;
            let k = id % cfg.dim_rows;
            let v = row_v(id);
            e.count += 1;
            e.sum_v += v;
            e.min_k = e.min_k.min(k as i64);
            e.max_k = e.max_k.max(k as i64);
            e.per_k[k as usize] += v;
            let key = (row_text(id, cfg.text_len), id);
            if heap.len() < TOP_N {
                heap.push(Reverse(key));
            } else if key > heap.peek().expect("non-empty").0 {
                heap.pop();
                heap.push(Reverse(key));
            }
        }
        if cfg.rows == 0 {
            e.min_k = 0;
            e.max_k = 0;
        }
        let mut top: Vec<(String, u32)> = heap.into_iter().map(|Reverse(key)| key).collect();
        top.sort_unstable_by(|a, b| b.cmp(a));
        e.top = top.into_iter().map(|(_, id)| id).collect();
        e
    }

    fn join_sum(&self) -> i64 {
        self.per_k
            .iter()
            .enumerate()
            .map(|(k, sum)| sum * dim_w(k as u32))
            .sum()
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

fn int_at(row: &[String], i: usize, what: &str) -> Result<i64, String> {
    let cell = row
        .get(i)
        .ok_or_else(|| format!("{what}: row {row:?} has no column {i}"))?;
    cell.parse::<i64>()
        .map_err(|e| format!("{what}: {cell:?} is not an integer: {e}"))
}

/// Load the store and check the aggregate, the external sort and the
/// indexed join. The caller owns the cgroup cap and the scratch store.
pub fn scenario(addr: &str, cfg: &MemoryConfig) -> Result<String, String> {
    if cfg.text_len < 8 {
        return Err("text_len must be at least 8 (the sort key)".to_string());
    }
    if cfg.dim_rows == 0 {
        return Err("dim_rows must be at least 1".to_string());
    }
    let expected = Expected::build(cfg);
    let started = Instant::now();
    let mut c = TcpCandidate::connect(addr, cfg.timeout)?;

    expect_ok(
        c.exec("CREATE TABLE big (id INTEGER PRIMARY KEY, k INTEGER NOT NULL, v INTEGER NOT NULL, t TEXT NOT NULL)")?,
        "CREATE TABLE big",
    )?;
    expect_ok(
        c.exec("CREATE TABLE dim (k INTEGER PRIMARY KEY, w INTEGER NOT NULL)")?,
        "CREATE TABLE dim",
    )?;

    // The dimension, then the big table. One transaction per batch keeps
    // the durability fsyncs low without asking the candidate to buffer the
    // whole store.
    c.send("BEGIN")?;
    for k in 0..cfg.dim_rows {
        c.send(&format!("INSERT INTO dim VALUES ({k}, {})", dim_w(k)))?;
    }
    c.send("COMMIT")?;
    expect_ok(c.recv()?, "BEGIN dim")?;
    for _ in 0..cfg.dim_rows {
        expect_ok(c.recv()?, "INSERT dim")?;
    }
    expect_ok(c.recv()?, "COMMIT dim")?;

    let mut base = 0u32;
    while base < cfg.rows {
        let n = BATCH.min(cfg.rows - base);
        c.send("BEGIN")?;
        for i in base..base + n {
            let id = i + 1;
            c.send(&format!(
                "INSERT INTO big VALUES ({id}, {}, {}, '{}')",
                id % cfg.dim_rows,
                row_v(id),
                row_text(id, cfg.text_len)
            ))?;
        }
        c.send("COMMIT")?;
        expect_ok(c.recv()?, "BEGIN big")?;
        for i in base..base + n {
            let id = i + 1;
            if let Err(e) = expect_ok(c.recv()?, &format!("INSERT big id {id}")) {
                return Err(format!("load failed: {e}"));
            }
        }
        expect_ok(c.recv()?, "COMMIT big")?;
        base += n;
    }
    let loaded = started.elapsed();

    // The index is built after the load so the join has to use it (or pay
    // a 400k x 1k nested loop the 300 s read timeout would not forgive).
    expect_ok(
        c.exec("CREATE INDEX big_k ON big (k)")?,
        "CREATE INDEX big_k",
    )?;

    let agg = expect_rows(
        c.exec("SELECT COUNT(*), SUM(v), MIN(k), MAX(k) FROM big")?,
        "aggregate",
    )?;
    let row = agg
        .first()
        .ok_or_else(|| "aggregate returned no rows".to_string())?;
    let (count, sum_v, min_k, max_k) = (
        int_at(row, 0, "aggregate COUNT")?,
        int_at(row, 1, "aggregate SUM")?,
        int_at(row, 2, "aggregate MIN")?,
        int_at(row, 3, "aggregate MAX")?,
    );
    if (count, sum_v, min_k, max_k)
        != (
            expected.count,
            expected.sum_v,
            expected.min_k,
            expected.max_k,
        )
    {
        return Err(format!(
            "aggregate: got COUNT={count} SUM={sum_v} MIN={min_k} MAX={max_k}, \
             expected COUNT={} SUM={} MIN={} MAX={}",
            expected.count, expected.sum_v, expected.min_k, expected.max_k
        ));
    }

    let ordered = expect_rows(
        c.exec(&format!(
            "SELECT id FROM big ORDER BY t DESC, id DESC LIMIT {TOP_N}"
        ))?,
        "ORDER BY",
    )?;
    let got: Vec<i64> = ordered
        .iter()
        .map(|row| int_at(row, 0, "ORDER BY id"))
        .collect::<Result<_, _>>()?;
    let want: Vec<i64> = expected.top.iter().map(|id| *id as i64).collect();
    if got != want {
        return Err(format!(
            "ORDER BY t DESC, id DESC LIMIT {TOP_N}: got {got:?}, expected {want:?}"
        ));
    }

    let joined = expect_rows(
        c.exec("SELECT COUNT(*), SUM(b.v * d.w) FROM big b JOIN dim d ON b.k = d.k")?,
        "join",
    )?;
    let row = joined
        .first()
        .ok_or_else(|| "join returned no rows".to_string())?;
    let (jcount, jsum) = (int_at(row, 0, "join COUNT")?, int_at(row, 1, "join SUM")?);
    if (jcount, jsum) != (expected.count, expected.join_sum()) {
        return Err(format!(
            "indexed join: got COUNT={jcount} SUM={jsum}, expected COUNT={} SUM={}",
            expected.count,
            expected.join_sum()
        ));
    }

    let mib = (cfg.rows as u64 * cfg.text_len as u64) / (1024 * 1024);
    Ok(format!(
        "{} rows (~{mib} MiB of text) loaded in {:.1}s; aggregate, ORDER BY and indexed join exact",
        cfg.rows,
        loaded.as_secs_f64()
    ))
}

#[cfg(test)]
mod tests {
    //! A SQLite-backed mock of the scenario's narrow SQL surface. The
    //! failure modes corrupt one answer at a time; the scenario must catch
    //! each.

    use super::*;
    use rusqlite::types::ValueRef;
    use rusqlite::Connection;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Good,
        BadAggregate,
        BadOrder,
        BadJoin,
    }

    fn render(conn: &Mutex<Connection>, mode: Mode, sql: &str) -> String {
        let low = sql.to_ascii_lowercase();
        if !low.starts_with("select") {
            let conn = conn.lock().unwrap();
            return match conn.execute(sql, []) {
                Ok(n) => format!("OK {n}\n"),
                Err(e) => format!("ERR syntax {e}\n"),
            };
        }
        let conn = conn.lock().unwrap();
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => return format!("ERR syntax {e}\n"),
        };
        let headers = stmt
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut q = stmt.query([]).expect("query");
        while let Some(row) = q.next().expect("step") {
            let mut cells = Vec::new();
            for i in 0..row.as_ref().column_count() {
                cells.push(match row.get_ref(i).expect("cell") {
                    ValueRef::Null => "\\N".to_string(),
                    ValueRef::Integer(n) => n.to_string(),
                    ValueRef::Real(f) => format!("{f}"),
                    ValueRef::Text(t) => {
                        crate::verifier::sql::wire::encode_text(&String::from_utf8_lossy(t))
                    }
                    ValueRef::Blob(_) => "\\N".to_string(),
                });
            }
            rows.push(cells);
        }
        if mode == Mode::BadAggregate && low.contains("min(k)") {
            if let Some(cell) = rows.first_mut().and_then(|r| r.first_mut()) {
                *cell = "999999999".to_string();
            }
        }
        if mode == Mode::BadOrder && low.contains("order by") {
            rows.reverse();
        }
        if mode == Mode::BadJoin && low.contains("join") {
            if let Some(cell) = rows.first_mut().and_then(|r| r.get_mut(1)) {
                *cell = "0".to_string();
            }
        }
        let mut out = format!("ROWS {}\n", headers.len());
        out.push_str(&headers.join("\t"));
        out.push('\n');
        for r in &rows {
            out.push_str(&r.join("\t"));
            out.push('\n');
        }
        out.push_str(&format!("END {}\n", rows.len()));
        out
    }

    fn spawn(mode: Mode) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let sql = line.trim_end();
                let response = render(&conn, mode, sql);
                if writer.write_all(response.as_bytes()).is_err() {
                    break;
                }
                line.clear();
            }
        });
        (addr, handle)
    }

    fn cfg() -> MemoryConfig {
        MemoryConfig {
            rows: 500,
            text_len: 24,
            dim_rows: 10,
            timeout: Duration::from_secs(20),
        }
    }

    #[test]
    fn a_faithful_server_passes_the_whole_scenario() {
        let (addr, h) = spawn(Mode::Good);
        let report = scenario(&addr, &cfg()).expect("the faithful mock must pass");
        assert!(report.contains("500 rows"), "{report}");
        h.join().unwrap();
    }

    #[test]
    fn a_wrong_aggregate_fails() {
        let (addr, h) = spawn(Mode::BadAggregate);
        let err = scenario(&addr, &cfg()).unwrap_err();
        assert!(err.contains("aggregate"), "{err}");
        h.join().unwrap();
    }

    #[test]
    fn a_wrong_sort_order_fails() {
        let (addr, h) = spawn(Mode::BadOrder);
        let err = scenario(&addr, &cfg()).unwrap_err();
        assert!(err.contains("ORDER BY"), "{err}");
        h.join().unwrap();
    }

    #[test]
    fn a_wrong_join_fails() {
        let (addr, h) = spawn(Mode::BadJoin);
        let err = scenario(&addr, &cfg()).unwrap_err();
        assert!(err.contains("indexed join"), "{err}");
        h.join().unwrap();
    }

    #[test]
    fn the_full_config_matches_the_reference_cross_check() {
        // The reference (SQLite, full config) answered these exact values in
        // the ticket-16 prototype run; the harness must agree without SQLite.
        let e = Expected::build(&MemoryConfig::full());
        assert_eq!(e.count, 400_000);
        assert_eq!(e.sum_v, 197_838_513_505);
        assert_eq!(e.join_sum(), 9_665_364_440_393);
        assert_eq!(
            e.top,
            vec![91199, 72812, 136975, 75654, 372335, 130321, 121123, 158640, 147649, 188940]
        );
    }

    #[test]
    fn the_expected_answers_match_a_brute_force_sort() {
        let cfg = cfg();
        let e = Expected::build(&cfg);
        let mut all: Vec<(String, u32)> = (1..=cfg.rows)
            .map(|id| (row_text(id, cfg.text_len), id))
            .collect();
        all.sort_unstable_by(|a, b| b.cmp(a));
        let want: Vec<u32> = all.iter().take(TOP_N).map(|(_, id)| *id).collect();
        assert_eq!(e.top, want);
    }
}
