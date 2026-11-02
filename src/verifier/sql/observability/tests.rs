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
    IndexScan,
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
            if self.mode == Mode::IndexScan {
                format!("INDEX SCAN {table}.{name}")
            } else {
                format!("INDEX {table}.{name}")
            }
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
        if low.starts_with("begin") || low.starts_with("commit") || low.starts_with("rollback") {
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
                    if sock.write_all(text.as_bytes()).is_err() || sock.write_all(b"\n").is_err() {
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

/// Round-15 L3: "INDEX SCAN t.ix" is an index plan, not a second scan.
#[test]
fn an_index_scan_plan_line_is_accepted() {
    let addr = serve(Mode::IndexScan);
    let t = Duration::from_secs(10);
    explain_plans(&addr, t, "ix_t", "ix_k").expect("INDEX SCAN names the index");
}

#[test]
fn the_scenario_repeats_cleanly() {
    let addr = serve(Mode::Good);
    for i in 0..5 {
        scenario(&addr, timeout(), 10 + i).unwrap_or_else(|e| panic!("repeat {i}: {e}"));
    }
}
