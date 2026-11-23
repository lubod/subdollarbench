use super::*;
use std::io::Read;
use std::net::ToSocketAddrs;
use std::os::unix::fs::FileExt;
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;
use std::time::SystemTime;

pub struct Cfg {
    pub id: u64,
    pub port: u16,
    pub log_path: String,
    pub nodes: Vec<String>,
    pub cluster: bool,
}

pub struct ReplState {
    committed: u64,
    max_ack: u64,
    upstream: String,
}

pub struct Repl {
    st: Mutex<ReplState>,
    cv: Condvar,
    primary: AtomicBool,
}

pub static CFG: OnceLock<Cfg> = OnceLock::new();

static REPL: Repl = Repl {
    st: Mutex::new(ReplState {
        committed: 0,
        max_ack: 0,
        upstream: String::new(),
    }),
    cv: Condvar::new(),
    primary: AtomicBool::new(true),
};

fn lk() -> MutexGuard<'static, ReplState> {
    REPL.st.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn is_primary() -> bool {
    REPL.primary.load(AtomOrd::SeqCst)
}

pub fn publish(len: u64) {
    lk().committed = len;
    REPL.cv.notify_all();
}

pub fn await_sync(end: u64) {
    if !CFG.get().map_or(false, |c| c.cluster) {
        return;
    }
    let mut st = lk();
    while st.max_ack < end {
        st = REPL.cv.wait(st).unwrap_or_else(|e| e.into_inner());
    }
}

fn role_name() -> &'static str {
    if is_primary() {
        "primary"
    } else {
        "replica"
    }
}

pub fn state_line() -> String {
    let lsn = lk().committed;
    let id = CFG.get().map_or(0, |c| c.id);
    format!("STATE {} {} {}\n", role_name(), lsn, id)
}

pub fn show_role() -> String {
    let lsn = lk().committed;
    format!("ROWS 2\nname\tvalue\nrole\t{}\nlsn\t{}\nEND 2\n", role_name(), lsn)
}

pub fn replay_all(tables: &mut BTreeMap<String, Table>, data: &[u8]) -> usize {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    while let Some(nl) = data[pos..].iter().position(|&b| b == b'\n') {
        spans.push((pos, pos + nl));
        pos += nl + 1;
    }
    let mut valid = 0usize;
    let mut i = 0;
    while i < spans.len() {
        let (s, e) = spans[i];
        let head = String::from_utf8_lossy(&data[s..e]).into_owned();
        match head.strip_prefix("G\t").and_then(|n| n.parse::<usize>().ok()) {
            Some(n) => {
                if i + n >= spans.len() {
                    break;
                }
                for &(s2, e2) in &spans[i + 1..=i + n] {
                    replay(tables, &String::from_utf8_lossy(&data[s2..e2]));
                }
                i += n + 1;
            }
            None => {
                replay(tables, &head);
                i += 1;
            }
        }
        valid = spans[i - 1].1 + 1;
    }
    valid
}

pub fn start(port: u16, data: &str, replicaof: Option<String>, db: Arc<Shared>) {
    let nodes: Vec<String> = env::var("SDB_SQL_NODES")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let cluster = !nodes.is_empty() || replicaof.is_some();
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let id = nanos ^ ((process::id() as u64) << 40);
    let len = lock(&db).log.len;
    let _ = CFG.set(Cfg {
        id,
        port,
        log_path: format!("{}/sdb.log", data.trim_end_matches('/')),
        nodes,
        cluster,
    });
    {
        let mut st = lk();
        st.committed = len;
        st.upstream = replicaof.clone().unwrap_or_default();
    }
    REPL.primary.store(replicaof.is_none(), AtomOrd::SeqCst);
    if cluster {
        thread::spawn(move || follow(db));
    }
}

pub fn serve_replica(mut w: TcpStream, r: BufReader<TcpStream>, from: u64) {
    let Some(cfg) = CFG.get() else {
        return;
    };
    if !is_primary() {
        let _ = w.write_all(b"NOTPRIMARY\n");
        return;
    }
    let Ok(f) = File::open(&cfg.log_path) else {
        return;
    };
    thread::spawn(move || ack_loop(r));
    let committed0 = lk().committed;
    if from > committed0 {
        let _ = w.write_all(b"RESET\n");
        return;
    }
    let mut pos = from;
    loop {
        let (committed, timed_out) = {
            let st = lk();
            if st.committed > pos {
                (st.committed, false)
            } else {
                let (st, t) = REPL
                    .cv
                    .wait_timeout(st, Duration::from_millis(500))
                    .unwrap_or_else(|e| e.into_inner());
                (st.committed, t.timed_out())
            }
        };
        let ok = if committed > pos {
            let mut buf = vec![0u8; (committed - pos) as usize];
            if f.read_exact_at(&mut buf, pos).is_err() {
                return;
            }
            pos = committed;
            let head = format!("D {}\n", buf.len());
            let sent = w.write_all(head.as_bytes());
            sent.is_ok() && w.write_all(&buf).is_ok()
        } else if timed_out {
            w.write_all(b"H\n").is_ok()
        } else {
            true
        };
        if !ok {
            return;
        }
    }
}

fn ack_loop(mut r: BufReader<TcpStream>) {
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if let Some(n) = line.trim().strip_prefix("A ").and_then(|v| v.parse::<u64>().ok()) {
            let mut st = lk();
            if n > st.max_ack {
                st.max_ack = n;
            }
            drop(st);
            REPL.cv.notify_all();
        }
    }
}

pub(crate) fn query(addr: &str) -> Option<(bool, u64, u64)> {
    let sa = addr.to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    s.write_all(b"STATE\n").ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() != 4 || f[0] != "STATE" {
        return None;
    }
    Some((f[1] == "primary", f[2].parse().ok()?, f[3].parse().ok()?))
}

fn port_of(addr: &str) -> u16 {
    addr.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(0)
}

pub(crate) fn reset_local(shared: &Shared) {
    {
        let mut g = lock(shared);
        let _ = g.log.file.set_len(0);
        let _ = g.log.file.sync_all();
        g.log.len = 0;
        g.log.dirty = false;
        g.tables.clear();
    }
    publish(0);
}

pub(crate) fn apply(shared: &Shared, body: &[u8]) -> Result<u64, ()> {
    let len = {
        let mut g = lock(shared);
        let db = &mut *g;
        db.log.write_raw(body).map_err(|_| ())?;
        replay_all(&mut db.tables, body);
        for t in db.tables.values_mut() {
            reindex(t);
        }
        db.log.len
    };
    publish(len);
    Ok(len)
}

pub(crate) fn session(shared: &Arc<Shared>, up: &str, heard: &mut Instant, contacted: &mut bool) {
    let Some(sa) = up.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return;
    };
    let Ok(mut s) = TcpStream::connect_timeout(&sa, Duration::from_millis(300)) else {
        return;
    };
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
    let Ok(mut w) = s.try_clone() else {
        return;
    };
    let from = lk().committed;
    if w.write_all(format!("REPLICATE {}\n", from).as_bytes()).is_err() {
        return;
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 1 << 16];
    loop {
        loop {
            let Some(nl) = buf.iter().position(|&b| b == b'\n') else {
                break;
            };
            let head = String::from_utf8_lossy(&buf[..nl]).into_owned();
            if head == "H" {
                buf.drain(..=nl);
                *heard = Instant::now();
                *contacted = true;
            } else if head == "NOTPRIMARY" {
                *contacted = true;
                return;
            } else if head == "RESET" {
                reset_local(shared);
                return;
            } else if let Some(n) = head.strip_prefix("D ").and_then(|v| v.parse::<usize>().ok()) {
                if buf.len() < nl + 1 + n {
                    break;
                }
                let body = buf[nl + 1..nl + 1 + n].to_vec();
                buf.drain(..nl + 1 + n);
                *heard = Instant::now();
                *contacted = true;
                let Ok(len) = apply(shared, &body) else {
                    return;
                };
                if w.write_all(format!("A {}\n", len).as_bytes()).is_err() {
                    return;
                }
            } else {
                return;
            }
            if is_primary() {
                return;
            }
        }
        match s.read(&mut tmp) {
            Ok(0) => return,
            Ok(k) => buf.extend_from_slice(&tmp[..k]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {
                if heard.elapsed() >= Duration::from_secs(3) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

pub(crate) fn elect(heard: &mut Instant) {
    let Some(cfg) = CFG.get() else {
        return;
    };
    let mut best_lsn = lk().committed;
    let mut best_port = cfg.port;
    let mut best_is_me = true;
    for node in &cfg.nodes {
        let Some((primary, lsn, id)) = query(node) else {
            continue;
        };
        if id == cfg.id {
            continue;
        }
        if primary {
            lk().upstream = node.clone();
            *heard = Instant::now();
            return;
        }
        let port = port_of(node);
        if lsn > best_lsn || (lsn == best_lsn && port < best_port) {
            best_lsn = lsn;
            best_port = port;
            best_is_me = false;
        }
    }
    if best_is_me {
        let mut st = lk();
        st.upstream.clear();
        st.max_ack = 0;
        REPL.primary.store(true, AtomOrd::SeqCst);
        drop(st);
        REPL.cv.notify_all();
    } else {
        thread::sleep(Duration::from_millis(300));
    }
}

fn follow(shared: Arc<Shared>) {
    let mut heard = Instant::now();
    let mut contacted = false;
    loop {
        if is_primary() {
            heard = Instant::now();
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let up = lk().upstream.clone();
        session(&shared, &up, &mut heard, &mut contacted);
        if contacted && heard.elapsed() >= Duration::from_secs(3) && !is_primary() {
            elect(&mut heard);
        }
        thread::sleep(Duration::from_millis(100));
    }
}
