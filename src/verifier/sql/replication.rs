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
//! 2. the replicas are caught up, then refuse writes with `ERR readonly`;
//! 3. both replicas are frozen for about a second: `COMMIT` must not answer
//!    until one is resumed (an asynchronous ack answers within the freeze);
//!    both then apply the commit, so their LSNs are equal;
//! 4. the primary is killed; with equal LSNs the lower port must win within
//!    the election budget and hold every acked row;
//! 5. a new committed write reaches the other survivor;
//! 6. the old primary is respawned as a replica, converges, and refuses
//!    writes.
//!
//! The lite variant has two nodes and a manual `PROMOTE`: the replica is
//! frozen across the COMMIT, resumed, then the primary is killed and the
//! replica promoted.

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
    /// Freeze the node (`docker pause`) for the sync-commit probe.
    Pause(usize),
    /// Thaw the node again.
    Resume(usize),
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

/// Create the table and the ten acked seed rows, outside a transaction.
/// The replicas are caught up before the read-only check, so a replica that
/// resolves the table before checking its role still answers `readonly`
/// (round-17 J2). Returns the base LSN.
fn seed_base(ports: &[u16], cfg: &ReplicationConfig, table: &str) -> Result<i64, String> {
    let mut c = TcpCandidate::connect(addr(ports, 0), cfg.timeout)?;
    expect_ok(
        c.exec(&format!("CREATE TABLE {table} (id INTEGER, v TEXT)"))?,
        "CREATE TABLE",
    )?;
    for i in 0..10 {
        expect_ok(
            c.exec(&format!("INSERT INTO {table} VALUES ({i}, 'v{i}')"))?,
            &format!("seed insert {i}"),
        )?;
    }
    let (_, last) = role(&addr(ports, 0), cfg.timeout)?;
    Ok(last)
}

/// The frozen sync-commit probe (round-17 J1). Every replica is paused
/// before the transaction is sent, so an asynchronous ack answers within a
/// second while a synchronous one waits. The pending `COMMIT` must answer
/// `OK` within five seconds of resuming `resume`.
fn seed_commit(
    primary: &str,
    cfg: &ReplicationConfig,
    table: &str,
    resume: usize,
    control: &mut dyn FnMut(HaAction) -> Result<(), String>,
) -> Result<i64, String> {
    let mut c = TcpCandidate::connect(primary, cfg.timeout)?;
    expect_ok(c.exec("BEGIN")?, "frozen BEGIN")?;
    for i in 10..15 {
        expect_ok(
            c.exec(&format!("INSERT INTO {table} VALUES ({i}, 'v{i}')"))?,
            &format!("transaction insert {i}"),
        )?;
    }
    c.send("COMMIT")?;
    c.set_read_timeout(Duration::from_secs(1))?;
    let started = Instant::now();
    match c.recv() {
        Ok(resp) => {
            return Err(format!(
                "COMMIT answered within one second while every replica was frozen: {resp:?}"
            ))
        }
        Err(e) if started.elapsed() < Duration::from_millis(900) => {
            return Err(format!(
                "COMMIT failed before the one-second freeze deadline: {e}"
            ))
        }
        Err(_) => {}
    }
    control(HaAction::Resume(resume))?;
    c.set_read_timeout(Duration::from_secs(5))?;
    let resp = c
        .recv()
        .map_err(|e| format!("COMMIT after resuming replica {}: {e}", resume + 1))?;
    expect_ok(resp, "COMMIT after resume")?;
    let (_, last) = role(primary, cfg.timeout)?;
    Ok(last)
}

fn read_rows(addr: &str, cfg: &ReplicationConfig, table: &str) -> Result<Vec<Vec<String>>, String> {
    let mut c = TcpCandidate::connect(addr, cfg.timeout)?;
    expect_rows(
        c.exec(&format!("SELECT id, v FROM {table}"))?,
        "replica rows",
    )
}

fn read_acked(addr: &str, cfg: &ReplicationConfig, table: &str) -> Result<(), String> {
    let rows = read_rows(addr, cfg, table)?;
    if rows.len() != 15 {
        return Err(format!("replica has {} rows, expected 15", rows.len()));
    }
    let v = rows
        .iter()
        .find(|r| r.first().map(String::as_str) == Some("14"))
        .and_then(|r| r.get(1));
    if v != Some(&"v14".to_string()) {
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
    let rows = read_rows(addr, cfg, table)?;
    let v = rows
        .iter()
        .find(|r| r.first().map(String::as_str) == Some("100"))
        .and_then(|r| r.get(1));
    if v != Some(&"new".to_string()) {
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

    // Create the table and seed rows, catch every replica up, and only then
    // check the read-only refusal: an engine that resolves the table before
    // its role must still see the table (round-17 J2).
    let base = seed_base(ports, cfg, &table)?;
    let seed_deadline = Instant::now() + cfg.timeout;
    for i in [1usize, 2usize] {
        wait_lsn(
            &addr(ports, i),
            base,
            seed_deadline,
            "replica missed the seed rows",
        )?;
    }
    for i in [1usize, 2usize] {
        replica_refuses(&addr(ports, i), cfg, &table)?;
    }

    // Freeze both replicas, then commit. A synchronous engine cannot answer
    // within one second; an asynchronous one does, which fails here.
    for i in [1usize, 2usize] {
        control(HaAction::Pause(i))?;
    }
    let commit_lsn = seed_commit(&addr(ports, 0), cfg, &table, 1, control)?;
    control(HaAction::Resume(2))?;

    // Both replicas now apply the commit; with equal LSNs the lower port
    // must win, deterministically (round-17 J1).
    let commit_deadline = Instant::now() + cfg.election;
    for i in [1usize, 2usize] {
        wait_lsn(
            &addr(ports, i),
            commit_lsn,
            commit_deadline,
            "replica did not receive the frozen commit",
        )?;
    }
    let (_, l1) = role(&addr(ports, 1), cfg.timeout)?;
    let (_, l2) = role(&addr(ports, 2), cfg.timeout)?;
    if l1 != l2 {
        return Err(format!(
            "the replicas did not converge after the frozen commit: lsn {l1} vs {l2}"
        ));
    }
    let expected = if ports[1] < ports[2] { 1 } else { 2 };

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
            "elected node {} (index {winner}), expected the lower-port survivor index {expected} after both replicas applied the frozen commit",
            ports[winner]
        ));
    }
    let (_, elected_lsn) = role(&addr(ports, winner), cfg.timeout)?;
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
            "3 nodes; the replicas were frozen across the COMMIT (the ack \
             waited for a resume); node 1 killed; node {} elected at lsn \
             {elected_lsn} with every acked row; the new write committed at \
             lsn {new_lsn}; the survivor and the rejoined old primary \
             converged and refuse writes",
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

    // Create the table, catch the replica up, then check the refusal
    // (round-17 J2), freeze it, and commit.
    let base = seed_base(ports, cfg, &table)?;
    wait_lsn(
        &addr(ports, 1),
        base,
        Instant::now() + cfg.timeout,
        "the replica missed the seed rows",
    )?;
    replica_refuses(&addr(ports, 1), cfg, &table)?;
    control(HaAction::Pause(1))?;
    let commit_lsn = seed_commit(&addr(ports, 0), cfg, &table, 1, control)?;
    // `seed_commit` resumes the replica; the ack means it holds the commit.
    let (_, lsn) = role(&addr(ports, 1), cfg.timeout)?;
    if lsn < commit_lsn {
        return Err(format!(
            "the promoted replica is at lsn {lsn}, behind the acked COMMIT lsn {commit_lsn}: the ack was not synchronous"
        ));
    }
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
    Ok((
        1,
        format!(
            "2 nodes; the replica was frozen across the COMMIT and acked \
             it after the resume; node 1 killed; node {} promoted at lsn \
             {lsn} with all 15 rows",
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
    }

    #[derive(Default)]
    struct State {
        roles: Vec<String>,
        lsn: i64,
        rows: BTreeMap<i64, String>,
        paused: Vec<bool>,
        tables: BTreeSet<String>,
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
                if writer.write_all(reply.as_bytes()).is_err() {
                    break;
                }
                continue;
            }
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
        let (winner, _) =
            lite_scenario(&ports, &cfg(), &mut ctl).expect("table-first lite replica");
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
        let (winner, report) =
            lite_scenario(&ports, &cfg(), &mut ctl).expect("faithful lite cluster");
        assert_eq!(winner, 1);
        assert!(report.contains("promoted at lsn"), "{report}");
    }
}
