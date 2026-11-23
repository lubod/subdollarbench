// End-to-end gate for `./test.sh`.
//
// This deliberately talks to the real server over TCP and only uses the
// text protocol: it must keep working (and keep failing when behaviour
// regresses) even in a scratch copy where only `src/main.rs` was rolled
// back to an older revision and the in-tree unit tests therefore no
// longer exist.  Nothing here imports the crate.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "sdb-e2e-{}-{}-{}",
        std::process::id(),
        tag,
        n
    ));
    let _ = fs::remove_dir_all(&p);
    p
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

struct Server {
    child: Child,
    dir: PathBuf,
    addr: u16,
    keep_dir: bool,
}

impl Server {
    /// Start the server on a free port, retrying on the (rare) chance that
    /// another process grabs the port between probe and bind.
    fn launch(dir: &Path) -> Server {
        let mut last = String::new();
        for _ in 0..5 {
            let port = free_port();
            match Server::try_start(dir, port) {
                Ok(s) => return s,
                Err(e) => last = e,
            }
        }
        panic!("could not start sdb: {last}");
    }

    fn try_start(dir: &Path, addr: u16) -> Result<Server, String> {
        let bin = env!("CARGO_BIN_EXE_sdb");
        let profile = dir.join("child.profraw");
        if !dir.exists() {
            let _ = fs::create_dir_all(dir);
        }
        let child = Command::new(bin)
            .arg("--port")
            .arg(addr.to_string())
            .arg("--data")
            .arg(dir)
            // Keep a coverage profile written by this child away from the
            // one the harness collects for the test process itself.
            .env("LLVM_PROFILE_FILE", profile)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn: {e}"))?;
        let mut child = child;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match TcpStream::connect(("127.0.0.1", addr)) {
                Ok(s) => {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                    break;
                }
                Err(_) => {
                    if let Ok(Some(st)) = child.try_wait() {
                        return Err(format!("server exited early: {st}"));
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("server did not accept connections in time".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        Ok(Server { child, dir: dir.to_path_buf(), addr, keep_dir: false })
    }

    fn conn(&self) -> Conn {
        Conn::new(self.addr)
    }

    /// Stop the process but keep the store for a restart test.
    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.keep_dir = true;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !self.keep_dir {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

struct Conn {
    out: TcpStream,
    inp: BufReader<TcpStream>,
}

impl Conn {
    fn new(addr: u16) -> Conn {
        let s = TcpStream::connect(("127.0.0.1", addr)).expect("connect");
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(Duration::from_secs(20))).ok();
        let inp = BufReader::new(s.try_clone().unwrap());
        Conn { out: s, inp }
    }

    fn line(&mut self) -> String {
        let mut l = String::new();
        let n = self.inp.read_line(&mut l).expect("read response line");
        assert!(n > 0, "server closed the connection");
        l.trim_end_matches(['\n', '\r']).to_string()
    }

    /// Send one statement and return its full answer (one or more lines).
    fn exec(&mut self, sql: &str) -> Vec<String> {
        self.out.write_all(sql.as_bytes()).expect("write sql");
        self.out.write_all(b"\n").expect("write newline");
        self.out.flush().expect("flush");
        let first = self.line();
        let mut lines = vec![first.clone()];
        if first.starts_with("ROWS ") {
            let ncols: usize = first[5..].trim().parse().expect("ROWS ncols");
            let header = self.line();
            assert_eq!(header.split('\t').count(), ncols, "header: {header:?}");
            lines.push(header);
            loop {
                let l = self.line();
                lines.push(l.clone());
                if l.starts_with("END ") {
                    let n = lines.len();
                    assert!(n >= 2, "bad frame");
                    break;
                }
            }
            let nrows: usize = lines
                .last()
                .unwrap()
                .strip_prefix("END ")
                .unwrap()
                .trim()
                .parse()
                .expect("END nrows");
            assert_eq!(lines.len(), nrows + 3, "ROWS frame shape: {lines:?}");
        }
        lines
    }

    fn ok(&mut self, sql: &str, n: u64) {
        let r = self.exec(sql);
        assert_eq!(r, vec![format!("OK {}", n)], "sql: {sql}");
    }

    fn err(&mut self, sql: &str, class: &str) {
        let r = self.exec(sql);
        assert_eq!(r.len(), 1, "sql: {sql} -> {r:?}");
        assert!(
            r[0].starts_with(&format!("ERR {} ", class)),
            "sql: {sql} -> {r:?}"
        );
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        let r = self.exec(sql);
        assert!(r[0].starts_with("ROWS "), "sql: {sql} -> {r:?}");
        assert!(r.len() >= 3, "sql: {sql} -> {r:?}");
        let end = r.last().unwrap();
        assert!(end.starts_with("END "), "missing END: {r:?}");
        r[2..r.len() - 1]
            .iter()
            .map(|l| l.split('\t').map(|s| s.to_string()).collect())
            .collect()
    }

    fn flat(&mut self, sql: &str) -> Vec<String> {
        self.rows(sql)
            .into_iter()
            .map(|r| r.join("|"))
            .collect()
    }
}

#[test]
fn end_to_end_gate() {
    let dir = temp_dir("gate");
    let server = Server::launch(&dir);
    let mut c = server.conn();

    // ---- core engine -------------------------------------------------
    c.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT UNIQUE, n INTEGER NOT NULL DEFAULT 0)", 0);
    c.ok("INSERT INTO t (s) VALUES ('a'), ('b')", 2);
    c.ok("INSERT INTO t (id, s, n) VALUES (7, 'c', 9)", 1);
    assert_eq!(
        c.flat("SELECT id, s, n FROM t ORDER BY id"),
        vec!["1|a|0", "2|b|0", "7|c|9"]
    );
    c.err("INSERT INTO t (s) VALUES ('a')", "constraint");
    c.err("SELECT * FROM nope", "no_table");
    c.err("SELECT * FROM t WHERE COUNT(*) > 1", "syntax");

    // ---- ticket 7: aggregates ---------------------------------------
    c.ok("CREATE TABLE g (grp TEXT, n INTEGER)", 0);
    c.ok(
        "INSERT INTO g VALUES ('a', 1), ('a', 2), ('b', 3), ('b', NULL), (NULL, 5)",
        5,
    );
    // Grouped aggregate over a key expression, with HAVING.  A build
    // without aggregates answers `ERR syntax` here and the gate fails.
    assert_eq!(
        c.flat("SELECT grp, COUNT(*), COUNT(n), SUM(n), MIN(n), MAX(n) FROM g GROUP BY grp ORDER BY grp"),
        vec!["\\N|1|1|5|5|5", "a|2|2|3|1|2", "b|2|1|3|3|3"]
    );
    assert_eq!(
        c.flat("SELECT COUNT(*), SUM(n), MIN(n), MAX(n) FROM g WHERE n > 99"),
        vec!["0|\\N|\\N|\\N"]
    );
    assert_eq!(
        c.flat("SELECT grp, SUM(n) AS s FROM g GROUP BY grp HAVING SUM(n) > 3 ORDER BY s"),
        vec!["\\N|5"]
    );
    assert_eq!(
        c.flat("SELECT upper(grp) AS u, COUNT(*) FROM g GROUP BY upper(grp) ORDER BY u"),
        vec!["\\N|1", "A|2", "B|2"]
    );
    c.err("SELECT SUM(zz) FROM g", "no_column");
    c.err("SELECT COUNT(*) FROM g GROUP BY COUNT(*)", "syntax");

    // ---- ticket 8: joins --------------------------------------------
    c.ok("CREATE TABLE a (id INTEGER, v TEXT)", 0);
    c.ok("CREATE TABLE b (id INTEGER, aid INTEGER, w TEXT)", 0);
    c.ok("CREATE TABLE c3 (bid INTEGER, z INTEGER)", 0);
    c.ok("INSERT INTO a VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')", 3);
    c.ok("INSERT INTO b VALUES (10, 1, 'b1'), (11, 1, 'b2'), (12, 2, 'b3')", 3);
    c.ok("INSERT INTO c3 VALUES (10, 100), (12, 300)", 2);
    // Comma join with a qualified predicate.
    assert_eq!(
        c.flat("SELECT a.id, b.w FROM a, b WHERE a.id = b.aid ORDER BY b.id"),
        vec!["1|b1", "1|b2", "2|b3"]
    );
    // INNER JOIN ... ON
    assert_eq!(
        c.flat("SELECT a.id, b.w FROM a INNER JOIN b ON a.id = b.aid ORDER BY b.id"),
        vec!["1|b1", "1|b2", "2|b3"]
    );
    // LEFT JOIN keeps unmatched left rows, with NULLs on the right.
    assert_eq!(
        c.flat("SELECT a.id, b.id FROM a LEFT JOIN b ON a.id = b.aid ORDER BY a.id, b.id"),
        vec!["1|10", "1|11", "2|12", "3|\\N"]
    );
    // Three-way left join.
    assert_eq!(
        c.flat("SELECT a.id, b.w, c3.z FROM a LEFT JOIN b ON a.id = b.aid LEFT JOIN c3 ON b.id = c3.bid ORDER BY a.id, c3.z"),
        vec!["1|b2|\\N", "1|b1|100", "2|b3|300", "3|\\N|\\N"]
    );
    // SELECT * over a join lists every column in FROM order.
    assert_eq!(
        c.rows("SELECT * FROM a, b WHERE a.id = b.aid AND b.id = 10"),
        vec![vec!["1", "a1", "10", "1", "b1"]]
    );
    // Self join with aliases; ambiguous bare names are refused.
    assert_eq!(
        c.flat("SELECT x.id, y.id FROM a x, a y WHERE x.id < y.id ORDER BY x.id, y.id"),
        vec!["1|2", "1|3", "2|3"]
    );
    c.err("SELECT id FROM a, b", "ambiguous");
    c.err("SELECT zz.id FROM a", "no_column");
    c.err("SELECT * FROM a JOIN nope ON 1 = 1", "no_table");
    // Aggregate over a join.
    assert_eq!(
        c.flat("SELECT a.v, COUNT(*) FROM a, b WHERE a.id = b.aid GROUP BY a.v ORDER BY a.v"),
        vec!["a1|2", "a2|1"]
    );

    // ---- later tickets still hold -----------------------------------
    c.ok("CREATE TABLE child (id INTEGER, pid INTEGER REFERENCES t(id))", 0);
    c.err("INSERT INTO child VALUES (1, 99)", "constraint");
    c.ok("CREATE INDEX a_id ON a (id)", 0);
    let plan = c.rows("EXPLAIN SELECT * FROM a WHERE id = 1");
    assert!(
        plan.iter().any(|r| r[0].contains("INDEX a.a_id")),
        "EXPLAIN plan: {plan:?}"
    );
    assert_eq!(
        c.flat("SELECT id FROM a WHERE id IN (SELECT aid FROM b) ORDER BY id"),
        vec!["1", "2"]
    );
    c.ok("BEGIN", 0);
    c.ok("INSERT INTO a VALUES (4, 'a4')", 1);
    c.ok("ROLLBACK", 0);
    assert_eq!(c.flat("SELECT COUNT(*) FROM a"), vec!["3"]);
    let stats = c.rows("SHOW STATS");
    assert_eq!(stats.len(), 10, "SHOW STATS: {stats:?}");
    assert!(stats.iter().any(|r| r[0] == "statements"));
    assert!(stats.iter().all(|r| r.len() == 2 && r[1].parse::<u64>().is_ok()));

    // ---- pipelined statements come back in order --------------------
    c.out
        .write_all(b"SELECT 1\nSELECT 2\nBAD\n")
        .expect("pipeline");
    c.out.flush().unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(c.line());
        if got.last().unwrap().starts_with("ROWS ") {
            loop {
                let l = c.line();
                let end = l.starts_with("END ");
                got.push(l);
                if end {
                    break;
                }
            }
        }
    }
    assert_eq!(got[0], "ROWS 1");
    assert_eq!(got[2], "1");
    assert_eq!(got[3], "END 1");
    assert_eq!(got[4], "ROWS 1");
    assert_eq!(got[6], "2");
    assert!(got[8].starts_with("ERR syntax"), "{got:?}");

    // ---- persistence across a restart -------------------------------
    drop(c);
    server.stop();
    let server = Server::launch(&dir);
    let mut c = server.conn();
    assert_eq!(
        c.flat("SELECT id, s, n FROM t ORDER BY id"),
        vec!["1|a|0", "2|b|0", "7|c|9"]
    );
    c.err("INSERT INTO t (s) VALUES ('a')", "constraint");
    assert_eq!(
        c.flat("SELECT COUNT(*) FROM g"),
        vec!["5"],
        "aggregate table must survive restart"
    );
    assert_eq!(
        c.flat("SELECT a.id, b.w FROM a, b WHERE a.id = b.aid ORDER BY b.id"),
        vec!["1|b1", "1|b2", "2|b3"],
        "join tables must survive restart"
    );

    // Garbage / an unterminated line must not kill the connection.
    c.out.write_all(b"\x00\x01not sql\n").unwrap();
    c.out.flush().unwrap();
    let l = c.line();
    assert!(l.starts_with("ERR "), "garbage answer: {l}");
    c.out.write_all(b"SELECT COUNT(*) FROM t\n").unwrap();
    c.out.flush().unwrap();
    let mut got = vec![c.line()];
    while !got.last().unwrap().starts_with("END ") {
        got.push(c.line());
    }
    assert_eq!(got[0], "ROWS 1", "connection unusable after garbage: {got:?}");
    assert_eq!(got[2], "3");
}
