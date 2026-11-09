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
    /// R75 F6: a faithful server that frees a closed connection's slot
    /// 100 ms after it notices the close.
    SlowRelease,
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
                if mode == Mode::SlowRelease {
                    std::thread::sleep(Duration::from_millis(100));
                }
                current.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    addr
}

/// R75 F6: a server that never frees a slot: every connection is refused
/// with the cap's error line.
fn spawn_stuck_full() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.write_all(b"ERR limit too many connections\n");
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
fn a_never_answering_server_names_the_step() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept the probe");
        std::thread::sleep(Duration::from_secs(2));
        drop(stream);
    });
    let started = Instant::now();
    let mut c = Raw::connect(&addr, Duration::from_millis(100), "the hostile-input probe").unwrap();
    let e = c.response().unwrap_err();
    assert!(
        e.contains("no answer within 0.1s to the hostile-input probe"),
        "{e}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(c);
    let _ = handle.join();
}

#[test]
fn a_faithful_server_passes_the_whole_scenario() {
    let c = cfg();
    let addr = spawn(Mode::Good, c.clone());
    let report = scenario(&addr, &c).expect("the faithful mock must pass");
    assert!(report.contains("auth enforced"), "{report}");
    assert!(report.contains("4-connection cap"), "{report}");
}

/// R75 F6: the cap fill right after a close can see the closed socket
/// still counted. A faithful server that frees a slot 100 ms after the
/// close must still pass: the fill retries an `ERR limit` refusal for up
/// to 2 s.
#[test]
fn a_slow_release_server_passes_the_cap_check() {
    let c = cfg();
    let addr = spawn(Mode::SlowRelease, c.clone());
    // Warm one connection and close it: its slot frees 100 ms later,
    // while the fill is already opening the cap's four.
    drop(auth(&addr, &c).expect("the warm connection"));
    connection_cap_check(&addr, &c).expect("a faithful slow-release mock must pass");
}

/// R75 F6: the retry must not turn a stuck cap into a pass. A server that
/// never frees a slot still fails with the existing refusal message, after
/// the 2 s retry window.
#[test]
fn a_server_that_never_releases_still_fails() {
    let c = cfg();
    let addr = spawn_stuck_full();
    let started = Instant::now();
    let err = connection_cap_check(&addr, &c).unwrap_err();
    assert!(
        err.contains("connection 0 of the first 4 was refused"),
        "{err}"
    );
    assert!(err.contains("too many connections"), "{err}");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "gave up after {:?}, before the 2 s retry window",
        started.elapsed()
    );
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
