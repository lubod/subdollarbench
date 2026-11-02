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
