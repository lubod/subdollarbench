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
    let table_low = table.to_ascii_lowercase();
    if low.iter().any(|l| {
        l.contains("scan")
            && l.contains(&table_low)
            // "INDEX SCAN t.ix" names the index; it is an index plan, not
            // an additional scan (round-15 L3).
            && !l.contains("index")
    }) {
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
    // A prior connection's close may land late; settle the gauge with two
    // equal reads so a delayed EOF cannot move it across the baseline
    // (round-15 L2).
    let mut base = stats(&mut c)?;
    require_all(&base, "SHOW STATS (baseline)")?;
    let settle_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        std::thread::sleep(Duration::from_millis(25));
        let again = stats(&mut c)?;
        require_all(&again, "SHOW STATS (baseline settle)")?;
        let stable = again["connections_current"] == base["connections_current"];
        base = again;
        if stable || std::time::Instant::now() >= settle_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

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
mod tests;
