//! Stage 14 (observability): `EXPLAIN` plans and `SHOW STATS` counters
//! (docs/PROPOSAL-sdb-sql.md, "14. Observability").
//!
//! Both are wire statements the differential generator does not produce, so
//! this is a scripted scenario:
//!
//! - **`EXPLAIN <select>`** answers a `ROWS` plan (one line per step). An
//!   equality predicate on an indexed column must name
//!   `INDEX <table>.<index>`; without a usable index the plan names
//!   `SCAN <table>`.
//! - **`SHOW STATS`** answers `name`/`value` rows. After scripted traffic
//!   the harness checks the exact deltas of `statements`, `selects`,
//!   `inserts`, `updates`, `deletes`, `commits`, `rollbacks`, `errors`,
//!   `connections_current` and `connections_total`.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::collections::BTreeMap;
use std::time::Duration;

/// The counters the contract names. `SHOW STATS` must answer all of them.
pub const STAT_NAMES: [&str; 10] = [
    "statements",
    "selects",
    "inserts",
    "updates",
    "deletes",
    "commits",
    "rollbacks",
    "errors",
    "connections_current",
    "connections_total",
];

fn ok_any(resp: Result<Response, String>, what: &str) -> Result<(), String> {
    match resp {
        Ok(Response::Ok(_)) => Ok(()),
        Ok(other) => Err(format!("{what} answered {other:?}, expected OK")),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

fn rows(resp: Response, what: &str) -> Result<Vec<Vec<String>>, String> {
    match resp {
        Response::Rows { rows, .. } => Ok(rows),
        other => Err(format!("{what} answered {other:?}, expected ROWS")),
    }
}

/// The plan lines of an `EXPLAIN` answer.
fn plan_lines(resp: Result<Response, String>) -> Result<Vec<String>, String> {
    Ok(rows(resp?, "EXPLAIN")?
        .into_iter()
        .map(|r| r.join(" "))
        .collect())
}

/// `SHOW STATS` as a name -> value map (names lowercased).
fn stats(c: &mut TcpCandidate) -> Result<BTreeMap<String, i64>, String> {
    let rows = rows(c.exec("SHOW STATS")?, "SHOW STATS")?;
    let mut out = BTreeMap::new();
    for row in rows {
        let Some(name) = row.first() else { continue };
        let Some(value) = row.get(1) else {
            return Err(format!("SHOW STATS row {row:?} has no value column"));
        };
        let n = value
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("SHOW STATS {name:?} value {value:?} is not an integer: {e}"))?;
        out.insert(name.trim().to_ascii_lowercase(), n);
    }
    Ok(out)
}

fn require_all(map: &BTreeMap<String, i64>, what: &str) -> Result<(), String> {
    let missing: Vec<&str> = STAT_NAMES
        .iter()
        .copied()
        .filter(|n| !map.contains_key(*n))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{what} misses counters {missing:?}: {map:?}"))
    }
}

/// `EXPLAIN` must name the index for an indexed equality predicate and a
/// scan otherwise.
pub fn explain_plans(
    addr: &str,
    timeout: Duration,
    table: &str,
    index: &str,
) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    ok_any(
        c.exec(&format!(
            "CREATE TABLE {table} (id INTEGER, k INTEGER, v TEXT)"
        )),
        "CREATE TABLE",
    )?;
    // Enough rows that a planner has a reason to use the index.
    for i in 0..20 {
        ok_any(
            c.exec(&format!("INSERT INTO {table} VALUES ({i}, {i}, 'v{i}')")),
            "INSERT",
        )?;
    }
    ok_any(
        c.exec(&format!("CREATE INDEX {index} ON {table} (k)")),
        "CREATE INDEX",
    )?;

    let indexed = plan_lines(c.exec(&format!("EXPLAIN SELECT v FROM {table} WHERE k = 5")))?;
    let low: Vec<String> = indexed.iter().map(|l| l.to_ascii_lowercase()).collect();
    let names_index = low
        .iter()
        .any(|l| l.contains("index") && l.contains(&index.to_ascii_lowercase()));
    if !names_index {
        return Err(format!(
            "the plan for an indexed equality does not name {index:?}: {indexed:?}"
        ));
    }
    if low
        .iter()
        .any(|l| l.contains("scan") && l.contains(&table.to_ascii_lowercase()))
    {
        return Err(format!(
            "the plan for an indexed equality still scans {table}: {indexed:?}"
        ));
    }

    let scanned = plan_lines(c.exec(&format!("EXPLAIN SELECT v FROM {table} WHERE id = 5")))?;
    let low: Vec<String> = scanned.iter().map(|l| l.to_ascii_lowercase()).collect();
    let names_scan = low
        .iter()
        .any(|l| l.contains("scan") && l.contains(&table.to_ascii_lowercase()));
    if !names_scan {
        return Err(format!(
            "the plan for an unindexed equality does not scan {table}: {scanned:?}"
        ));
    }
    Ok(())
}

/// `SHOW STATS` deltas after a known script, plus the connection gauges.
pub fn stats_deltas(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    ok_any(
        c.exec(&format!("CREATE TABLE {table} (id INTEGER, v TEXT)")),
        "CREATE TABLE",
    )?;
    let base = stats(&mut c)?;
    require_all(&base, "SHOW STATS (baseline)")?;

    // The scripted traffic. `statements` counts every line, including the
    // final SHOW STATS; SELECTs are counted by their first keyword even when
    // the statement fails (that failure is also an `error`).
    for sql in [
        "SELECT 1",
        "SELECT 2",
        "SELECT 3",
        "INSERT INTO t_stats VALUES (1, 'a')",
        "INSERT INTO t_stats VALUES (2, 'b')",
        "UPDATE t_stats SET v = 'c' WHERE id = 1",
        "DELETE FROM t_stats WHERE id = 2",
        "BEGIN",
        "COMMIT",
        "BEGIN",
        "ROLLBACK",
    ] {
        let sql = sql.replace("t_stats", table);
        match c.exec(&sql) {
            Ok(Response::Err { class, text }) => {
                return Err(format!(
                    "scripted statement {sql:?} failed: ERR {class} {text}"
                ))
            }
            Ok(_) => {}
            Err(e) => return Err(format!("scripted statement {sql:?}: {e}")),
        }
    }
    // One deliberate error: a missing table.
    match c.exec("SELECT * FROM zz_missing_table") {
        Ok(Response::Err { class, .. }) if class == "no_table" => {}
        other => {
            return Err(format!(
                "the error probe answered {other:?}, expected ERR no_table"
            ))
        }
    }

    let after = stats(&mut c)?;
    require_all(&after, "SHOW STATS")?;
    let want: [(&str, i64); 8] = [
        // 12 traffic statements (the two BEGINs included) + the final SHOW
        // STATS itself.
        ("statements", 13),
        ("selects", 4), // three plain SELECTs + the failing one
        ("inserts", 2),
        ("updates", 1),
        ("deletes", 1),
        ("commits", 1),
        ("rollbacks", 1),
        ("errors", 1),
    ];
    for (name, delta) in want {
        let got = after[name] - base[name];
        if got != delta {
            return Err(format!(
                "SHOW STATS delta for {name}: {got}, expected {delta} (baseline {}, after {})",
                base[name], after[name]
            ));
        }
    }
    // The gauges: the same connection is open for both reads.
    for name in ["connections_current", "connections_total"] {
        let got = after[name] - base[name];
        if got != 0 {
            return Err(format!(
                "SHOW STATS {name} moved by {got} across two reads on one connection"
            ));
        }
    }

    // A second connection is counted, and closing it releases the gauge.
    let b = TcpCandidate::connect(addr, timeout)?;
    let mut b = b;
    let during = stats(&mut b)?;
    require_all(&during, "SHOW STATS (second connection)")?;
    if during["connections_current"] != base["connections_current"] + 1
        || during["connections_total"] != base["connections_total"] + 1
    {
        return Err(format!(
            "a second connection is not counted: current {} -> {}, total {} -> {}",
            base["connections_current"],
            during["connections_current"],
            base["connections_total"],
            during["connections_total"]
        ));
    }
    drop(b);
    // The server notices a client's close when its read loop sees EOF, which
    // is not synchronous with the client's close: give it a moment.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut closed = stats(&mut c)?;
    while closed["connections_current"] != base["connections_current"]
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(25));
        closed = stats(&mut c)?;
    }
    if closed["connections_current"] != base["connections_current"] {
        return Err(format!(
            "connections_current did not return after a close: {} -> {}",
            base["connections_current"], closed["connections_current"]
        ));
    }
    if closed["connections_total"] != base["connections_total"] + 1 {
        return Err(format!(
            "connections_total lost the closed connection: {} -> {}",
            base["connections_total"], closed["connections_total"]
        ));
    }
    Ok(())
}

/// The stage-14 contract.
pub fn scenario(addr: &str, timeout: Duration, seed: u64) -> Result<(), String> {
    let ns = format!("o{seed:x}");
    explain_plans(addr, timeout, &format!("{ns}_t"), &format!("{ns}_k"))?;
    stats_deltas(addr, timeout, &format!("{ns}_s"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! A focused mock of the two statements plus the SQL the scenario runs.
    //! Modes cover the pass case and each failure class (a plan that ignores
    //! the index; counters that miss the error/statement accounting).

    use super::super::compare::render_response;
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Good,
        IgnoresIndex,
        BadStats,
    }

    #[derive(Default, Clone)]
    struct Counters {
        statements: i64,
        selects: i64,
        inserts: i64,
        updates: i64,
        deletes: i64,
        commits: i64,
        rollbacks: i64,
        errors: i64,
    }

    struct Mock {
        mode: Mode,
        counters: Mutex<Counters>,
        current: Mutex<i64>,
        total: Mutex<i64>,
        indexes: Mutex<Vec<(String, String, String)>>, // (name, table, column)
    }

    impl Mock {
        fn bump(&self, f: impl FnOnce(&mut Counters)) {
            if self.mode == Mode::BadStats {
                // A counter bug the scenario must catch.
                return;
            }
            f(&mut self.counters.lock().unwrap());
        }
        fn count_kind(&self, sql: &str) {
            let first = sql
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            self.bump(|c| {
                c.statements += 1;
                match first.as_str() {
                    "SELECT" => c.selects += 1,
                    "INSERT" => c.inserts += 1,
                    "UPDATE" => c.updates += 1,
                    "DELETE" => c.deletes += 1,
                    "COMMIT" => c.commits += 1,
                    "ROLLBACK" => c.rollbacks += 1,
                    _ => {}
                }
            });
        }
        fn stats_response(&self) -> Response {
            let c = self.counters.lock().unwrap().clone();
            let mut rows = vec![
                vec!["statements".to_string(), c.statements.to_string()],
                vec!["selects".to_string(), c.selects.to_string()],
                vec!["inserts".to_string(), c.inserts.to_string()],
                vec!["updates".to_string(), c.updates.to_string()],
                vec!["deletes".to_string(), c.deletes.to_string()],
                vec!["commits".to_string(), c.commits.to_string()],
                vec!["rollbacks".to_string(), c.rollbacks.to_string()],
                vec!["errors".to_string(), c.errors.to_string()],
                vec![
                    "connections_current".to_string(),
                    self.current.lock().unwrap().to_string(),
                ],
                vec![
                    "connections_total".to_string(),
                    self.total.lock().unwrap().to_string(),
                ],
            ];
            rows.sort();
            Response::Rows {
                ncols: 2,
                headers: vec!["name".to_string(), "value".to_string()],
                end_count: rows.len() as u64,
                rows,
            }
        }
        fn explain(&self, sql: &str) -> Response {
            // The scenario's selects name the from-table and a WHERE column.
            let low = sql.to_ascii_lowercase();
            let after_from = low.split(" from ").nth(1).unwrap_or("");
            let table = after_from
                .split_whitespace()
                .next()
                .unwrap_or("t")
                .to_string();
            let has_index = self
                .indexes
                .lock()
                .unwrap()
                .iter()
                .any(|(_, t, c)| t == &table && low.contains(&format!("where {c} ")));
            let line = if has_index && self.mode != Mode::IgnoresIndex {
                let name = self
                    .indexes
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(_, t, c)| t == &table && low.contains(&format!("where {c} ")))
                    .map(|(n, _, _)| n.clone())
                    .unwrap_or_default();
                format!("INDEX {table}.{name}")
            } else {
                format!("SCAN {table}")
            };
            Response::Rows {
                ncols: 1,
                headers: vec!["plan".to_string()],
                end_count: 1,
                rows: vec![vec![line]],
            }
        }
        fn exec(&self, line: &str) -> Response {
            let t = line.trim();
            let low = t.to_ascii_lowercase();
            if low == "show stats" {
                self.bump(|c| c.statements += 1);
                return self.stats_response();
            }
            self.count_kind(t);
            if let Some(rest) = low.strip_prefix("explain ") {
                let _ = rest;
                return self.explain(t);
            }
            if low.starts_with("select * from ") {
                let table = t.split_whitespace().last().unwrap_or("");
                // The only table the error probe names is missing.
                if table.contains("zz_missing") {
                    if self.mode != Mode::BadStats {
                        self.counters.lock().unwrap().errors += 1;
                    }
                    return Response::Err {
                        class: "no_table".to_string(),
                        text: format!("no such table: {table}"),
                    };
                }
                return Response::Rows {
                    ncols: 1,
                    headers: vec!["v".to_string()],
                    end_count: 0,
                    rows: Vec::new(),
                };
            }
            if low.starts_with("select ") {
                // `SELECT 1` / `SELECT 2` / `SELECT 3`.
                let v = t.split_whitespace().nth(1).unwrap_or("0").to_string();
                return Response::Rows {
                    ncols: 1,
                    headers: vec!["1".to_string()],
                    end_count: 1,
                    rows: vec![vec![v]],
                };
            }
            if low.starts_with("create index ") {
                // CREATE INDEX name ON table (col)
                let parts: Vec<&str> = t.split_whitespace().collect();
                let name = parts.get(2).unwrap_or(&"").to_string();
                let table = parts.get(4).unwrap_or(&"").to_string();
                let col = parts
                    .last()
                    .unwrap_or(&"")
                    .trim_matches(|c: char| c == '(' || c == ')' || c == ';')
                    .to_string();
                self.indexes.lock().unwrap().push((name, table, col));
                return Response::Ok(0);
            }
            if low.starts_with("create table ") {
                return Response::Ok(0);
            }
            if low.starts_with("insert into ") {
                return Response::Ok(1);
            }
            if low.starts_with("update ") {
                return Response::Ok(0);
            }
            if low.starts_with("delete from ") {
                return Response::Ok(0);
            }
            if low.starts_with("begin") || low.starts_with("commit") || low.starts_with("rollback")
            {
                return Response::Ok(0);
            }
            Response::Err {
                class: "syntax".to_string(),
                text: "mock: unsupported statement".to_string(),
            }
        }
    }

    fn serve(mode: Mode) -> String {
        let mock = std::sync::Arc::new(Mock {
            mode,
            counters: Mutex::new(Counters::default()),
            current: Mutex::new(0),
            total: Mutex::new(0),
            indexes: Mutex::new(Vec::new()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                let mock = mock.clone();
                *mock.current.lock().unwrap() += 1;
                *mock.total.lock().unwrap() += 1;
                std::thread::spawn(move || {
                    let mut sock = sock;
                    sock.set_nodelay(true).unwrap();
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap() > 0 {
                        let resp = mock.exec(&line);
                        let text = render_response(&Ok(resp));
                        if sock.write_all(text.as_bytes()).is_err()
                            || sock.write_all(b"\n").is_err()
                        {
                            break;
                        }
                        line.clear();
                    }
                    *mock.current.lock().unwrap() -= 1;
                });
            }
        });
        addr.to_string()
    }

    fn timeout() -> Duration {
        Duration::from_secs(10)
    }

    #[test]
    fn the_scenario_passes_on_a_faithful_server() {
        let addr = serve(Mode::Good);
        scenario(&addr, timeout(), 3).unwrap();
    }

    #[test]
    fn a_plan_that_ignores_the_index_fails() {
        let addr = serve(Mode::IgnoresIndex);
        let err = explain_plans(&addr, timeout(), "pt", "pk").unwrap_err();
        assert!(err.contains("does not name"), "{err}");
    }

    #[test]
    fn counters_that_miss_the_scripted_deltas_fail() {
        let addr = serve(Mode::BadStats);
        let err = stats_deltas(&addr, timeout(), "st").unwrap_err();
        assert!(
            err.contains("delta") || err.contains("moved"),
            "a silent counter must fail: {err}"
        );
    }

    #[test]
    fn the_scenario_repeats_cleanly() {
        let addr = serve(Mode::Good);
        for i in 0..5 {
            scenario(&addr, timeout(), 10 + i).unwrap_or_else(|e| panic!("repeat {i}: {e}"));
        }
    }
}
