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

use super::wire::{read_response, Response};
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
}

impl Raw {
    fn connect(addr: &str, timeout: Duration) -> Result<Self, String> {
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
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.reader
            .get_mut()
            .write_all(bytes)
            .map_err(|e| format!("write: {e}"))
    }

    fn response(&mut self) -> Result<Response, String> {
        read_response(&mut self.reader)
    }

    fn exec(&mut self, sql: &str) -> Result<Response, String> {
        self.send(sql.as_bytes())?;
        self.send(b"\n")?;
        self.response()
    }
}

fn auth(addr: &str, cfg: &SecurityConfig) -> Result<Raw, String> {
    let mut c = Raw::connect(addr, cfg.timeout)?;
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
    let mut c = Raw::connect(addr, cfg.timeout)?;
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
    let mut c = Raw::connect(addr, cfg.timeout)?;
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
    let depth = cfg.max_depth + 1;
    let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
    expect_limit(c.exec(&sql)?, "a deeply nested expression")?;
    expect_one(&mut c, "after a deeply nested expression")
}

fn oversize_check(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut c = auth(addr, cfg)?;
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
    slow.send(b"SELECT ")?; // a partial statement, no newline
    std::thread::sleep(Duration::from_millis(800));
    let mut c = auth(addr, cfg)?;
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

fn connection_cap_check(addr: &str, cfg: &SecurityConfig) -> Result<(), String> {
    let mut held = Vec::with_capacity(cfg.max_conns);
    for i in 0..cfg.max_conns {
        match auth(addr, cfg) {
            Ok(c) => held.push(c),
            Err(e) => {
                return Err(format!(
                    "connection {i} of the first {} was refused: {e}",
                    cfg.max_conns
                ))
            }
        }
    }
    let mut extra = Raw::connect(addr, cfg.timeout)?;
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
mod tests {
    //! A small line-protocol mock with the same limits. Modes remove one
    //! control at a time (auth, password hygiene, the limits, the cap), and
    //! the scenario must catch each.

    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Good,
        NoAuth,
        LeaksPassword,
        NoLimits,
        Uncapped,
    }

    fn depth_ok(sql: &str, max: usize) -> bool {
        let mut depth = 0i64;
        for ch in sql.chars() {
            if ch == '(' {
                depth += 1;
                if depth >= max as i64 {
                    return false;
                }
            } else if ch == ')' {
                depth -= 1;
            }
        }
        true
    }

    fn auth_error(mode: Mode, cfg: &SecurityConfig) -> String {
        if mode == Mode::LeaksPassword {
            format!("ERR auth password {} rejected\n", cfg.password)
        } else {
            "ERR auth authentication required\n".to_string()
        }
    }

    fn handle(stream: TcpStream, cfg: &SecurityConfig, mode: Mode) {
        let reader_stream = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(reader_stream);
        let mut writer = stream;
        let mut authed = mode == Mode::NoAuth;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line).unwrap_or(0);
            if n == 0 {
                break;
            }
            let sql = String::from_utf8_lossy(&line);
            let sql = sql.trim_end_matches('\n');
            if sql.is_empty() {
                continue;
            }
            if !authed {
                if let Some(pw) = sql.strip_prefix("AUTH ") {
                    if pw == cfg.password {
                        authed = true;
                        let _ = writer.write_all(b"OK 0\n");
                        continue;
                    }
                }
                let _ = writer.write_all(auth_error(mode, cfg).as_bytes());
                continue;
            }
            if mode != Mode::NoLimits {
                if sql.len() > cfg.max_stmt {
                    let _ = writer.write_all(b"ERR limit statement too large\n");
                    continue;
                }
                if !depth_ok(sql, cfg.max_depth) {
                    let _ = writer.write_all(b"ERR limit nesting too deep\n");
                    continue;
                }
            }
            let low = sql.to_ascii_lowercase();
            if low.contains(" join ") && low.contains("count(*)") {
                if mode == Mode::NoLimits {
                    let _ = writer.write_all(b"ROWS 1\nCOUNT(*)\n0\nEND 1\n");
                } else {
                    std::thread::sleep(Duration::from_secs_f64(cfg.statement_secs));
                    let _ = writer.write_all(b"ERR limit interrupted\n");
                }
                continue;
            }
            let reply: &[u8] = if sql.eq_ignore_ascii_case("SELECT 1") {
                b"ROWS 1\n1\n1\nEND 1\n"
            } else if low.starts_with("create")
                || low.starts_with("begin")
                || low.starts_with("commit")
                || low.starts_with("rollback")
            {
                b"OK 0\n"
            } else if low.starts_with("insert") {
                b"OK 1\n"
            } else {
                b"ERR syntax mock\n"
            };
            if writer.write_all(reply).is_err() {
                break;
            }
        }
    }

    fn spawn(mode: Mode, cfg: SecurityConfig) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let current = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let now = current.load(Ordering::SeqCst);
                if mode != Mode::Uncapped && now >= cfg.max_conns {
                    let mut s = stream;
                    let _ = s.write_all(b"ERR limit too many connections\n");
                    continue;
                }
                current.fetch_add(1, Ordering::SeqCst);
                let cfg = cfg.clone();
                let current = current.clone();
                std::thread::spawn(move || {
                    handle(stream, &cfg, mode);
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        addr
    }

    fn cfg() -> SecurityConfig {
        SecurityConfig {
            password: "test-password".to_string(),
            timeout: Duration::from_secs(10),
            max_stmt: 512,
            max_depth: 32,
            max_conns: 4,
            statement_secs: 0.2,
        }
    }

    #[test]
    fn a_faithful_server_passes_the_whole_scenario() {
        let c = cfg();
        let addr = spawn(Mode::Good, c.clone());
        let report = scenario(&addr, &c).expect("the faithful mock must pass");
        assert!(report.contains("auth enforced"), "{report}");
        assert!(report.contains("4-connection cap"), "{report}");
    }

    #[test]
    fn missing_auth_is_caught() {
        let c = cfg();
        let addr = spawn(Mode::NoAuth, c.clone());
        let err = scenario(&addr, &c).unwrap_err();
        assert!(err.contains("without AUTH"), "{err}");
    }

    #[test]
    fn a_password_leak_is_caught() {
        let c = cfg();
        let addr = spawn(Mode::LeaksPassword, c.clone());
        let err = scenario(&addr, &c).unwrap_err();
        assert!(err.contains("password"), "{err}");
    }

    #[test]
    fn missing_limits_are_caught() {
        let c = cfg();
        let addr = spawn(Mode::NoLimits, c.clone());
        let err = scenario(&addr, &c).unwrap_err();
        assert!(err.contains("ERR limit"), "{err}");
    }

    #[test]
    fn an_uncapped_server_is_caught() {
        let c = cfg();
        let addr = spawn(Mode::Uncapped, c.clone());
        let err = scenario(&addr, &c).unwrap_err();
        assert!(err.contains("cap"), "{err}");
    }
}
