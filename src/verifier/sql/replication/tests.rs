//! A tiny three/two-node cluster. The replicas share the mock's store
//! (instant replication); modes remove one property at a time: stale
//! replicas, a lost acked commit, no election, writable replicas, an
//! asynchronous ack and an election during the freeze. Two more modes
//! are correct engines the scenario must pass: a replica that resolves
//! the table first, and one that applies a commit just after acking it.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Good,
    StaleReplica,
    LosesAcked,
    NoElection,
    ReplicaWrites,
    /// ACK the commit without waiting for a replica (round-17 J1).
    AsyncAck,
    /// Resolve the table before the replica role (round-17 J2).
    TableFirst,
    /// Ack on durable receipt and apply 30 ms later: a replica's `SHOW
    /// ROLE` lags the ack (round-18 N1).
    ApplyLag,
    /// The lower-port replica elects itself when it is resumed, while
    /// the primary is still alive (round-18 N3).
    ElectsOnFreeze,
}

#[derive(Default)]
struct State {
    roles: Vec<String>,
    lsn: i64,
    rows: BTreeMap<i64, String>,
    paused: Vec<bool>,
    tables: BTreeSet<String>,
    lag_until: Option<Instant>,
}

fn spawn_cluster(n: usize, mode: Mode) -> (Vec<u16>, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State {
        roles: (0..n)
            .map(|i| if i == 0 { "primary" } else { "replica" }.to_string())
            .collect(),
        paused: vec![false; n],
        ..Default::default()
    }));
    let mut ports = Vec::new();
    for i in 0..n {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        ports.push(listener.local_addr().unwrap().port());
        let state = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if state.lock().unwrap().roles[i] == "dead" {
                    continue;
                }
                let state = state.clone();
                std::thread::spawn(move || handle(stream, i, state, mode, n));
            }
        });
    }
    (ports, state)
}

fn handle(stream: TcpStream, node: usize, state: Arc<Mutex<State>>, mode: Mode, n: usize) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).unwrap_or(0) == 0 {
            break;
        }
        let sql = String::from_utf8_lossy(&line);
        let sql = sql.trim_end_matches('\n').to_string();
        let low = sql.to_ascii_lowercase();
        if low == "commit" {
            // The sync-commit gate: an asynchronous engine answers even
            // while every replica is frozen; a synchronous one waits for
            // a replica to be resumed (round-17 J1).
            let reply = if mode == Mode::AsyncAck {
                "OK 0\n".to_string()
            } else {
                let deadline = Instant::now() + Duration::from_secs(4);
                loop {
                    let ready = {
                        let st = state.lock().unwrap();
                        (1..n).any(|i| !st.paused[i])
                    };
                    if ready {
                        break "OK 0\n".to_string();
                    }
                    if Instant::now() >= deadline {
                        break "ERR busy replica ack timeout\n".to_string();
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            if mode == Mode::ApplyLag {
                state.lock().unwrap().lag_until = Some(Instant::now() + Duration::from_millis(30));
            }
            if writer.write_all(reply.as_bytes()).is_err() {
                break;
            }
            continue;
        }
        let reply = {
            let mut st = state.lock().unwrap();
            let role = st.roles[node].clone();
            if low == "show role" {
                let lagging = mode == Mode::ApplyLag
                    && role == "replica"
                    && st.lag_until.is_some_and(|t| Instant::now() < t);
                let lsn = if lagging { st.lsn - 1 } else { st.lsn };
                format!("ROWS 2\nname\tvalue\nrole\t{role}\nlsn\t{lsn}\nEND 2\n")
            } else if low == "promote" {
                st.roles[node] = "primary".to_string();
                "OK 0\n".to_string()
            } else if low.starts_with("insert") {
                let table = sql
                    .split_whitespace()
                    .nth(2)
                    .unwrap_or("")
                    .trim_matches('(')
                    .to_string();
                if role != "primary" && mode != Mode::ReplicaWrites {
                    if mode == Mode::TableFirst && !st.tables.contains(&table) {
                        "ERR no_table replica\n".to_string()
                    } else {
                        "ERR readonly replica\n".to_string()
                    }
                } else {
                    st.lsn += 1;
                    let tail = sql
                        .trim_end_matches(')')
                        .rsplit_once('(')
                        .map(|(_, t)| t)
                        .unwrap_or("0, ''");
                    let id = tail
                        .split(',')
                        .next()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                        .unwrap_or(0);
                    let v = tail
                        .split_once('\'')
                        .and_then(|(_, r)| r.strip_suffix('\''))
                        .unwrap_or("")
                        .to_string();
                    st.rows.insert(id, v);
                    "OK 1\n".to_string()
                }
            } else if low.starts_with("create") {
                let table = sql
                    .split_whitespace()
                    .nth(2)
                    .unwrap_or("")
                    .trim_matches('(')
                    .to_string();
                if !table.is_empty() {
                    st.tables.insert(table);
                }
                "OK 0\n".to_string()
            } else if low.starts_with("begin") {
                "OK 0\n".to_string()
            } else if low.starts_with("select id, v") {
                let mut rows: Vec<Vec<String>> = if mode == Mode::StaleReplica && node != 0 {
                    vec![]
                } else {
                    st.rows
                        .iter()
                        .map(|(id, v)| vec![id.to_string(), v.clone()])
                        .collect()
                };
                rows.sort_by_key(|r| r[0].parse::<i64>().unwrap_or(0));
                let mut out = "ROWS 2\nid\tv\n".to_string();
                for r in &rows {
                    out.push_str(&r.join("\t"));
                    out.push('\n');
                }
                out.push_str(&format!("END {}\n", rows.len()));
                out
            } else if low.starts_with("select count(*)") {
                let n = if mode == Mode::StaleReplica && node != 0 {
                    0
                } else {
                    st.rows.len()
                };
                format!("ROWS 1\nCOUNT(*)\n{n}\nEND 1\n")
            } else if low.starts_with("select v") {
                let id = low
                    .split("id = ")
                    .nth(1)
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(-1);
                let rows = if mode == Mode::StaleReplica && node != 0 {
                    vec![]
                } else {
                    st.rows
                        .get(&id)
                        .map(|v| vec![vec![v.clone()]])
                        .unwrap_or_default()
                };
                let mut out = format!(
                    "ROWS {}\nvalue\n",
                    rows.first().map(|r| r.len()).unwrap_or(1)
                );
                for r in &rows {
                    out.push_str(&r.join("\t"));
                    out.push('\n');
                }
                out.push_str(&format!("END {}\n", rows.len()));
                out
            } else {
                "ERR syntax mock\n".to_string()
            }
        };
        if writer.write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
}

fn control(
    state: Arc<Mutex<State>>,
    mode: Mode,
    n: usize,
    ports: Vec<u16>,
) -> impl FnMut(HaAction) -> Result<(), String> {
    move |action: HaAction| {
        let mut st = state.lock().unwrap();
        match action {
            HaAction::Kill(0) => {
                st.roles[0] = "dead".to_string();
                if mode == Mode::LosesAcked {
                    st.rows.clear();
                }
                if mode != Mode::NoElection {
                    // The replicas share the LSN, so the tie goes to the
                    // survivor with the lowest port.
                    let mut best = 1;
                    if n >= 3 && ports[2] < ports[1] {
                        best = 2;
                    }
                    st.roles[best] = "primary".to_string();
                    for i in 1..n {
                        if i != best {
                            st.roles[i] = "replica".to_string();
                        }
                    }
                }
            }
            HaAction::Kill(_) => {}
            HaAction::Rejoin { node, .. } => {
                st.roles[node] = "replica".to_string();
            }
            HaAction::Pause(node) => {
                if node < st.paused.len() {
                    st.paused[node] = true;
                }
            }
            HaAction::Resume(node) => {
                if node < st.paused.len() {
                    st.paused[node] = false;
                }
                if mode == Mode::ElectsOnFreeze && n >= 3 {
                    let lower = if ports[1] < ports[2] { 1 } else { 2 };
                    st.roles[lower] = "primary".to_string();
                }
            }
        }
        Ok(())
    }
}

fn cfg() -> ReplicationConfig {
    ReplicationConfig {
        timeout: Duration::from_secs(2),
        election: Duration::from_millis(500),
        seed: 0x19,
    }
}

#[test]
fn the_full_scenario_passes_against_a_faithful_cluster() {
    let (ports, state) = spawn_cluster(3, Mode::Good);
    let mut ctl = control(state, Mode::Good, 3, ports.clone());
    let (winner, report) = scenario(&ports, &cfg(), &mut ctl).expect("faithful cluster");
    let expected = if ports[1] < ports[2] { 1 } else { 2 };
    assert_eq!(winner, expected, "the lower-port replica wins the tie");
    assert!(report.contains("elected at lsn"), "{report}");
}

#[test]
fn an_async_commit_ack_is_caught() {
    let (ports, state) = spawn_cluster(3, Mode::AsyncAck);
    let mut ctl = control(state, Mode::AsyncAck, 3, ports.clone());
    let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
    assert!(err.contains("COMMIT answered within one second"), "{err}");
}

#[test]
fn an_async_lite_commit_ack_is_caught() {
    let (ports, state) = spawn_cluster(2, Mode::AsyncAck);
    let mut ctl = control(state, Mode::AsyncAck, 2, ports.clone());
    let err = lite_scenario(&ports, &cfg(), &mut ctl).unwrap_err();
    assert!(err.contains("COMMIT answered within one second"), "{err}");
}

#[test]
fn a_replica_that_applies_after_the_ack_still_passes() {
    let (ports, state) = spawn_cluster(2, Mode::ApplyLag);
    let mut ctl = control(state, Mode::ApplyLag, 2, ports.clone());
    let (winner, _) = lite_scenario(&ports, &cfg(), &mut ctl).expect("apply lag, lite");
    assert_eq!(winner, 1);
    let (ports, state) = spawn_cluster(3, Mode::ApplyLag);
    let mut ctl = control(state, Mode::ApplyLag, 3, ports.clone());
    scenario(&ports, &cfg(), &mut ctl).expect("apply lag, full");
}

#[test]
fn an_election_during_the_freeze_is_caught() {
    let (ports, state) = spawn_cluster(3, Mode::ElectsOnFreeze);
    let mut ctl = control(state, Mode::ElectsOnFreeze, 3, ports.clone());
    let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
    assert!(err.contains("the freeze triggered an election"), "{err}");
}

#[test]
fn a_table_first_replica_still_passes() {
    let (ports, state) = spawn_cluster(3, Mode::TableFirst);
    let mut ctl = control(state, Mode::TableFirst, 3, ports.clone());
    let (winner, _) = scenario(&ports, &cfg(), &mut ctl).expect("table-first replica");
    let expected = if ports[1] < ports[2] { 1 } else { 2 };
    assert_eq!(winner, expected);
}

#[test]
fn a_table_first_lite_replica_still_passes() {
    let (ports, state) = spawn_cluster(2, Mode::TableFirst);
    let mut ctl = control(state, Mode::TableFirst, 2, ports.clone());
    let (winner, _) = lite_scenario(&ports, &cfg(), &mut ctl).expect("table-first lite replica");
    assert_eq!(winner, 1);
}

#[test]
fn stale_replicas_are_caught() {
    let (ports, state) = spawn_cluster(3, Mode::StaleReplica);
    let mut ctl = control(state, Mode::StaleReplica, 3, ports.clone());
    let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
    assert!(err.contains("rows"), "{err}");
}

#[test]
fn losing_the_acked_commit_is_caught() {
    let (ports, state) = spawn_cluster(3, Mode::LosesAcked);
    let mut ctl = control(state, Mode::LosesAcked, 3, ports.clone());
    let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
    assert!(err.contains("rows"), "{err}");
}

#[test]
fn a_missing_election_is_caught() {
    let (ports, state) = spawn_cluster(3, Mode::NoElection);
    let mut ctl = control(state, Mode::NoElection, 3, ports.clone());
    let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
    assert!(err.contains("exactly one primary"), "{err}");
}

#[test]
fn writable_replicas_are_caught() {
    let (ports, state) = spawn_cluster(3, Mode::ReplicaWrites);
    let mut ctl = control(state, Mode::ReplicaWrites, 3, ports.clone());
    let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
    assert!(err.contains("readonly"), "{err}");
}

#[test]
fn the_lite_scenario_passes_and_keeps_the_acked_rows() {
    let (ports, state) = spawn_cluster(2, Mode::Good);
    let mut ctl = control(state, Mode::Good, 2, ports.clone());
    let (winner, report) = lite_scenario(&ports, &cfg(), &mut ctl).expect("faithful lite cluster");
    assert_eq!(winner, 1);
    assert!(report.contains("promoted at lsn"), "{report}");
}
