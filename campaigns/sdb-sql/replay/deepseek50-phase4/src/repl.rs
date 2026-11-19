// Replication and failover.
//
// Nodes talk a small private protocol over the same SQL listener. A control
// connection starts with a 0x01 byte that no SQL client ever sends, so the
// normal statement path is untouched.
//
//   replica -> primary   "\x01SDBR HELLO\t<myport>\t<my_lsn>"
//   primary -> replica   "LIVE\t<lsn>"            (replica is current)
//                        "SNAP\t<lsn>\t<nrecs>"   followed by nrecs WAL lines
//                        "B\t<lsn>\t<len>"        followed by len bytes of lines
//                        "H\t<lsn>"               heartbeat
//   replica -> primary   "A <lsn>"                durable apply ack
//
// A commit is only acknowledged to its SQL client after at least one replica
// answered with an ack for that LSN. Replicas apply a batch by framing it in
// the WAL and fsyncing before applying it in memory and acking, so an acked
// LSN is on the replica's disk.
//
// Failover: a replica that has heard nothing from its primary for 3.3 s asks
// every other node for its role and LSN and promotes the highest LSN (ties:
// lowest port). A primary that meets a better primary demotes itself, so the
// cluster converges on exactly one writer.

use super::*;
use std::cmp::Reverse;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AO};
use std::sync::mpsc::{self, channel, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// First bytes of every replication control connection.
pub const MAGIC: &str = "\u{1}SDBR";

/// Silence after which a replica considers its primary gone. The contract
/// allows declaring death only after 3 s; this leaves a little margin.
const SILENCE: Duration = Duration::from_millis(3300);

struct RepConn {
    tx: Sender<Vec<u8>>,
    acked: AtomicU64,
}

pub struct Repl {
    port: u16,
    data_dir: PathBuf,
    nodes: Vec<(String, u16)>,
    db: Arc<Mutex<Db>>,
    /// This node currently accepts writes.
    primary: AtomicBool,
    /// Last committed LSN known to this node.
    lsn: AtomicU64,
    /// True when the node participates in a cluster (records WAL for
    /// replication, persists the LSN, waits for acks). False on a lone node.
    active: bool,
    last_contact: Mutex<Option<Instant>>,
    /// Set once a frame from the primary has actually arrived. A replica
    /// that has never heard from its primary (slow startup, wrong alias)
    /// must not crown itself: silence only means death after real contact.
    contacted: AtomicBool,
    primary_addr: Mutex<Option<String>>,
    reps: Mutex<Vec<Arc<RepConn>>>,
    ack: Mutex<u64>,
    ack_cv: Condvar,
    commit_lock: Mutex<()>,
    loops_started: AtomicBool,
}

static REPL: OnceLock<Arc<Repl>> = OnceLock::new();

/// A replica/primary instance built by unit tests.  The real server has one
/// process-wide `REPL`; the tests need several isolated instances, so they
/// construct them directly and drive the same functions.
#[cfg(test)]
pub(crate) fn test_instance(
    port: u16,
    data_dir: PathBuf,
    nodes: Vec<(String, u16)>,
    db: Arc<Mutex<Db>>,
    primary: bool,
    lsn: u64,
    active: bool,
    replicaof: Option<String>,
) -> Arc<Repl> {
    Arc::new(Repl {
        port,
        data_dir,
        nodes,
        db,
        primary: AtomicBool::new(primary),
        lsn: AtomicU64::new(lsn),
        active,
        last_contact: Mutex::new(if primary { None } else { Some(Instant::now()) }),
        contacted: AtomicBool::new(false),
        primary_addr: Mutex::new(replicaof),
        reps: Mutex::new(Vec::new()),
        ack: Mutex::new(lsn),
        ack_cv: Condvar::new(),
        commit_lock: Mutex::new(()),
        loops_started: AtomicBool::new(false),
    })
}

#[cfg(test)]
impl Repl {
    pub(crate) fn test_lsn(&self) -> u64 {
        self.lsn.load(AO::Acquire)
    }
    pub(crate) fn test_is_primary(&self) -> bool {
        self.primary.load(AO::Acquire)
    }
    pub(crate) fn test_addr(&self) -> Option<String> {
        mx(&self.primary_addr).clone()
    }
    pub(crate) fn test_set_contact(&self, v: Option<Instant>) {
        *mx(&self.last_contact) = v;
    }
    pub(crate) fn test_has_contact(&self) -> bool {
        mx(&self.last_contact).is_some()
    }
    pub(crate) fn test_set_contacted(&self, v: bool) {
        self.contacted.store(v, AO::Release);
    }
    pub(crate) fn test_set_primary(&self, v: bool) {
        self.primary.store(v, AO::Release);
    }
    pub(crate) fn test_reps_len(&self) -> usize {
        mx(&self.reps).len()
    }
}

pub(crate) fn mx<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

pub(crate) fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
    )
}

fn connect_to(host: &str, port: u16, ms: u64) -> Option<TcpStream> {
    let addrs: Vec<std::net::SocketAddr> = (host, port).to_socket_addrs().ok()?.collect();
    for a in addrs {
        if let Ok(s) = TcpStream::connect_timeout(&a, Duration::from_millis(ms)) {
            let _ = s.set_nodelay(true);
            return Some(s);
        }
    }
    None
}

pub(crate) fn parse_hostport(s: &str) -> Option<(String, u16)> {
    let (h, p) = s.rsplit_once(':')?;
    Some((h.trim().to_string(), p.trim().parse().ok()?))
}

pub(crate) fn parse_nodes(s: &str) -> Vec<(String, u16)> {
    let mut v = Vec::new();
    for part in s.split(',') {
        if let Some((h, p)) = parse_hostport(part) {
            if !h.is_empty() {
                v.push((h, p));
            }
        }
    }
    v
}

pub(crate) fn lsn_file(dir: &Path) -> PathBuf {
    dir.join("repl.lsn")
}

pub(crate) fn load_lsn(dir: &Path) -> u64 {
    fs::read_to_string(lsn_file(dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

pub(crate) fn store_lsn(dir: &Path, lsn: u64) {
    if let Ok(f) = File::create(lsn_file(dir)) {
        let mut f = f;
        let _ = f.write_all(format!("{}\n", lsn).as_bytes());
        let _ = f.sync_data();
    }
}

pub(crate) fn key(lsn: u64, port: u16) -> (u64, Reverse<u16>) {
    (lsn, Reverse(port))
}

pub fn active() -> bool {
    active_of(REPL.get().map(|a| a.as_ref()))
}

pub(crate) fn active_of(st: Option<&Repl>) -> bool {
    match st {
        Some(s) => s.active,
        None => false,
    }
}

pub fn is_replica() -> bool {
    is_replica_of(REPL.get().map(|a| a.as_ref()))
}

pub(crate) fn is_replica_of(st: Option<&Repl>) -> bool {
    match st {
        Some(s) => !s.primary.load(AO::Acquire),
        None => false,
    }
}

/// `SHOW ROLE`: name/value rows for `role` and `lsn`.
pub fn show_role() -> String {
    show_role_of(REPL.get().map(|a| a.as_ref()))
}

pub(crate) fn show_role_of(st: Option<&Repl>) -> String {
    let (role, lsn) = match st {
        Some(s) => (
            if s.primary.load(AO::Acquire) {
                "primary"
            } else {
                "replica"
            },
            s.lsn.load(AO::Acquire),
        ),
        None => ("primary", 0),
    };
    format!(
        "ROWS 2\nname\tvalue\nrole\t{}\nlsn\t{}\nEND 2\n",
        role, lsn
    )
}

/// A write committed on a lone node: keep the LSN growing, nothing else.
pub fn note_commit() {
    if let Some(s) = REPL.get() {
        s.lsn.fetch_add(1, AO::AcqRel);
    }
}

#[cfg(test)]
pub(crate) fn note_commit_on(st: &Repl) {
    st.lsn.fetch_add(1, AO::AcqRel);
}

// ------------------------------------------------------------- startup ---

pub fn init(port: u16, data_dir: PathBuf, replicaof: Option<String>, db: Arc<Mutex<Db>>) {
    let nodes = env::var("SDB_SQL_NODES")
        .map(|s| parse_nodes(&s))
        .unwrap_or_default();
    let active = nodes.len() > 1 || replicaof.is_some();
    let lsn = if active { load_lsn(&data_dir) } else { 0 };
    let primary = replicaof.is_none();
    let addr = replicaof.clone();
    let st = Arc::new(Repl {
        port,
        data_dir,
        nodes,
        db,
        primary: AtomicBool::new(primary),
        lsn: AtomicU64::new(lsn),
        active,
        last_contact: Mutex::new(if primary { None } else { Some(Instant::now()) }),
        contacted: AtomicBool::new(false),
        primary_addr: Mutex::new(addr),
        reps: Mutex::new(Vec::new()),
        ack: Mutex::new(0),
        ack_cv: Condvar::new(),
        commit_lock: Mutex::new(()),
        loops_started: AtomicBool::new(false),
    });
    if REPL.set(st.clone()).is_err() {
        return;
    }
    if st.loops_started.swap(true, AO::AcqRel) {
        return;
    }
    let s2 = st.clone();
    thread::spawn(move || primary_loop(s2));
    let s5 = st.clone();
    thread::spawn(move || watch_loop(s5));
    let s3 = st.clone();
    thread::spawn(move || syncer(s3));
    let s4 = st.clone();
    thread::spawn(move || election_loop(s4));
}

// -------------------------------------------------------------- commit ---

/// Replicate one committed batch. Blocks until a replica durably applied it
/// (or until no replica is attached at all, so a broken cluster cannot wedge
/// the primary forever).
pub fn commit_batch(recs: Vec<String>) {
    if let Some(st) = REPL.get() {
        commit_batch_on(st, recs);
    }
}

pub(crate) fn commit_batch_on(st: &Arc<Repl>, recs: Vec<String>) {
    if !st.active {
        st.lsn.fetch_add(1, AO::AcqRel);
        return;
    }
    let recs: Vec<String> = recs
        .into_iter()
        .filter(|r| r != "T\tB" && r != "T\tE")
        .collect();
    if recs.is_empty() {
        return;
    }
    let _g = mx(&st.commit_lock);
    if !st.primary.load(AO::Acquire) {
        return;
    }
    let lsn = st.lsn.load(AO::Acquire) + 1;
    // The local batch is already durable in the WAL, so the next LSN is
    // fixed now: persist it before the wait so a restart cannot reuse it.
    store_lsn(&st.data_dir, lsn);

    let mut body = String::new();
    for r in &recs {
        body.push_str(r);
        body.push('\n');
    }
    let mut frame = format!("B\t{}\t{}\n", lsn, body.len()).into_bytes();
    frame.extend_from_slice(body.as_bytes());

    let reps = mx(&st.reps).clone();
    let mut sent = 0usize;
    for r in &reps {
        if r.tx.send(frame.clone()).is_ok() {
            sent += 1;
        }
    }
    if sent > 0 {
        let start = Instant::now();
        let mut acked = mx(&st.ack);
        while *acked < lsn {
            if mx(&st.reps).is_empty() && start.elapsed() > Duration::from_millis(1500) {
                break;
            }
            let (g, _) = match st
                .ack_cv
                .wait_timeout(acked, Duration::from_millis(100))
            {
                Ok(v) => v,
                Err(p) => p.into_inner(),
            };
            acked = g;
        }
    }
    // The write is now committed: publish the LSN (SHOW ROLE, snapshots and
    // elections all read it).
    st.lsn.store(lsn, AO::Release);
}

// --------------------------------------------------- primary listeners ---

pub(crate) fn primary_loop(st: Arc<Repl>) {
    loop {
        thread::sleep(Duration::from_millis(200));
        if !st.primary.load(AO::Acquire) {
            continue;
        }
        let lsn = st.lsn.load(AO::Acquire);
        let hb = format!("H\t{}\n", lsn).into_bytes();
        let reps = mx(&st.reps).clone();
        for r in reps {
            let _ = r.tx.send(hb.clone());
        }
    }
}

/// Slow loop: announcements and peer watch. Kept off the heartbeat thread so
/// a frozen peer (which never answers) cannot delay liveness beats.
pub(crate) fn watch_loop(st: Arc<Repl>) {
    loop {
        thread::sleep(Duration::from_millis(1000));
        if !st.primary.load(AO::Acquire) {
            continue;
        }
        let lsn = st.lsn.load(AO::Acquire);
        // A survivor that was frozen while the election happened may still
        // point at the old primary. Announce ourselves periodically so it
        // finds the new writer as soon as it wakes up.
        announce(&st, lsn);
        // Peer watch: a better primary makes this one stand down.
        for (h, p) in st.nodes.clone() {
            if p == st.port {
                continue;
            }
            if let Some((role, plsn)) = vote_call(&st, &h, p) {
                if role == "primary" && key(plsn, p) > key(lsn, st.port) {
                    demote(&st, &h, p);
                    break;
                }
            }
        }
    }
}

pub(crate) fn announce(st: &Arc<Repl>, lsn: u64) {
    let me = st.port;
    for (h, p) in st.nodes.clone() {
        if p == me {
            continue;
        }
        let msg = format!("{} PROMOTE\t{}\t{}\n", MAGIC, me, lsn);
        thread::spawn(move || {
            if let Some(mut s) = connect_to(&h, p, 400) {
                let _ = s.write_all(msg.as_bytes());
                let _ = s.shutdown(Shutdown::Both);
            }
        });
    }
}

pub(crate) fn demote(st: &Arc<Repl>, host: &str, port: u16) {
    st.primary.store(false, AO::Release);
    *mx(&st.primary_addr) = Some(format!("{}:{}", host, port));
    *mx(&st.last_contact) = Some(Instant::now());
}

pub(crate) fn vote_call(st: &Repl, host: &str, port: u16) -> Option<(&'static str, u64)> {
    let mut s = connect_to(host, port, 250)?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(250)));
    let msg = format!("{} VOTE\t{}\n", MAGIC, st.port);
    s.write_all(msg.as_bytes()).ok()?;
    let mut br = BufReader::new(s);
    let mut line = String::new();
    let mut tries = 0;
    loop {
        match br.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => break,
            Err(e) if is_timeout(&e) => {
                tries += 1;
                if tries >= 2 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let f: Vec<&str> = line.trim().split('\t').collect();
    if f.len() >= 3 && f[0] == "R" {
        let role = if f[1] == "primary" {
            "primary"
        } else {
            "replica"
        };
        let lsn = f[2].parse().ok()?;
        Some((role, lsn))
    } else {
        None
    }
}

/// First line of a control connection (after MAGIC).
pub fn handle_control(sock: &TcpStream, rest: &str) {
    if let Some(st) = REPL.get() {
        handle_control_on(st, sock, rest);
    }
}

pub(crate) fn handle_control_on(st: &Arc<Repl>, sock: &TcpStream, rest: &str) {
    let rest = rest.trim_start_matches([' ', '\t']);
    let mut it = rest.split_whitespace();
    let cmd = it.next().unwrap_or("");
    match cmd {
        "HELLO" => {
            let port: u16 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let lsn: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            if let Ok(s) = sock.try_clone() {
                serve_replica(st, s, port, lsn);
            }
        }
        "VOTE" => {
            let role = if st.primary.load(AO::Acquire) {
                "primary"
            } else {
                "replica"
            };
            let reply = format!(
                "R\t{}\t{}\t{}\n",
                role,
                st.lsn.load(AO::Acquire),
                st.port
            );
            let mut s = sock;
            let _ = s.write_all(reply.as_bytes());
        }
        "PROMOTE" => {
            let port: u16 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let plsn: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            if !st.primary.load(AO::Acquire) && port != 0 && port != st.port {
                if plsn >= st.lsn.load(AO::Acquire) {
                    if let Some((h, _)) = st.nodes.iter().find(|(_, p)| *p == port).cloned() {
                        follow(st, &h, port);
                    }
                } else {
                    // A lower-LSN node claiming the primary: our own round
                    // decides, right away.
                    *mx(&st.last_contact) = None;
                }
            }
        }
        _ => {}
    }
}

/// Snapshot every table as the same records the WAL uses.
pub(crate) fn snapshot_records(db: &Db) -> Vec<String> {
    let mut out = Vec::new();
    for t in db.tables.values() {
        let mut rec = String::new();
        rec.push_str("C\t");
        esc_field(&t.name, &mut rec);
        for c in &t.cols {
            rec.push('\t');
            esc_field(&c.name, &mut rec);
            rec.push(':');
            rec.push_str(c.ty.code());
        }
        let mut meta: Vec<String> = Vec::new();
        enc_schema_meta(&t.name, &t.cols, &t.sch, &mut meta);
        for m in meta {
            rec.push('\t');
            rec.push_str(&m);
        }
        out.push(rec);
        for ri in 0..t.rows.len() {
            let row = t.rows.get(ri);
            let mut rec = String::new();
            rec.push_str("I\t");
            esc_field(&t.name, &mut rec);
            rec.push('\t');
            rec.push_str(&t.cols.len().to_string());
            rec.push('\t');
            rec.push_str("1");
            for v in row.iter() {
                rec.push('\t');
                enc_value(v, &mut rec);
            }
            out.push(rec);
        }
        for d in &t.sch.idx_defs {
            let mut rec = String::new();
            rec.push_str("X\t");
            esc_field(&d.name, &mut rec);
            rec.push('\t');
            esc_field(&t.name, &mut rec);
            rec.push('\t');
            rec.push_str(&d.col.to_string());
            rec.push('\t');
            rec.push_str(if d.unique { "1" } else { "0" });
            out.push(rec);
        }
    }
    out
}

pub(crate) fn serve_replica(st: &Arc<Repl>, sock: TcpStream, _replica_port: u16, replica_lsn: u64) {
    let guard = mx(&st.commit_lock);
    let lsn = st.lsn.load(AO::Acquire);
    let need_snap = replica_lsn != lsn;
    let snapshot = if need_snap {
        let db = mx(&st.db);
        Some(snapshot_records(&db))
    } else {
        None
    };

    let (tx, rx) = channel::<Vec<u8>>();
    let rc = Arc::new(RepConn {
        tx,
        acked: AtomicU64::new(replica_lsn),
    });

    let mut ws = match sock.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let reader_fd = match sock.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    thread::spawn(move || {
        while let Ok(buf) = rx.recv() {
            if ws.write_all(&buf).is_err() {
                break;
            }
        }
        let _ = ws.shutdown(Shutdown::Both);
    });

    let mut pre = String::new();
    match &snapshot {
        Some(recs) => {
            pre.push_str(&format!("SNAP\t{}\t{}\n", lsn, recs.len()));
            for r in recs {
                pre.push_str(r);
                pre.push('\n');
            }
        }
        None => pre.push_str(&format!("LIVE\t{}\n", lsn)),
    }
    {
        let mut s = match sock.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
        if s.write_all(pre.as_bytes()).is_err() {
            return;
        }
    }
    mx(&st.reps).push(rc.clone());
    drop(guard);

    // Acks from this replica until it goes away.
    let mut br = BufReader::new(reader_fd);
    let mut line = String::new();
    loop {
        line.clear();
        match br.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let t = line.trim();
        if let Some(v) = t.strip_prefix("A ") {
            if let Ok(n) = v.trim().parse::<u64>() {
                let _ = rc.acked.fetch_max(n, AO::AcqRel);
                let mut a = mx(&st.ack);
                if n > *a {
                    *a = n;
                    st.ack_cv.notify_all();
                }
            }
        }
    }
    {
        let mut reps = mx(&st.reps);
        reps.retain(|r| !Arc::ptr_eq(r, &rc));
    }
    st.ack_cv.notify_all();
}

// ------------------------------------------------- replica side (syncer) ---

pub(crate) fn follow(st: &Arc<Repl>, host: &str, port: u16) {
    let addr = format!("{}:{}", host, port);
    *mx(&st.primary_addr) = Some(addr);
    *mx(&st.last_contact) = Some(Instant::now());
}

pub(crate) fn syncer(st: Arc<Repl>) {
    loop {
        if st.primary.load(AO::Acquire) {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let addr = mx(&st.primary_addr).clone();
        let (host, port) = match addr.and_then(|a| parse_hostport(&a)) {
            Some(x) => x,
            None => {
                thread::sleep(Duration::from_millis(150));
                continue;
            }
        };
        let sock = match connect_to(&host, port, 500) {
            Some(s) => s,
            None => {
                thread::sleep(Duration::from_millis(150));
                continue;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
        let mut ws = match sock.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let hello = format!(
            "{} HELLO\t{}\t{}\n",
            MAGIC,
            st.port,
            st.lsn.load(AO::Acquire)
        );
        if ws.write_all(hello.as_bytes()).is_err() {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let mut br = BufReader::new(sock);
        let connected_to = format!("{}:{}", host, port);
        loop {
            if st.primary.load(AO::Acquire) {
                let _ = ws.shutdown(Shutdown::Both);
                break;
            }
            // A new primary was announced: stop feeding from the old one.
            if mx(&st.primary_addr).as_deref() != Some(connected_to.as_str()) {
                let _ = ws.shutdown(Shutdown::Both);
                break;
            }
            let mut line = String::new();
            let mut got = false;
            loop {
                match br.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        got = true;
                        break;
                    }
                    Err(e) if is_timeout(&e) => {
                        if st.primary.load(AO::Acquire)
                            || mx(&st.primary_addr).as_deref() != Some(connected_to.as_str())
                        {
                            break;
                        }
                        continue;
                    }
                    Err(_) => break,
                }
            }
            if !got {
                break;
            }
            *mx(&st.last_contact) = Some(Instant::now());
            st.contacted.store(true, AO::Release);
            let text = line.trim_end_matches(['\r', '\n']).to_string();
            let f: Vec<&str> = text.split('\t').collect();
            match f[0] {
                "LIVE" => {
                    let lsn: u64 = f.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    if lsn != st.lsn.load(AO::Acquire) {
                        st.lsn.store(lsn, AO::Release);
                        store_lsn(&st.data_dir, lsn);
                    }
                }
                "H" => {}
                "SNAP" => {
                    let lsn: u64 = f.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let n: usize = f.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let mut recs = Vec::with_capacity(n);
                    let mut bad = false;
                    for _ in 0..n {
                        let mut r = String::new();
                        if !read_line_wait(&mut br, &mut r, &st) {
                            bad = true;
                            break;
                        }
                        *mx(&st.last_contact) = Some(Instant::now());
                        recs.push(r.trim_end_matches(['\r', '\n']).to_string());
                    }
                    if bad {
                        break;
                    }
                    if apply_snapshot(&st, &recs, lsn) {
                        let _ = ws.write_all(format!("A {}\n", lsn).as_bytes());
                    } else {
                        force_resync(&st);
                        break;
                    }
                }
                "B" => {
                    let lsn: u64 = f.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let len: usize = f.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let mut buf = vec![0u8; len];
                    if !read_exact_wait(&mut br, &mut buf, &st) {
                        break;
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let recs: Vec<String> = text
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(|l| l.to_string())
                        .collect();
                    if apply_batch(&st, &recs, lsn) {
                        let _ = ws.write_all(format!("A {}\n", lsn).as_bytes());
                    } else {
                        force_resync(&st);
                        break;
                    }
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(80));
    }
}

pub(crate) fn read_line_wait(br: &mut BufReader<TcpStream>, out: &mut String, st: &Repl) -> bool {
    loop {
        match br.read_line(out) {
            Ok(0) => return false,
            Ok(_) => return true,
            Err(e) if is_timeout(&e) => {
                if st.primary.load(AO::Acquire) {
                    return false;
                }
                continue;
            }
            Err(_) => return false,
        }
    }
}

pub(crate) fn read_exact_wait(br: &mut BufReader<TcpStream>, buf: &mut [u8], st: &Repl) -> bool {
    let mut got = 0usize;
    while got < buf.len() {
        match br.read(&mut buf[got..]) {
            Ok(0) => return false,
            Ok(n) => {
                got += n;
                *mx(&st.last_contact) = Some(Instant::now());
            }
            Err(e) if is_timeout(&e) => {
                if st.primary.load(AO::Acquire) {
                    return false;
                }
                continue;
            }
            Err(_) => return false,
        }
    }
    true
}

pub(crate) fn apply_batch(st: &Repl, recs: &[String], lsn: u64) -> bool {
    {
        let mut db = mx(&st.db);
        if !db.repl_apply(recs) {
            return false;
        }
    }
    st.lsn.store(lsn, AO::Release);
    store_lsn(&st.data_dir, lsn);
    true
}

pub(crate) fn apply_snapshot(st: &Repl, recs: &[String], lsn: u64) -> bool {
    {
        let mut db = mx(&st.db);
        if !db.repl_install(recs) {
            return false;
        }
    }
    st.lsn.store(lsn, AO::Release);
    store_lsn(&st.data_dir, lsn);
    true
}

pub(crate) fn force_resync(st: &Repl) {
    st.lsn.store(0, AO::Release);
    store_lsn(&st.data_dir, 0);
}

// ------------------------------------------------------------ election ---

pub(crate) fn election_loop(st: Arc<Repl>) {
    loop {
        thread::sleep(Duration::from_millis(150));
        if st.primary.load(AO::Acquire) {
            continue;
        }
        let stale = {
            let g = mx(&st.last_contact);
            match *g {
                Some(t) => t.elapsed() > SILENCE,
                None => false,
            }
        };
        if !stale {
            continue;
        }
        elect(&st);
    }
}

/// Ask every peer for its role and LSN, in parallel and with a hard
/// deadline, so one unreachable alias (frozen process, stale DNS) cannot
/// stretch an election past the contract's budget.
pub(crate) fn gather_votes(st: &Arc<Repl>) -> Vec<(u16, &'static str, u64)> {
    let (tx, rx) = mpsc::channel::<(u16, &'static str, u64)>();
    for (h, p) in st.nodes.clone() {
        if p == st.port {
            continue;
        }
        let tx = tx.clone();
        let stc = st.clone();
        thread::spawn(move || {
            if let Some((role, lsn)) = vote_call(&stc, &h, p) {
                let _ = tx.send((p, role, lsn));
            }
        });
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_millis(900);
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok(v) => out.push(v),
            Err(_) => break,
        }
    }
    out
}

pub(crate) fn elect(st: &Arc<Repl>) {
    let my_lsn = st.lsn.load(AO::Acquire);
    let mut best = key(my_lsn, st.port);
    let mut best_addr: Option<(String, u16)> = None;
    let mut best_role = "replica";
    // A reachable primary is alive: never take over from it while our data
    // is not newer. This also closes the race where a replica wakes up from
    // a pause just as the primary hands it the missing batch.
    let mut live_primary: Option<(u16, u64)> = None;
    let votes = gather_votes(st);
    for (p, role, lsn) in votes {
        if role == "primary" && live_primary.is_none() {
            live_primary = Some((p, lsn));
        }
        let k = key(lsn, p);
        if k > best {
            best = k;
            best_addr = st.nodes.iter().find(|(_, q)| *q == p).cloned();
            best_role = role;
        }
    }
    let mut live_primary_addr: Option<(String, u16)> = live_primary.and_then(|(p, _)| {
        st.nodes.iter().find(|(_, q)| *q == p).cloned()
    });
    if let Some((p, lsn)) = live_primary {
        if lsn >= my_lsn {
            if let Some((h, _)) = live_primary_addr.take() {
                follow(st, &h, p);
            }
            return;
        }
    }
    if best == key(my_lsn, st.port) {
        // Contact may have resumed while we were asking around.
        let fresh = mx(&st.last_contact)
            .map(|t| t.elapsed() < Duration::from_millis(800))
            .unwrap_or(false);
        if !fresh && st.contacted.load(AO::Acquire) {
            promote(st);
        }
    } else if best_role == "primary" {
        if let Some((h, p)) = best_addr {
            follow(st, &h, p);
        }
    } else {
        // The better node is still a replica; it will promote itself on its
        // own round. A quiet retry avoids two promotions in the same tick.
        // If it cannot (e.g. it cannot see us), the loop below eventually
        // promotes the survivor that can still serve.
        let waited = mx(&st.last_contact)
            .map(|t| t.elapsed() > SILENCE + Duration::from_millis(700))
            .unwrap_or(false);
        if waited && st.nodes.len() <= 2 {
            promote(st);
        }
    }
}

pub(crate) fn promote(st: &Arc<Repl>) {
    if st.primary.swap(true, AO::AcqRel) {
        return;
    }
    *mx(&st.primary_addr) = None;
    *mx(&st.last_contact) = None;
    let lsn = st.lsn.load(AO::Acquire);
    // A new term: no replica has acknowledged anything of ours yet.
    *mx(&st.ack) = lsn;
    announce(st, lsn);
}

// ------------------------------------------------- Db apply entry points ---

impl Db {
    /// Apply a replicated batch: frame it in the WAL, fsync, then publish it
    /// in memory. The primary only acks the SQL commit after this returns.
    fn repl_apply(&mut self, recs: &[String]) -> bool {
        let wal = match &self.wal {
            Some(w) => w.clone(),
            None => return false,
        };
        let mut last = 0u64;
        if let Some(t) = self.wal_append("T\tB") {
            last = t;
        }
        for r in recs {
            if let Some(t) = self.wal_append_owned(r.clone()) {
                last = t;
            }
        }
        if let Some(t) = self.wal_append("T\tE") {
            last = t;
        }
        self.repl_buf.clear();
        if last > 0 && wal.flush(last).is_err() {
            return false;
        }
        for r in recs {
            if !self.apply_record(r) {
                return false;
            }
        }
        self.data_gen += 1;
        true
    }

    /// Replace the whole store with a snapshot taken on the primary.
    fn repl_install(&mut self, recs: &[String]) -> bool {
        self.txns.clear();
        self.writer = None;
        self.tables.clear();
        self.gen += 1;
        let gen = self.gen;

        let tmp = self.snap_path.with_extension("tmp");
        let ok = (|| -> std::io::Result<()> {
            let f = File::create(&tmp)?;
            let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
            writeln!(w, "G\t{}", gen)?;
            for r in recs {
                w.write_all(r.as_bytes())?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
            let f = w.into_inner().map_err(|e| e.into_error())?;
            f.sync_all()?;
            fs::rename(&tmp, &self.snap_path)?;
            if let Ok(d) = File::open(self.snap_path.parent().unwrap_or(&self.snap_path)) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        if ok.is_err() {
            return false;
        }

        let header = format!("G\t{}\n", gen);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.wal_path)
        {
            Ok(f) => f,
            Err(_) => return false,
        };
        if (&file).write_all(header.as_bytes()).is_err() || file.sync_data().is_err() {
            return false;
        }
        self.wal_bytes = header.len() as u64;
        self.wal = Some(Arc::new(WalShared {
            m: Mutex::new(Wal {
                file: Arc::new(file),
                segs: Vec::new(),
                pos: header.len() as u64,
                cap: 0,
                seq: 0,
                durable: 0,
                syncing: false,
                fail_lo: 1,
                fail_upto: 0,
            }),
            cv: Condvar::new(),
        }));
        for r in recs {
            if !self.apply_record(r) {
                return false;
            }
        }
        self.data_gen += 1;
        self.repl_buf.clear();
        true
    }
}
