//! A tiny in-memory mock of the scenario's SQL. Failure modes corrupt
//! one incident property at a time: acking an unpersisted write,
//! applying a refused write, or damaging a seed row under ENOSPC.

use super::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Good,
    AcksFull,
    AppliesRefused,
    CorruptsSeed,
}

struct State {
    rows: HashMap<i64, String>,
    full: bool,
}

fn rows_response(rows: Vec<Vec<String>>) -> String {
    let mut out = format!("ROWS {}\n", rows.first().map(|r| r.len()).unwrap_or(1));
    out.push_str("value\n");
    for r in &rows {
        out.push_str(&r.join("\t"));
        out.push('\n');
    }
    out.push_str(&format!("END {}\n", rows.len()));
    out
}

fn handle(state: &mut State, mode: Mode, sql: &str) -> String {
    let low = sql.to_ascii_lowercase();
    if low.starts_with("create table") {
        return "OK 0\n".to_string();
    }
    if low.starts_with("insert into t values (") {
        let tail = sql
            .trim_end_matches(')')
            .rsplit_once('(')
            .map(|(_, t)| t)
            .unwrap_or_default();
        let id = tail
            .split(',')
            .next()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(-1);
        let value = tail
            .split_once('\'')
            .and_then(|(_, rest)| rest.strip_suffix('\''))
            .unwrap_or("")
            .to_string();
        if state.full {
            if mode == Mode::AcksFull {
                state.rows.insert(id, value);
                return "OK 1\n".to_string();
            }
            if mode == Mode::AppliesRefused {
                state.rows.insert(id, value);
            }
            if mode == Mode::CorruptsSeed {
                state.rows.insert(0, "corrupted".to_string());
            }
            return "ERR full no space left on device\n".to_string();
        }
        state.rows.insert(id, value);
        return "OK 1\n".to_string();
    }
    if low.starts_with("select count(*)") {
        return rows_response(vec![vec![state.rows.len().to_string()]]);
    }
    if low.starts_with("select v from t where id = ") {
        let id = low
            .split("id = ")
            .nth(1)
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(-1);
        let rows = state
            .rows
            .get(&id)
            .map(|v| vec![vec![v.clone()]])
            .unwrap_or_default();
        return rows_response(rows);
    }
    if low.starts_with("select id, v from t") {
        let mut rows: Vec<Vec<String>> = state
            .rows
            .iter()
            .map(|(id, v)| vec![id.to_string(), v.clone()])
            .collect();
        rows.sort_by_key(|r| r[0].parse::<i64>().unwrap_or(0));
        return rows_response(rows);
    }
    format!("ERR syntax mock does not understand {sql:.80}\n")
}

fn spawn(mode: Mode) -> (String, Arc<AtomicBool>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let full = Arc::new(AtomicBool::new(false));
    let full2 = full.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut state = State {
            rows: HashMap::new(),
            full: false,
        };
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            state.full = full2.load(Ordering::SeqCst);
            let response = handle(&mut state, mode, line.trim_end());
            if writer.write_all(response.as_bytes()).is_err() {
                break;
            }
            line.clear();
        }
    });
    (addr, full, handle)
}

fn cfg() -> DiskConfig {
    DiskConfig {
        seed_rows: 5,
        seed_len: 16,
        fail_len: 64,
        retry_len: 32,
        timeout: Duration::from_secs(10),
    }
}

#[test]
fn the_incident_passes_against_a_faithful_engine() {
    let (addr, full, h) = spawn(Mode::Good);
    let mut control = |op: DiskOp| {
        full.store(op == DiskOp::Fill, Ordering::SeqCst);
        Ok(())
    };
    let report = incident(&addr, &cfg(), &mut control).expect("faithful mock");
    assert!(report.contains("retry succeeded"), "{report}");
    assert!(!full.load(Ordering::SeqCst));
    h.join().unwrap();
}

#[test]
fn acking_an_unpersisted_write_fails() {
    let (addr, full, h) = spawn(Mode::AcksFull);
    let mut control = |op: DiskOp| {
        full.store(op == DiskOp::Fill, Ordering::SeqCst);
        Ok(())
    };
    let err = incident(&addr, &cfg(), &mut control).unwrap_err();
    assert!(err.contains("acked"), "{err}");
    h.join().unwrap();
}

#[test]
fn applying_a_refused_write_fails() {
    let (addr, full, h) = spawn(Mode::AppliesRefused);
    let mut control = |op: DiskOp| {
        full.store(op == DiskOp::Fill, Ordering::SeqCst);
        Ok(())
    };
    let err = incident(&addr, &cfg(), &mut control).unwrap_err();
    assert!(err.contains("changed the row count"), "{err}");
    h.join().unwrap();
}

#[test]
fn corrupting_a_seed_row_fails() {
    let (addr, full, h) = spawn(Mode::CorruptsSeed);
    let mut control = |op: DiskOp| {
        full.store(op == DiskOp::Fill, Ordering::SeqCst);
        Ok(())
    };
    let err = incident(&addr, &cfg(), &mut control).unwrap_err();
    assert!(err.contains("corrupted"), "{err}");
    h.join().unwrap();
}
