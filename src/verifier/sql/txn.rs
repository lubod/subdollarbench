//! Stage 6 (transactions) protocol helpers and invariants
//! (docs/PROPOSAL-sdb-sql.md, "6. Transactions"): cross-connection
//! isolation and a kill with a transaction open. Nested `BEGIN`, bare
//! `COMMIT`/`ROLLBACK` and errors inside a transaction are covered by the
//! generator's probes; these are the parts the differential script cannot
//! express.
//!
//! Lifecycle (kill, restart) arrives as callbacks, as in `crash`.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::time::Duration;

fn ok0(got: Response, what: &str) -> Result<(), String> {
    match got {
        Response::Ok(0) => Ok(()),
        other => Err(format!("{what} answered {other:?}, expected OK 0")),
    }
}

fn ok_n(got: Response, n: u64, what: &str) -> Result<(), String> {
    match got {
        Response::Ok(got) if got == n => Ok(()),
        other => Err(format!("{what} answered {other:?}, expected OK {n}")),
    }
}

/// The keys visible on a fresh connection, in order.
fn visible_keys(addr: &str, timeout: Duration, table: &str) -> Result<Vec<String>, String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    match c.exec(&format!("SELECT k FROM {table} ORDER BY k"))? {
        Response::Rows { rows, .. } => Ok(rows.into_iter().flatten().collect()),
        other => Err(format!("SELECT k answered {other:?}")),
    }
}

/// Two connections: uncommitted rows are invisible to the other one, a
/// `COMMIT` makes them visible, a `ROLLBACK` discards them.
pub fn isolation(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    let mut a = TcpCandidate::connect(addr, timeout)?;
    ok0(
        a.exec(&format!("CREATE TABLE {table} (k INTEGER, v TEXT)"))?,
        "CREATE TABLE",
    )?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (0, 'base')"))?,
        1,
        "baseline INSERT",
    )?;

    // BEGIN on A; the row must stay invisible to B until COMMIT.
    ok0(a.exec("BEGIN")?, "BEGIN")?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (1, 'uncommitted')"))?,
        1,
        "uncommitted INSERT",
    )?;
    let seen = visible_keys(addr, timeout, table)?;
    if seen != vec!["0".to_string()] {
        return Err(format!(
            "an uncommitted row is visible to another connection: {seen:?}"
        ));
    }
    ok0(a.exec("COMMIT")?, "COMMIT")?;
    let seen = visible_keys(addr, timeout, table)?;
    if seen != vec!["0".to_string(), "1".to_string()] {
        return Err(format!("a committed row is missing: {seen:?}"));
    }

    // BEGIN on A; ROLLBACK must discard the row without any trace.
    ok0(a.exec("BEGIN")?, "second BEGIN")?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (2, 'rolled back')"))?,
        1,
        "second INSERT",
    )?;
    ok0(a.exec("ROLLBACK")?, "ROLLBACK")?;
    let seen = visible_keys(addr, timeout, table)?;
    if seen != vec!["0".to_string(), "1".to_string()] {
        return Err(format!("a rolled-back row survived: {seen:?}"));
    }
    Ok(())
}

/// A kill with a transaction open: the committed baseline survives, the
/// uncommitted row is gone (never half-applied).
pub fn kill_with_open_transaction(
    addr: &str,
    timeout: Duration,
    table: &str,
    kill: &mut dyn FnMut() -> Result<(), String>,
    restart: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut a = TcpCandidate::connect(addr, timeout)?;
    ok0(
        a.exec(&format!("CREATE TABLE {table} (k INTEGER, v TEXT)"))?,
        "CREATE TABLE",
    )?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (0, 'committed')"))?,
        1,
        "committed INSERT",
    )?;
    ok0(a.exec("BEGIN")?, "BEGIN")?;
    ok_n(
        a.exec(&format!("INSERT INTO {table} VALUES (1, 'doomed')"))?,
        1,
        "doomed INSERT",
    )?;
    // The connection stays open across the kill: the transaction must be
    // discarded by *recovery*, not by a clean client disconnect.
    kill()?;
    restart()?;
    let seen = visible_keys(addr, timeout, table)?;
    if seen != vec!["0".to_string()] {
        return Err(format!(
            "after a kill with an open transaction: {seen:?} (expected the committed row only)"
        ));
    }
    Ok(())
}
