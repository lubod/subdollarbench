//! A scripted PostgreSQL frontend mock. It speaks just enough of v3 for
//! the scenario's fixed statements; modes break one property at a time.

use super::*;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Good,
    AcceptsSsl,
    NoParameterStatus,
    WrongSqlstate,
    BrokenExtended,
    NoReady,
    AcceptsAnyPassword,
}

#[derive(Default)]
struct Store {
    rows: Vec<(i64, String)>,
    txn: u8,
    duplicate_seen: bool,
}

fn msg(t: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![t];
    v.extend_from_slice(&be_i32((body.len() + 4) as i32));
    v.extend_from_slice(body);
    v
}

fn rowdesc(names: &[&str]) -> Vec<u8> {
    let mut b = be_i16(names.len() as i16).to_vec();
    for n in names {
        b.extend_from_slice(&cstr(n));
        b.extend_from_slice(&be_i32(0));
        b.extend_from_slice(&be_i16(0));
        b.extend_from_slice(&be_i32(25));
        b.extend_from_slice(&be_i16(-1));
        b.extend_from_slice(&be_i32(-1));
        b.extend_from_slice(&be_i16(0));
    }
    msg(b'T', &b)
}

fn datarow(values: &[&str]) -> Vec<u8> {
    let mut b = be_i16(values.len() as i16).to_vec();
    for v in values {
        b.extend_from_slice(&be_i32(v.len() as i32));
        b.extend_from_slice(v.as_bytes());
    }
    msg(b'D', &b)
}

fn complete(tag: &str) -> Vec<u8> {
    msg(b'C', &cstr(tag))
}

fn ready(txn: u8) -> Vec<u8> {
    msg(b'Z', &[txn])
}

fn error(code: &str, text: &str) -> Vec<u8> {
    let mut b = b"SERROR\0VERROR\0C".to_vec();
    b.extend_from_slice(code.as_bytes());
    b.extend_from_slice(b"\0M");
    b.extend_from_slice(text.as_bytes());
    b.extend_from_slice(b"\0\0");
    msg(b'E', &b)
}

fn read_msg(r: &mut BufReader<TcpStream>) -> Option<(u8, Vec<u8>)> {
    let mut t = [0u8; 1];
    r.read_exact(&mut t).ok()?;
    let mut l = [0u8; 4];
    r.read_exact(&mut l).ok()?;
    let n = i32::from_be_bytes(l);
    let mut b = vec![0u8; (n - 4).max(0) as usize];
    r.read_exact(&mut b).ok()?;
    Some((t[0], b))
}

fn send(w: &mut TcpStream, bytes: &[u8]) {
    let _ = w.write_all(bytes);
}

fn handle(stream: TcpStream, mode: Mode, store: Arc<Mutex<Store>>) {
    let mut r = BufReader::new(stream.try_clone().unwrap());
    let mut w = stream;
    // SSL request (or a direct startup).
    let mut hdr = [0u8; 8];
    if r.read_exact(&mut hdr).is_err() {
        return;
    }
    let (mut len, mut code) = (
        i32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]),
        i32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]),
    );
    if len == 8 && code == 80877103 {
        if mode == Mode::AcceptsSsl {
            send(&mut w, b"S");
            return;
        }
        send(&mut w, b"N");
        if r.read_exact(&mut hdr).is_err() {
            return;
        }
        len = i32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        code = i32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    }
    if code != 196608 {
        return;
    }
    let mut rest = vec![0u8; (len - 8).max(0) as usize];
    if r.read_exact(&mut rest).is_err() {
        return;
    }
    // Cleartext password (the scenario always sends one).
    send(&mut w, &msg(b'R', &be_i32(3)));
    let Some((t, p)) = read_msg(&mut r) else {
        return;
    };
    if t != b'p'
        || (mode != Mode::AcceptsAnyPassword && p.split(|&x| x == 0).next() != Some(b"pw-test"))
    {
        send(&mut w, &error("28P01", "bad password"));
        return;
    }
    send(&mut w, &msg(b'R', &be_i32(0)));
    if mode != Mode::NoParameterStatus {
        let mut s = b"server_version\0".to_vec();
        s.extend_from_slice(b"16.0\0");
        send(&mut w, &msg(b'S', &s));
        let mut s = b"client_encoding\0".to_vec();
        s.extend_from_slice(b"UTF8\0");
        send(&mut w, &msg(b'S', &s));
    }
    send(&mut w, &msg(b'K', &[0, 0, 0, 1, 0, 0, 0, 2]));
    send(&mut w, &ready(b'I'));

    let mut stmt = String::new();
    let mut params: Vec<String> = Vec::new();
    loop {
        let Some((t, b)) = read_msg(&mut r) else {
            break;
        };
        let text = String::from_utf8_lossy(&b).to_string();
        match t {
            b'Q' => {
                let sql = text.split('\0').next().unwrap_or("").to_string();
                let reply = script(&sql, mode, &store);
                send(&mut w, &reply);
                if mode == Mode::NoReady {
                    return;
                }
            }
            b'P' => {
                if mode == Mode::BrokenExtended {
                    send(&mut w, &error("42601", "mock parse failure"));
                    continue;
                }
                let parts: Vec<&str> = text.split('\0').collect();
                stmt = parts.get(1).unwrap_or(&"").to_string();
                params.clear();
                send(&mut w, &msg(b'1', &[]));
            }
            b'B' => {
                params.clear();
                // skip portal/stmt/formats; find the parameter bytes
                let parts: Vec<&[u8]> = b.split(|&x| x == 0).collect();
                // The Bind body is binary; extract the text after the
                // statement name by looking for the known literal.
                let raw = String::from_utf8_lossy(&b).to_string();
                if let Some(p) = raw.split("hello").nth(1) {
                    params.push("hello".to_string());
                    let _ = p;
                }
                let _ = parts;
                send(&mut w, &msg(b'2', &[]));
            }
            b'D' => send(&mut w, &msg(b'n', &[])),
            b'E' => {
                if mode == Mode::BrokenExtended {
                    send(&mut w, &error("42601", "mock execute failure"));
                    continue;
                }
                let low = stmt.to_ascii_lowercase();
                if !low.starts_with("select") {
                    send(&mut w, &error("42601", "syntax error"));
                } else if low.contains("$1") {
                    send(&mut w, &rowdesc(&["?column?"]));
                    send(&mut w, &datarow(&["hello"]));
                    send(&mut w, &complete("SELECT 1"));
                } else {
                    send(&mut w, &rowdesc(&["?column?"]));
                    send(&mut w, &datarow(&["2"]));
                    send(&mut w, &complete("SELECT 1"));
                }
            }
            b'S' => send(&mut w, &ready(b'I')),
            b'C' => send(&mut w, &msg(b'3', &[])),
            b'X' => break,
            b'H' => {}
            _ => send(&mut w, &error("08P01", "unsupported")),
        }
    }
}

fn script(sql: &str, mode: Mode, store: &Arc<Mutex<Store>>) -> Vec<u8> {
    let low = sql.to_ascii_lowercase();
    let mut st = store.lock().unwrap();
    let mut out = Vec::new();
    if low.starts_with("select 1") || low.starts_with("select 2") {
        let v = if low.contains('2') { "2" } else { "1" };
        out.extend_from_slice(&rowdesc(&["?column?"]));
        out.extend_from_slice(&datarow(&[v]));
        out.extend_from_slice(&complete("SELECT 1"));
    } else if low.starts_with("create table") {
        out.extend_from_slice(&complete("CREATE TABLE"));
    } else if low.starts_with("insert") {
        let dup = low.contains("(1,") && st.rows.iter().any(|(i, _)| *i == 1);
        if dup {
            st.duplicate_seen = true;
            out.extend_from_slice(&error("23505", "duplicate key"));
        } else {
            let id = if low.contains("(2,") { 2 } else { 1 };
            let v = low.split('\'').nth(1).unwrap_or("").to_string();
            st.rows.push((id, v));
            out.extend_from_slice(&complete("INSERT 0 1"));
        }
    } else if low.starts_with("select count(*)") {
        out.extend_from_slice(&rowdesc(&["count"]));
        out.extend_from_slice(&datarow(&[&st.rows.len().to_string()]));
        out.extend_from_slice(&complete("SELECT 1"));
    } else if low.starts_with("select v") {
        let v = st
            .rows
            .iter()
            .find(|(i, _)| *i == 1)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        out.extend_from_slice(&rowdesc(&["v"]));
        out.extend_from_slice(&datarow(&[&v]));
        out.extend_from_slice(&complete("SELECT 1"));
    } else if low.starts_with("begin") {
        st.txn = b'T';
        out.extend_from_slice(&complete("BEGIN"));
    } else if low.starts_with("commit") {
        st.txn = b'I';
        out.extend_from_slice(&complete("COMMIT"));
    } else if low.contains("missing") {
        let code = if mode == Mode::WrongSqlstate {
            "42601"
        } else {
            "42P01"
        };
        out.extend_from_slice(&error(code, "no such table"));
    } else if low.starts_with("selec") {
        out.extend_from_slice(&error("42601", "syntax error"));
    } else {
        out.extend_from_slice(&error("42601", "mock syntax"));
    }
    if mode != Mode::NoReady {
        out.extend_from_slice(&ready(st.txn));
    }
    out
}

fn spawn(mode: Mode) -> String {
    spawn_with_store(mode).0
}

fn spawn_with_store(mode: Mode) -> (String, Arc<Mutex<Store>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let store = Arc::new(Mutex::new(Store {
        rows: Vec::new(),
        txn: b'I',
        duplicate_seen: false,
    }));
    let kept = store.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let store = store.clone();
            std::thread::spawn(move || handle(stream, mode, store));
        }
    });
    (addr, kept)
}

fn cfg(extended: bool) -> PgConfig {
    PgConfig {
        user: "sdb".to_string(),
        password: Some("pw-test".to_string()),
        timeout: Duration::from_secs(2),
        extended,
        unique_keys: true,
    }
}

#[test]
fn startup_timeout_is_readable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept the probe");
        std::thread::sleep(Duration::from_millis(300));
        drop(stream);
    });
    let mut c = cfg(true);
    c.timeout = Duration::from_millis(100);
    let err = PgConn::connect(&addr, &c).unwrap_err();
    assert!(
        err.contains("no answer within 0.1s to the SSLRequest startup step"),
        "{err}"
    );
    let _ = handle.join();
}

#[test]
fn a_faithful_full_frontend_passes() {
    let addr = spawn(Mode::Good);
    let report = scenario(&addr, &cfg(true)).expect("faithful full mock");
    assert!(report.contains("extended"), "{report}");
}

#[test]
fn the_lite_skips_extended_query() {
    let addr = spawn(Mode::BrokenExtended);
    let report = scenario(&addr, &cfg(false)).expect("lite mock ignores extended");
    assert!(report.contains("simple query"), "{report}");
}

#[test]
fn accepting_ssl_is_caught() {
    let addr = spawn(Mode::AcceptsSsl);
    let err = scenario(&addr, &cfg(true)).unwrap_err();
    assert!(err.contains("SSLRequest"), "{err}");
}

#[test]
fn a_missing_parameter_status_is_caught() {
    let addr = spawn(Mode::NoParameterStatus);
    let err = scenario(&addr, &cfg(true)).unwrap_err();
    assert!(err.contains("server_version"), "{err}");
}

#[test]
fn a_wrong_sqlstate_is_caught() {
    let addr = spawn(Mode::WrongSqlstate);
    let err = scenario(&addr, &cfg(true)).unwrap_err();
    assert!(err.contains("42P01"), "{err}");
}

#[test]
fn broken_extended_query_is_caught() {
    let addr = spawn(Mode::BrokenExtended);
    let err = scenario(&addr, &cfg(true)).unwrap_err();
    assert!(err.contains("extended") || err.contains("42601"), "{err}");
}

#[test]
fn ticket_9_absent_skips_the_duplicate_probe() {
    let (addr, store) = spawn_with_store(Mode::Good);
    let mut c = cfg(true);
    c.unique_keys = false;
    scenario(&addr, &c).expect("the no-ticket-9 path");
    assert!(
        !store.lock().unwrap().duplicate_seen,
        "the scenario sent the 23505 probe without ticket 9"
    );
}

#[test]
fn accepting_the_wrong_password_is_caught() {
    let addr = spawn(Mode::AcceptsAnyPassword);
    let err = scenario(&addr, &cfg(true)).unwrap_err();
    assert!(err.contains("accepted the wrong password"), "{err}");
}

#[test]
fn no_ready_for_query_is_caught() {
    let addr = spawn(Mode::NoReady);
    let err = scenario(&addr, &cfg(false)).unwrap_err();
    assert!(err.contains("ReadyForQuery"), "{err}");
}
