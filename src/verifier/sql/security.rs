//! Stage 18 (hostile input and limits): AUTH, fuzzing, recursion, statement
//! size, cancellation, slow-loris and the connection cap
//! (docs/PROPOSAL-sdb-sql.md, "18. Hostile input and limits").
//!
//! The pipeline restarts the candidate on a fresh scratch store with
//! `SDB_SQL_PASSWORD` set. This scenario drives the raw text protocol (the
//! differential client cannot authenticate) and checks, in order:
//!
//! 1. no `AUTH` / a wrong password -> `ERR auth`; the password never appears
//!    in an error; a correct `AUTH` unlocks the connection;
//! 2. deterministic byte fuzzing and grammar mutations never kill the
//!    server (a valid query still answers);
//! 3. a 10,000-deep expression -> `ERR limit`, no stack overflow;
//! 4. a statement over 16 MiB -> `ERR limit`, and the connection recovers;
//! 5. a query that runs for the 5 s statement budget -> `ERR limit`;
//! 6. a slow-loris client with a partial line does not starve another
//!    connection;
//! 7. at most 256 connections are open; the next is refused cleanly, an
//!    existing one still answers, and the cap releases after closes.

use super::wire::{read_response_ctx, Response};
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub const FULL_MAX_STMT: usize = 16 * 1024 * 1024;
pub const FULL_MAX_DEPTH: usize = 10_000;
pub const FULL_MAX_CONNS: usize = 256;
pub const FULL_STATEMENT_SECS: f64 = 5.0;

/// The shape of one scenario run. The full config is the ticket's contract;
/// the unit tests shrink every limit and use a fast mock.
#[derive(Debug, Clone)]
pub struct SecurityConfig {
    pub password: String,
    pub timeout: Duration,
    pub max_stmt: usize,
    pub max_depth: usize,
    pub max_conns: usize,
    pub statement_secs: f64,
}

impl SecurityConfig {
    pub fn full(password: impl Into<String>) -> Self {
        Self {
            password: password.into(),
            timeout: Duration::from_secs(30),
            max_stmt: FULL_MAX_STMT,
            max_depth: FULL_MAX_DEPTH,
            max_conns: FULL_MAX_CONNS,
            statement_secs: FULL_STATEMENT_SECS,
        }
    }
}

/// A raw text-protocol connection (AUTH needs lines the differential client
/// would otherwise treat as statements).
struct Raw {
    reader: BufReader<TcpStream>,
    timeout: Duration,
    step: &'static str,
}

impl Raw {
    fn connect(addr: &str, timeout: Duration, step: &'static str) -> Result<Self, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| format!("write timeout: {e}"))?;
        stream
            .set_nodelay(true)
            .map_err(|e| format!("nodelay: {e}"))?;
        Ok(Self {
            reader: BufReader::new(stream),
            timeout,
            step,
        })
    }

    fn set_step(&mut self, step: &'static str) {
        self.step = step;
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.reader
            .get_mut()
            .write_all(bytes)
            .map_err(|e| format!("write: {e}"))
    }

    fn response(&mut self) -> Result<Response, String> {
        read_response_ctx(&mut self.reader, Some((self.timeout, self.step)))
    }

    fn exec(&mut self, sql: &str) -> Result<Response, String> {
        self.send(sql.as_bytes())?;
        self.send(b"\n")?;
        self.response()
    }
}

fn auth(addr: &str, cfg: &SecurityConfig) -> Result<Raw, String> {
    let mut c = Raw::connect(addr, cfg.timeout, "AUTH")?;
    c.send(format!("AUTH {}\n", cfg.password).as_bytes())?;
    match c.response()? {
        Response::Ok(_) => Ok(c),
        other => Err(format!("AUTH answered {other:?}, expected OK")),
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

fn expect_limit(resp: Response, what: &str) -> Result<(), String> {
    match resp {
        Response::Err { class, .. } if class == "limit" => Ok(()),
        other => Err(format!("{what}: expected ERR limit, got {other:?}")),
    }
}

fn no_password(text: &str, cfg: &SecurityConfig, what: &str) -> Result<(), String> {
    if text.contains(&cfg.password) {
        Err(format!("{what} leaked the password in its error text"))
    } else {
        Ok(())
    }
}

fn expect_one(c: &mut Raw, what: &str) -> Result<(), String> {
    let rows = expect_rows(c.exec("SELECT 1")?, what)?;
    if rows == vec![vec!["1".to_string()]] {
        Ok(())
    } else {
        Err(format!("{what}: SELECT 1 answered {rows:?}"))
    }
}

fn auth_checks(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    // No AUTH.
    let mut c = Raw::connect(addr, cfg.timeout, "AUTH")?;
    match c.exec("SELECT 1")? {
        Response::Err { class, text } => {
            if class != "auth" {
                return Err(format!(
                    "without AUTH SELECT 1 answered ERR {class}, expected auth"
                ));
            }
            no_password(&text, cfg, "the missing-AUTH error")?;
        }
        other => {
            return Err(format!(
                "without AUTH SELECT 1 answered {other:?}, expected ERR auth"
            ))
        }
    }
    drop(c);

    // Wrong password (deliberately unrelated to the real one, so an echo
    // cannot be mistaken for a leak).
    let mut c = Raw::connect(addr, cfg.timeout, "AUTH")?;
    c.send(b"AUTH sdb-wrong-password\n")?;
    match c.response()? {
        Response::Err { class, text } => {
            if class != "auth" {
                return Err(format!(
                    "a wrong password answered ERR {class}, expected auth"
                ));
            }
            no_password(&text, cfg, "the wrong-password error")?;
        }
        other => {
            return Err(format!(
                "a wrong password answered {other:?}, expected ERR auth"
            ))
        }
    }
    drop(c);

    // Correct AUTH.
    let mut c = auth(addr, cfg)?;
    expect_one(&mut c, "after AUTH")?;
    Ok(())
}

fn fuzz_checks(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut c = auth(addr, cfg)?;
    c.set_step("byte fuzzing");
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for i in 0..60usize {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let len = 1 + (x as usize % 120);
        let mut line = Vec::with_capacity(len);
        // A leading letter guarantees the line is not whitespace-only (the
        // server may skip blank lines without answering).
        line.push(b'A' + (x % 26) as u8);
        while line.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let b = (x & 0xff) as u8;
            line.push(if b == b'\n' || b == b'\r' { b'z' } else { b });
        }
        c.send(&line)?;
        c.send(b"\n")?;
        match c.response() {
            Ok(_) => {}
            Err(e) => return Err(format!("byte fuzz line {i} killed the connection: {e}")),
        }
    }
    expect_one(&mut c, "after byte fuzzing")
}

fn mutation_checks(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut c = auth(addr, cfg)?;
    c.set_step("grammar mutations");
    let bases = [
        "SELECT 1",
        "CREATE TABLE mut (a INTEGER)",
        "INSERT INTO mut VALUES (1)",
        "SELECT a FROM mut ORDER BY a",
        "SELECT COUNT(*) FROM mut",
    ];
    for (i, base) in bases.iter().enumerate() {
        for kind in 0..6 {
            let mut s = base.to_string();
            match kind {
                0 => s = s.replace(' ', ""),
                1 => s = s.to_uppercase(),
                2 => s.push(';'),
                3 => s = s.chars().rev().collect(),
                4 => s = s.replace("SELECT", "SELEC"),
                _ => s.insert(1, '('),
            }
            c.send(s.as_bytes())?;
            c.send(b"\n")?;
            match c.response() {
                Ok(_) => {}
                Err(e) => {
                    return Err(format!(
                        "grammar mutation {i}.{kind} ({s:?}) killed the connection: {e}"
                    ))
                }
            }
        }
    }
    expect_one(&mut c, "after grammar mutations")
}

fn nesting_check(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut c = auth(addr, cfg)?;
    c.set_step("the deeply nested expression");
    let depth = cfg.max_depth + 1;
    let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
    expect_limit(c.exec(&sql)?, "a deeply nested expression")?;
    expect_one(&mut c, "after a deeply nested expression")
}

fn oversize_check(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut c = auth(addr, cfg)?;
    c.set_step("the oversized statement");
    let mut sql = String::with_capacity(cfg.max_stmt + 32);
    sql.push_str("SELECT '");
    sql.extend(std::iter::repeat_n('x', cfg.max_stmt + 1));
    sql.push('\'');
    c.send(sql.as_bytes())?;
    c.send(b"\n")?;
    expect_limit(c.response()?, "a statement over the size cap")?;
    expect_one(&mut c, "after an oversized statement")
}

fn slow_loris_check(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut slow = auth(addr, cfg)?;
    slow.set_step("the slow-loris partial line");
    slow.send(b"SELECT ")?; // a partial statement, no newline
    std::thread::sleep(Duration::from_millis(800));
    let mut c = auth(addr, cfg)?;
    c.set_step("another connection while slow-loris is pending");
    let started = Instant::now();
    expect_one(&mut c, "while a slow-loris client is pending")?;
    if started.elapsed() > Duration::from_secs(5) {
        return Err(format!(
            "a slow-loris client delayed another connection by {:.1}s",
            started.elapsed().as_secs_f64()
        ));
    }
    drop(slow);
    Ok(())
}

fn statement_timeout_check(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut c = auth(addr, cfg)?;
    c.set_step("the statement-budget step");
    for t in ["a", "b", "c", "d"] {
        expect_ok(
            c.exec(&format!("CREATE TABLE {t} (k INTEGER)"))?,
            &format!("CREATE TABLE {t}"),
        )?;
    }
    // Duplicated keys make the four-way join huge; 1e9 matches take a
    // healthy engine well over five seconds.
    for t in ["a", "b", "c", "d"] {
        expect_ok(c.exec("BEGIN")?, "BEGIN (load)")?;
        for i in 0..1000 {
            c.send(format!("INSERT INTO {t} VALUES ({})\n", i % 10).as_bytes())?;
        }
        for i in 0..1000 {
            expect_ok(c.response()?, &format!("INSERT {t} row {i}"))?;
        }
        expect_ok(c.exec("COMMIT")?, "COMMIT (load)")?;
    }
    let started = Instant::now();
    let resp = c.exec(
        "SELECT COUNT(*) FROM a JOIN b ON a.k = b.k JOIN c ON b.k = c.k JOIN d ON c.k = d.k",
    )?;
    let elapsed = started.elapsed();
    expect_limit(resp, "a statement over the time budget")?;
    // The workload must actually have run: a fixed "ERR limit" reply that
    // never executes anything is not a timeout guard.
    if elapsed.as_secs_f64() < cfg.statement_secs * 0.6 {
        return Err(format!(
            "the {:.1}s statement was cancelled after {:.2}s; the guard did not run the workload",
            cfg.statement_secs,
            elapsed.as_secs_f64()
        ));
    }
    expect_one(&mut c, "after a cancelled statement")
}

/// AUTH for the cap fill. A refusal answered `ERR limit` is retryable:
/// the earlier checks' closes can take a few ms to free their slots, and
/// a correct server may still count a closed socket when the fill starts
/// (CI 42655763250: "connection 3 of the first 4 was refused"). Any other
/// failure is final. The bool says whether the error may be retried.
fn auth_for_cap(addr: &str, cfg: &SecurityConfig) -> Result<Raw, (bool, String)> {
    let mut c = match Raw::connect(addr, cfg.timeout, "AUTH") {
        Ok(c) => c,
        Err(e) => return Err((false, e)),
    };
    if let Err(e) = c.send(format!("AUTH {}\n", cfg.password).as_bytes()) {
        return Err((false, e));
    }
    match c.response() {
        Ok(Response::Ok(_)) => Ok(c),
        Ok(Response::Err { class, text }) if class == "limit" => {
            Err((true, format!("AUTH answered ERR limit: {text}")))
        }
        Ok(other) => Err((false, format!("AUTH answered {other:?}, expected OK"))),
        Err(e) => Err((false, e)),
    }
}

fn connection_cap_check(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut held = Vec::with_capacity(cfg.max_conns);
    // R75 F6: a refusal answered `ERR limit` is retried for up to 2 s,
    // 50 ms apart, like the release step at the end of this check. Any
    // other refusal, or a cap still full after 2 s, fails as before.
    let fill_deadline = Instant::now() + Duration::from_secs(2);
    for i in 0..cfg.max_conns {
        loop {
            match auth_for_cap(addr, cfg) {
                Ok(c) => {
                    held.push(c);
                    break;
                }
                Err((true, _)) if Instant::now() < fill_deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err((_, e)) => {
                    return Err(format!(
                        "connection {i} of the first {} was refused: {e}",
                        cfg.max_conns
                    ))
                }
            }
        }
    }
    let mut extra = Raw::connect(addr, cfg.timeout, "the connection-cap probe")?;
    extra.send(format!("AUTH {}\n", cfg.password).as_bytes())?;
    match extra.response() {
        Ok(Response::Ok(_)) => {
            return Err(format!(
                "connection {} was accepted beyond the {}-connection cap",
                cfg.max_conns + 1,
                cfg.max_conns
            ))
        }
        // A clean error line or a clean close are both refusals.
        Ok(Response::Err { .. }) | Err(_) => {}
        Ok(other) => {
            return Err(format!(
                "connection {} answered {other:?}, expected a clean refusal",
                cfg.max_conns + 1
            ))
        }
    }
    held[0].set_step("an existing connection with the cap full");
    expect_one(&mut held[0], "an existing connection with the cap full")?;
    drop(extra);
    drop(held);

    // The cap must release when the held connections close.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match auth(addr, cfg) {
            Ok(_) => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(format!("the connection cap did not release: {e}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// The stage-18 contract. The caller owns the password, the scratch store
/// and the cgroup/pids limits.
pub fn scenario(addr: &str, cfg: &SecurityConfig) -> Result<String, String> {
    auth_checks(addr, cfg)?;
    fuzz_checks(addr, cfg)?;
    mutation_checks(addr, cfg)?;
    nesting_check(addr, cfg)?;
    oversize_check(addr, cfg)?;
    slow_loris_check(addr, cfg)?;
    statement_timeout_check(addr, cfg)?;
    connection_cap_check(addr, cfg)?;
    Ok(format!(
        "auth enforced; fuzz/mutations survived; nesting, {}-byte, {:.0}s and slow-loris limits held; {}-connection cap enforced",
        cfg.max_stmt, cfg.statement_secs, cfg.max_conns
    ))
}

#[cfg(test)]
mod tests;
