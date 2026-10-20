//! Stage 19 (replication and failover): three SQL nodes, `SHOW ROLE`/LSN,
//! read-only replicas, sync-acked commits, election, rejoin
//! (docs/PROPOSAL-sdb-sql.md, "19. Replication and failover").
//!
//! The pipeline starts the cluster with the existing HA container plumbing
//! (`start.sh --port/--data/--replicaof`, plus `SDB_SQL_NODES`). This
//! scenario drives only the observable contract:
//!
//! 1. node 1 is the primary, nodes 2/3 replicas; `SHOW ROLE` reports
//!    `role` and `lsn` on every node;
//! 2. a committed transaction is visible on both replicas; replicas refuse
//!    writes with `ERR readonly`;
//! 3. the primary is killed; within the election budget exactly one
//!    survivor is primary — the highest LSN, tie to the lowest port — and
//!    holds every acked row;
//! 4. a new committed write reaches the other survivor;
//! 5. the old primary is respawned as a replica, converges, and refuses
//!    writes.
//!
//! The lite variant has two nodes and a manual `PROMOTE`; there is no third
//! survivor to ack new commits, so the promoted node acks locally.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ReplicationConfig {
    pub timeout: Duration,
    pub election: Duration,
    pub seed: u64,
}

impl ReplicationConfig {
    pub fn full(seed: u64) -> Self {
        Self {
            timeout: Duration::from_secs(10),
            election: Duration::from_secs(10),
            seed,
        }
    }

    pub fn lite(seed: u64) -> Self {
        Self {
            timeout: Duration::from_secs(10),
            election: Duration::from_secs(10),
            seed,
        }
    }
}

/// Control operations the pipeline performs on the Docker cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaAction {
    /// Stop the node's server (the failover).
    Kill(usize),
    /// Recreate the node as a fresh replica of `primary`.
    Rejoin { node: usize, primary: usize },
}

fn addr(ports: &[u16], i: usize) -> String {
    format!("127.0.0.1:{}", ports[i])
}

fn role(addr: &str, timeout: Duration) -> Result<(String, i64), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    let rows = match c.exec("SHOW ROLE")? {
        Response::Rows { rows, .. } => rows,
        other => return Err(format!("SHOW ROLE answered {other:?}")),
    };
    let mut role = None;
    let mut lsn = None;
    for row in rows {
        match (row.first().map(String::as_str), row.get(1)) {
            (Some("role"), Some(v)) => role = Some(v.clone()),
            (Some("lsn"), Some(v)) => {
                lsn = Some(
                    v.parse::<i64>()
                        .map_err(|e| format!("SHOW ROLE lsn {v:?}: {e}"))?,
                )
            }
            _ => {}
        }
    }
    match (role, lsn) {
        (Some(r), Some(l)) => Ok((r, l)),
        _ => Err("SHOW ROLE is missing role/lsn".to_string()),
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

fn wait_role(
    addr: &str,
    want_primary: bool,
    deadline: Instant,
    what: &str,
) -> Result<(String, i64), String> {
    let mut last;
    loop {
        match role(addr, Duration::from_secs(2)) {
            Ok((r, l)) if (r == "primary") == want_primary => return Ok((r, l)),
            Ok((r, l)) => last = format!("{r} lsn={l}"),
            Err(e) => last = e,
        }
        if Instant::now() >= deadline {
            return Err(format!("{what}: {last}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_lsn(addr: &str, want: i64, deadline: Instant, what: &str) -> Result<(), String> {
    let mut last = 0;
    loop {
        if let Ok((_, l)) = role(addr, Duration::from_secs(2)) {
            last = l;
            if l >= want {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("{what}: lsn {last}, expected >= {want}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn seed_rows(ports: &[u16], cfg: &ReplicationConfig, table: &str) -> Result<i64, String> {
    let mut c = TcpCandidate::connect(addr(ports, 0), cfg.timeout)?;
    expect_ok(
        c.exec(&format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, v TEXT NOT NULL)"
        ))?,
        "CREATE TABLE",
    )?;
    for i in 0..10 {
        expect_ok(
            c.exec(&format!("INSERT INTO {table} VALUES ({i}, 'v{i}')"))?,
            &format!("seed insert {i}"),
        )?;
    }
    expect_ok(c.exec("BEGIN")?, "BEGIN")?;
    for i in 10..15 {
        expect_ok(
            c.exec(&format!("INSERT INTO {table} VALUES ({i}, 'v{i}')"))?,
            &format!("transaction insert {i}"),
        )?;
    }
    expect_ok(c.exec("COMMIT")?, "COMMIT")?;
    // The commit's LSN is the last log entry; the replicas must reach it.
    let (_, last) = role(&addr(ports, 0), cfg.timeout)?;
    Ok(last)
}

fn read_acked(addr: &str, cfg: &ReplicationConfig, table: &str) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, cfg.timeout)?;
    let rows = expect_rows(
        c.exec(&format!("SELECT COUNT(*) FROM {table}"))?,
        "replica COUNT",
    )?;
    if rows != vec![vec!["15".to_string()]] {
        return Err(format!("replica COUNT is {rows:?}, expected 15"));
    }
    let v = expect_rows(
        c.exec(&format!("SELECT v FROM {table} WHERE id = 14"))?,
        "replica value",
    )?;
    if v != vec![vec!["v14".to_string()]] {
        return Err(format!("replica id=14 is {v:?}, expected v14"));
    }
    Ok(())
}

fn replica_refuses(addr: &str, cfg: &ReplicationConfig, table: &str) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, cfg.timeout)?;
    match c.exec(&format!("INSERT INTO {table} VALUES (999, 'forbidden')"))? {
        Response::Err { class, .. } if class == "readonly" => Ok(()),
        other => Err(format!(
            "a write on a replica answered {other:?}, expected ERR readonly"
        )),
    }
}

fn commit_new_row(addr: &str, cfg: &ReplicationConfig, table: &str) -> Result<i64, String> {
    let mut c = TcpCandidate::connect(addr, cfg.timeout)?;
    expect_ok(c.exec("BEGIN")?, "failover BEGIN")?;
    expect_ok(
        c.exec(&format!("INSERT INTO {table} VALUES (100, 'new')"))?,
        "failover INSERT",
    )?;
    expect_ok(c.exec("COMMIT")?, "failover COMMIT")?;
    let (_, lsn) = role(addr, cfg.timeout)?;
    Ok(lsn)
}

fn wait_new_row(
    addr: &str,
    want_lsn: i64,
    cfg: &ReplicationConfig,
    table: &str,
) -> Result<(), String> {
    wait_lsn(
        addr,
        want_lsn,
        Instant::now() + cfg.election,
        "replica did not reach the failover commit",
    )?;
    let mut c = TcpCandidate::connect(addr, cfg.timeout)?;
    let v = expect_rows(
        c.exec(&format!("SELECT v FROM {table} WHERE id = 100"))?,
        "failover value",
    )?;
    if v != vec![vec!["new".to_string()]] {
        return Err(format!("the new row is {v:?}, expected new"));
    }
    Ok(())
}

/// The three-node full contract. Returns the winning node index (0-based)
/// and a report; the caller runs the cumulative suite against that node.
pub fn scenario(
    ports: &[u16],
    cfg: &ReplicationConfig,
    control: &mut dyn FnMut(HaAction) -> Result<(), String>,
) -> Result<(usize, String), String> {
    if ports.len() != 3 {
        return Err("the full replication scenario needs three nodes".to_string());
    }
    let table = format!("ha_{:x}", cfg.seed);
    let deadline = Instant::now() + cfg.timeout;
    wait_role(&addr(ports, 0), true, deadline, "node 1 is not primary")?;
    let (_, base) = role(&addr(ports, 0), cfg.timeout)?;
    for i in [1usize, 2usize] {
        wait_role(&addr(ports, i), false, deadline, "node is not a replica")?;
        wait_lsn(
            &addr(ports, i),
            base,
            deadline,
            "replica never reached the initial LSN",
        )?;
    }

    let commit_lsn = seed_rows(ports, cfg, &table)?;
    for i in [1usize, 2usize] {
        wait_lsn(
            &addr(ports, i),
            commit_lsn,
            Instant::now() + cfg.election,
            "replica missed the acked commit",
        )?;
        read_acked(&addr(ports, i), cfg, &table)?;
        replica_refuses(&addr(ports, i), cfg, &table)?;
    }

    // The replicas' LSNs decide the election; tie goes to the lower port.
    let (_, l1) = role(&addr(ports, 1), cfg.timeout)?;
    let (_, l2) = role(&addr(ports, 2), cfg.timeout)?;
    let expected = if l1 != l2 {
        if l1 > l2 {
            1
        } else {
            2
        }
    } else if ports[1] < ports[2] {
        1
    } else {
        2
    };

    control(HaAction::Kill(0))?;
    let deadline = Instant::now() + cfg.election;
    let winner = loop {
        let mut primaries = Vec::new();
        for i in [1usize, 2usize] {
            if let Ok((r, _)) = role(&addr(ports, i), Duration::from_secs(2)) {
                if r == "primary" {
                    primaries.push(i);
                }
            }
        }
        if primaries.len() == 1 {
            break primaries[0];
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "failover did not leave exactly one primary (found {primaries:?})"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if winner != expected {
        return Err(format!(
            "elected node {} (index {winner}), expected the highest-LSN/lowest-port survivor index {expected}",
            ports[winner]
        ));
    }
    read_acked(&addr(ports, winner), cfg, &table)?;
    let new_lsn = commit_new_row(&addr(ports, winner), cfg, &table)?;
    let other = if winner == 1 { 2 } else { 1 };
    wait_new_row(&addr(ports, other), new_lsn, cfg, &table)?;

    control(HaAction::Rejoin {
        node: 0,
        primary: winner,
    })?;
    wait_role(
        &addr(ports, 0),
        false,
        Instant::now() + cfg.election,
        "the old primary did not rejoin as a replica",
    )?;
    wait_new_row(&addr(ports, 0), new_lsn, cfg, &table)?;
    replica_refuses(&addr(ports, 0), cfg, &table)?;

    Ok((
        winner,
        format!(
            "3 nodes; the acked commit reached both replicas; node 1 killed; \
             node {} elected at lsn {new_lsn}; the survivor and the rejoined \
             old primary converged and refuse writes",
            ports[winner]
        ),
    ))
}

/// The two-node lite contract: kill the primary, `PROMOTE` the replica, no
/// acked commit lost. Returns the promoted node (1) and a report.
pub fn lite_scenario(
    ports: &[u16],
    cfg: &ReplicationConfig,
    control: &mut dyn FnMut(HaAction) -> Result<(), String>,
) -> Result<(usize, String), String> {
    if ports.len() != 2 {
        return Err("the lite replication scenario needs two nodes".to_string());
    }
    let table = format!("ha_{:x}", cfg.seed);
    let deadline = Instant::now() + cfg.timeout;
    wait_role(&addr(ports, 0), true, deadline, "node 1 is not primary")?;
    let (_, base) = role(&addr(ports, 0), cfg.timeout)?;
    wait_role(&addr(ports, 1), false, deadline, "node 2 is not a replica")?;
    wait_lsn(
        &addr(ports, 1),
        base,
        deadline,
        "the replica never reached the initial LSN",
    )?;

    let commit_lsn = seed_rows(ports, cfg, &table)?;
    wait_lsn(
        &addr(ports, 1),
        commit_lsn,
        Instant::now() + cfg.election,
        "the replica missed the acked commit",
    )?;
    read_acked(&addr(ports, 1), cfg, &table)?;
    replica_refuses(&addr(ports, 1), cfg, &table)?;

    control(HaAction::Kill(0))?;
    let mut c = TcpCandidate::connect(addr(ports, 1), cfg.timeout)?;
    expect_ok(c.exec("PROMOTE")?, "PROMOTE")?;
    wait_role(
        &addr(ports, 1),
        true,
        Instant::now() + cfg.election,
        "the promoted replica is not primary",
    )?;
    read_acked(&addr(ports, 1), cfg, &table)?;
    let (_, lsn) = role(&addr(ports, 1), cfg.timeout)?;
    Ok((
        1,
        format!(
            "2 nodes; the acked commit was on the replica; node 1 killed; \
             node {} promoted at lsn {lsn} with all 15 rows",
            ports[1]
        ),
    ))
}

#[cfg(test)]
mod tests {
    //! A tiny three/two-node cluster. The replicas share the mock's store
    //! (instant replication); modes remove one property at a time: stale
    //! replicas, a lost acked commit, no election, and writable replicas.

    use super::*;
    use std::collections::BTreeMap;
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
    }

    #[derive(Default)]
    struct State {
        roles: Vec<String>,
        lsn: i64,
        rows: BTreeMap<i64, String>,
    }

    fn spawn_cluster(n: usize, mode: Mode) -> (Vec<u16>, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State {
            roles: (0..n)
                .map(|i| if i == 0 { "primary" } else { "replica" }.to_string())
                .collect(),
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
                    std::thread::spawn(move || handle(stream, i, state, mode));
                }
            });
        }
        (ports, state)
    }

    fn handle(stream: TcpStream, node: usize, state: Arc<Mutex<State>>, mode: Mode) {
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
            let reply = {
                let mut st = state.lock().unwrap();
                let role = st.roles[node].clone();
                if low == "show role" {
                    format!(
                        "ROWS 2\nname\tvalue\nrole\t{role}\nlsn\t{}\nEND 2\n",
                        st.lsn
                    )
                } else if low == "promote" {
                    st.roles[node] = "primary".to_string();
                    "OK 0\n".to_string()
                } else if low.starts_with("insert") {
                    if role != "primary" && mode != Mode::ReplicaWrites {
                        "ERR readonly replica\n".to_string()
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
                } else if low.starts_with("create")
                    || low.starts_with("begin")
                    || low.starts_with("commit")
                {
                    "OK 0\n".to_string()
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
    fn stale_replicas_are_caught() {
        let (ports, state) = spawn_cluster(3, Mode::StaleReplica);
        let mut ctl = control(state, Mode::StaleReplica, 3, ports.clone());
        let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
        assert!(err.contains("COUNT"), "{err}");
    }

    #[test]
    fn losing_the_acked_commit_is_caught() {
        let (ports, state) = spawn_cluster(3, Mode::LosesAcked);
        let mut ctl = control(state, Mode::LosesAcked, 3, ports.clone());
        let err = scenario(&ports, &cfg(), &mut ctl).unwrap_err();
        assert!(err.contains("COUNT"), "{err}");
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
        let (winner, report) =
            lite_scenario(&ports, &cfg(), &mut ctl).expect("faithful lite cluster");
        assert_eq!(winner, 1);
        assert!(report.contains("promoted at lsn"), "{report}");
    }
}
