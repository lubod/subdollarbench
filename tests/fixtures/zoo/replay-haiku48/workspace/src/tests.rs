use super::*;

use std::fs;
use std::io::Read;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

fn scratch(name: &str) -> String {
    let dir = env::temp_dir().join(format!("sdb-unit-{}-{}", process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

fn open(dir: &str) -> Arc<Shared> {
    Arc::new(Shared {
        db: Mutex::new(load(dir).expect("open store")),
        busy: Mutex::new(false),
        free: Condvar::new(),
        ver: AtomicU64::new(0),
    })
}

fn session(shared: &Arc<Shared>) -> Session {
    Session {
        shared: Arc::clone(shared),
        txn: None,
    }
}

/// Keeps only the class token of every `ERR` line, so golden files compare
/// answers and error classes, never message text.
fn normalize(transcript: &str) -> String {
    let mut out = String::new();
    let mut in_rows = false;
    for line in transcript.lines() {
        let kept = if in_rows {
            in_rows = !line.starts_with("END ");
            line.to_string()
        } else if line.starts_with("ROWS ") {
            in_rows = true;
            line.to_string()
        } else if line.starts_with("ERR ") {
            line.splitn(3, ' ').take(2).collect::<Vec<_>>().join(" ")
        } else {
            line.to_string()
        };
        out.push_str(&kept);
        out.push('\n');
    }
    out
}

/// Replays a golden script in-process. A `.restart` line reopens the store
/// from disk, which is how persistence is exercised.
fn play(script: &str, dir: &str) -> String {
    let mut shared = open(dir);
    let mut sess = session(&shared);
    let mut out = String::new();
    for line in script.lines().filter(|l| !l.trim().is_empty()) {
        if line == ".restart" {
            drop(sess);
            drop(shared);
            shared = open(dir);
            sess = session(&shared);
            continue;
        }
        out.push_str(&run(&mut sess, line));
    }
    normalize(&out)
}

fn first_diff(got: &str, want: &str) -> Option<String> {
    let mut g = got.lines();
    let mut w = want.lines();
    let mut n = 0;
    loop {
        n += 1;
        match (g.next(), w.next()) {
            (None, None) => return None,
            (a, b) if a == b => continue,
            (a, b) => return Some(format!("line {}: got {:?}, want {:?}", n, a, b)),
        }
    }
}

fn golden_scripts() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(GOLDEN)
        .expect("golden directory")
        .map(|e| e.expect("golden entry").path())
        .filter(|p| p.extension().map_or(false, |x| x == "sql"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn golden_scripts_match_reference_answers() {
    let mut failures = Vec::new();
    let scripts = golden_scripts();
    assert!(!scripts.is_empty(), "no golden scripts found");
    for path in scripts {
        let script = fs::read_to_string(&path).expect("read script");
        let want = fs::read_to_string(path.with_extension("out")).expect("read expectation");
        let dir = scratch(&path.file_stem().unwrap().to_string_lossy());
        let got = play(&script, &dir);
        let _ = fs::remove_dir_all(&dir);
        if let Some(d) = first_diff(&got, &want) {
            failures.push(format!("{}: {}", path.display(), d));
        }
    }
    assert!(failures.is_empty(), "golden mismatches:\n{}", failures.join("\n"));
}

#[test]
fn optimistic_conflict_is_refused_and_rolled_back() {
    let dir = scratch("conflict");
    let shared = open(&dir);
    let mut a = session(&shared);
    let mut b = session(&shared);
    assert_eq!(run(&mut a, "CREATE TABLE cf_t (id INTEGER)"), "OK 0\n");
    assert_eq!(run(&mut a, "BEGIN"), "OK 0\n");
    assert_eq!(run(&mut b, "INSERT INTO cf_t VALUES (1)"), "OK 1\n");
    assert!(run(&mut a, "INSERT INTO cf_t VALUES (2)").starts_with("ERR busy"));
    assert!(run(&mut a, "COMMIT").starts_with("ERR txn"));
    assert_eq!(
        normalize(&run(&mut b, "SELECT id AS id FROM cf_t ORDER BY id")),
        "ROWS 1\nid\n1\nEND 1\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn open_writer_blocks_nobody_but_writers() {
    let dir = scratch("locks");
    let shared = open(&dir);
    let mut a = session(&shared);
    let mut b = session(&shared);
    assert_eq!(run(&mut a, "CREATE TABLE lk_t (id INTEGER)"), "OK 0\n");
    assert_eq!(run(&mut a, "BEGIN"), "OK 0\n");
    assert_eq!(run(&mut a, "INSERT INTO lk_t VALUES (1)"), "OK 1\n");
    assert_eq!(
        normalize(&run(&mut b, "SELECT id AS id FROM lk_t")),
        "ROWS 1\nid\nEND 0\n"
    );
    assert_eq!(run(&mut a, "ROLLBACK"), "OK 0\n");
    assert_eq!(run(&mut b, "INSERT INTO lk_t VALUES (2)"), "OK 1\n");

    assert_eq!(run(&mut a, "BEGIN"), "OK 0\n");
    assert_eq!(run(&mut a, "INSERT INTO lk_t VALUES (3)"), "OK 1\n");
    drop(a);
    assert_eq!(run(&mut b, "INSERT INTO lk_t VALUES (4)"), "OK 1\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn torn_tail_is_dropped_and_older_records_survive() {
    let dir = scratch("torn");
    {
        let shared = open(&dir);
        let mut s = session(&shared);
        assert_eq!(run(&mut s, "CREATE TABLE tt_t (id INTEGER)"), "OK 0\n");
        assert_eq!(run(&mut s, "INSERT INTO tt_t VALUES (1)"), "OK 1\n");
        assert_eq!(run(&mut s, "INSERT INTO tt_t VALUES (2)"), "OK 1\n");
    }
    let log = format!("{}/sdb.log", dir);
    let mut file = fs::OpenOptions::new().append(true).open(&log).expect("log file");
    file.write_all(b"G\t5\nINSERT INTO tt_t VALUES (3)").expect("append torn tail");
    drop(file);
    {
        let shared = open(&dir);
        let mut s = session(&shared);
        assert_eq!(
            normalize(&run(&mut s, "SELECT id AS id FROM tt_t ORDER BY id")),
            "ROWS 1\nid\n1\n2\nEND 2\n"
        );
        assert_eq!(run(&mut s, "INSERT INTO tt_t VALUES (4)"), "OK 1\n");
    }
    let shared = open(&dir);
    let mut s = session(&shared);
    assert_eq!(
        normalize(&run(&mut s, "SELECT id AS id FROM tt_t ORDER BY id")),
        "ROWS 1\nid\n1\n2\n4\nEND 3\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stats_report_every_counter_by_name() {
    let dir = scratch("stats");
    let shared = open(&dir);
    let mut s = session(&shared);
    let out = run(&mut s, "SHOW STATS");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "ROWS 2");
    assert_eq!(lines[1], "name\tvalue");
    let names: Vec<&str> = lines[2..lines.len() - 1]
        .iter()
        .map(|l| l.split('\t').next().unwrap())
        .collect();
    for want in [
        "statements",
        "selects",
        "inserts",
        "updates",
        "deletes",
        "commits",
        "rollbacks",
        "errors",
        "connections_current",
        "connections_total",
    ] {
        assert!(names.contains(&want), "missing counter {}", want);
    }
    assert_eq!(lines.last().copied(), Some(format!("END {}", names.len()).as_str()));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explain_reports_index_or_scan() {
    let dir = scratch("explain");
    let shared = open(&dir);
    let mut s = session(&shared);
    assert_eq!(run(&mut s, "CREATE TABLE ex_t (k INTEGER, v TEXT)"), "OK 0\n");
    assert_eq!(run(&mut s, "CREATE UNIQUE INDEX ex_k ON ex_t (k)"), "OK 0\n");
    let indexed = run(&mut s, "EXPLAIN SELECT v AS v FROM ex_t WHERE k = 2");
    assert!(indexed.contains("INDEX ex_t.ex_k"), "plan was {}", indexed);
    assert!(!indexed.contains("SCAN ex_t"), "plan was {}", indexed);
    let scanned = run(&mut s, "EXPLAIN SELECT v AS v FROM ex_t WHERE v = 'x'");
    assert!(scanned.contains("SCAN ex_t"), "plan was {}", scanned);
    assert!(!scanned.contains("INDEX ex_t."), "plan was {}", scanned);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn connection_loop_pipelines_and_answers_unterminated_input() {
    let dir = scratch("net");
    let shared = open(&dir);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let db = Arc::clone(&shared);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let db = Arc::clone(&db);
            thread::spawn(move || handle_conn(stream, db));
        }
    });

    let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    c.write_all(b"CREATE TABLE nt (id INTEGER)\nINSERT INTO nt VALUES (1)\nSELECT id AS id FROM nt\n")
        .expect("send");
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let mut got = String::new();
    for _ in 0..6 {
        let mut line = String::new();
        r.read_line(&mut line).expect("answer line");
        got.push_str(&line);
    }
    assert_eq!(got, "OK 0\nOK 1\nROWS 1\nid\n1\nEND 1\n");

    c.write_all(b"SELECT 1 AS one").expect("send unterminated");
    c.shutdown(Shutdown::Write).expect("half close");
    let mut rest = String::new();
    r.read_to_string(&mut rest).expect("drain");
    assert_eq!(rest, "ROWS 1\none\n1\nEND 1\n");

    let mut g = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    g.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    g.write_all(b"\xff\xfe\x00 garbage\n").expect("send garbage");
    let mut gr = BufReader::new(g);
    let mut line = String::new();
    gr.read_line(&mut line).expect("garbage answer");
    assert!(line.starts_with("ERR "), "answer was {}", line);

    let _ = fs::remove_dir_all(&dir);
}

type Msg = (u8, Vec<u8>);

fn cstring(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn be_i16(b: &[u8]) -> i16 {
    i16::from_be_bytes([b[0], b[1]])
}

fn pg_cstr(b: &[u8], p: usize) -> (String, usize) {
    let end = b[p..].iter().position(|&c| c == 0).map_or(b.len(), |z| p + z);
    (String::from_utf8_lossy(&b[p..end]).into_owned(), end + 1)
}

fn pg_names(b: &[u8]) -> Vec<String> {
    let mut p = 2;
    (0..be_i16(b))
        .map(|_| {
            let (name, next) = pg_cstr(b, p);
            p = next + 18;
            name
        })
        .collect()
}

fn pg_fields(b: &[u8]) -> Vec<Option<String>> {
    let mut p = 2;
    (0..be_i16(b))
        .map(|_| {
            let len = i32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]);
            p += 4;
            if len < 0 {
                return None;
            }
            let v = String::from_utf8_lossy(&b[p..p + len as usize]).into_owned();
            p += len as usize;
            Some(v)
        })
        .collect()
}

fn pg_error_code(b: &[u8]) -> String {
    let mut p = 0;
    while p < b.len() && b[p] != 0 {
        let key = b[p];
        let (val, next) = pg_cstr(b, p + 1);
        if key == b'C' {
            return val;
        }
        p = next;
    }
    String::new()
}

/// Renders backend messages as short tokens, so each test reads as a transcript.
fn pg_summary(msgs: &[Msg]) -> String {
    let toks: Vec<String> = msgs
        .iter()
        .map(|(t, b)| match *t {
            b'R' => format!("R{}", i32::from_be_bytes([b[0], b[1], b[2], b[3]])),
            b'Z' => format!("Z{}", b[0] as char),
            b'C' => format!("C:{}", pg_cstr(b, 0).0),
            b'E' => format!("E:{}", pg_error_code(b)),
            b'T' => format!("T[{}]", pg_names(b).join(",")),
            b'D' => {
                let fields = pg_fields(b);
                let vals: Vec<&str> = fields.iter().map(|f| f.as_deref().unwrap_or("~")).collect();
                format!("D[{}]", vals.join(","))
            }
            b't' => format!("t{}", be_i16(b)),
            other => (other as char).to_string(),
        })
        .collect();
    toks.join(" ")
}

/// Starts a PostgreSQL-protocol listener on loopback; every connection runs `pg::handle`.
fn pg_listener(db: &Arc<Shared>, password: Option<&str>) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind pg loopback");
    let port = listener.local_addr().expect("local addr").port();
    let db = Arc::clone(db);
    let pw = password.map(String::from);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let db = Arc::clone(&db);
            let pw = pw.clone();
            thread::spawn(move || pg::handle(stream, db, pw));
        }
    });
    port
}

struct PgClient {
    s: TcpStream,
}

impl PgClient {
    fn connect(port: u16) -> PgClient {
        let s = TcpStream::connect(("127.0.0.1", port)).expect("connect pg");
        s.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
        PgClient { s }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).expect("send raw");
    }

    fn send(&mut self, tag: Option<u8>, body: &[u8]) {
        let mut out: Vec<u8> = tag.into_iter().collect();
        out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        out.extend_from_slice(body);
        self.raw(&out);
    }

    fn byte(&mut self) -> u8 {
        let mut b = [0u8; 1];
        self.s.read_exact(&mut b).expect("single byte answer");
        b[0]
    }

    fn try_read(&mut self) -> Option<Msg> {
        let mut hdr = [0u8; 5];
        self.s.read_exact(&mut hdr).ok()?;
        let len = i32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        self.s.read_exact(&mut body).ok()?;
        Some((hdr[0], body))
    }

    fn until_ready(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        loop {
            let m = self.try_read().expect("message before ReadyForQuery");
            let ready = m.0 == b'Z';
            out.push(m);
            if ready {
                return out;
            }
        }
    }

    fn drain(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        while let Some(m) = self.try_read() {
            out.push(m);
        }
        out
    }

    fn query(&mut self, sql: &str) -> String {
        self.send(Some(b'Q'), &cstring(sql));
        pg_summary(&self.until_ready())
    }

    fn startup_body(user: &str) -> Vec<u8> {
        let mut body = 196608i32.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0");
        body.extend_from_slice(&cstring(user));
        body.extend_from_slice(b"database\0sdb\0\0");
        body
    }

    fn startup(&mut self, password: Option<&str>) -> String {
        let mut out = Vec::new();
        self.send(None, &PgClient::startup_body("sdb"));
        if let Some(pw) = password {
            out.push(self.try_read().expect("auth request"));
            self.send(Some(b'p'), &cstring(pw));
        }
        out.extend(self.until_ready());
        pg_summary(&out)
    }

    /// Sends every message in `msgs`, then a Sync, and summarizes the answers.
    fn batch(&mut self, msgs: &[(u8, Vec<u8>)]) -> String {
        for (t, body) in msgs {
            self.send(Some(*t), body);
        }
        self.send(Some(b'S'), &[]);
        pg_summary(&self.until_ready())
    }
}

fn pg_parse(name: &str, sql: &str, oids: &[i32]) -> (u8, Vec<u8>) {
    let mut b = cstring(name);
    b.extend_from_slice(&cstring(sql));
    b.extend_from_slice(&(oids.len() as i16).to_be_bytes());
    for o in oids {
        b.extend_from_slice(&o.to_be_bytes());
    }
    (b'P', b)
}

fn pg_bind(portal: &str, stmt: &str, fmts: &[i16], vals: &[Option<&[u8]>]) -> (u8, Vec<u8>) {
    let mut b = cstring(portal);
    b.extend_from_slice(&cstring(stmt));
    b.extend_from_slice(&(fmts.len() as i16).to_be_bytes());
    for f in fmts {
        b.extend_from_slice(&f.to_be_bytes());
    }
    b.extend_from_slice(&(vals.len() as i16).to_be_bytes());
    for v in vals {
        match v {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(x) => {
                b.extend_from_slice(&(x.len() as i32).to_be_bytes());
                b.extend_from_slice(x);
            }
        }
    }
    b.extend_from_slice(&0i16.to_be_bytes());
    (b'B', b)
}

fn pg_named(kind: u8, name: &str) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend_from_slice(&cstring(name));
    b
}

fn pg_execute(portal: &str, max_rows: i32) -> (u8, Vec<u8>) {
    let mut b = cstring(portal);
    b.extend_from_slice(&max_rows.to_be_bytes());
    (b'E', b)
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

#[test]
fn pg_startup_refuses_ssl_and_reports_parameters() {
    let dir = scratch("pg_startup");
    let shared = open(&dir);
    let port = pg_listener(&shared, None);
    let mut c = PgClient::connect(port);
    let mut ssl = 8i32.to_be_bytes().to_vec();
    ssl.extend_from_slice(&80877103i32.to_be_bytes());
    c.raw(&ssl);
    assert_eq!(c.byte(), b'N');
    assert_eq!(c.startup(None), "R0 S S S S S K ZI");
    assert_eq!(c.query("SELECT 1"), "T[?column?] D[1] C:SELECT 1 ZI");
    c.send(Some(b'X'), &[]);
    assert!(c.drain().is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_simple_query_reports_tags_and_transaction_state() {
    let dir = scratch("pg_simple");
    let shared = open(&dir);
    let port = pg_listener(&shared, None);
    let mut c = PgClient::connect(port);
    c.startup(None);
    assert_eq!(c.query("CREATE TABLE pgt (id INTEGER PRIMARY KEY, s TEXT)"), "C:CREATE TABLE ZI");
    assert_eq!(c.query("BEGIN"), "C:BEGIN ZT");
    assert_eq!(c.query("INSERT INTO pgt (id, s) VALUES (1, 'x'), (2, NULL)"), "C:INSERT 0 2 ZT");
    assert_eq!(
        c.query("SELECT id, s FROM pgt ORDER BY id"),
        "T[id,s] D[1,x] D[2,~] C:SELECT 2 ZT"
    );
    assert_eq!(c.query("UPDATE pgt SET s = 'y' WHERE id = 2"), "C:UPDATE 1 ZT");
    assert_eq!(c.query("DELETE FROM pgt WHERE id = 1"), "C:DELETE 1 ZT");
    assert_eq!(c.query("COMMIT"), "C:COMMIT ZI");
    assert_eq!(c.query("SELECT id FROM pgt"), "T[id] D[2] C:SELECT 1 ZI");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_errors_carry_sqlstate_and_keep_the_connection_usable() {
    let dir = scratch("pg_errors");
    let shared = open(&dir);
    let port = pg_listener(&shared, None);
    let mut c = PgClient::connect(port);
    c.startup(None);
    assert_eq!(c.query("CREATE TABLE e (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)"), "C:CREATE TABLE ZI");
    assert_eq!(c.query("INSERT INTO e (id, n) VALUES (1, 5)"), "C:INSERT 0 1 ZI");
    assert_eq!(c.query("INSERT INTO e (id, n) VALUES (1, 6)"), "E:23505 ZI");
    assert_eq!(c.query("INSERT INTO e (id, n) VALUES (2, NULL)"), "E:23502 ZI");
    assert_eq!(c.query("INSERT INTO e (id, n) VALUES (3, 'zz')"), "E:22P02 ZI");
    assert_eq!(c.query("SELECT nope FROM e"), "E:42703 ZI");
    assert_eq!(c.query("CREATE TABLE e (id INTEGER)"), "E:42P07 ZI");
    assert_eq!(c.query("SELEKT 1"), "E:42601 ZI");
    assert_eq!(c.query("SELECT * FROM missing_tbl"), "E:42P01 ZI");
    assert_eq!(c.query("BEGIN"), "C:BEGIN ZT");
    assert_eq!(c.query("SELECT nope FROM e"), "E:42703 ZE");
    assert_eq!(c.query("SELECT 1"), "E:25P02 ZE");
    assert_eq!(c.query("COMMIT"), "C:ROLLBACK ZI");
    assert_eq!(c.query("SELECT id FROM e"), "T[id] D[1] C:SELECT 1 ZI");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_splits_statements_and_handles_empty_and_session_commands() {
    let dir = scratch("pg_multi");
    let shared = open(&dir);
    let port = pg_listener(&shared, None);
    let mut c = PgClient::connect(port);
    c.startup(None);
    assert_eq!(c.query("SELECT 1; SELECT 'a;b'"), "T[?column?] D[1] C:SELECT 1 T[?column?] D[a;b] C:SELECT 1 ZI");
    assert_eq!(c.query(""), "I ZI");
    assert_eq!(c.query("  ;  "), "I ZI");
    assert_eq!(c.query("SET search_path TO public"), "C:SET ZI");
    assert_eq!(c.query("RESET ALL"), "C:SET ZI");
    assert_eq!(c.query("DISCARD ALL"), "C:DISCARD ALL ZI");
    assert_eq!(c.query("SELECT 'it''s' AS q"), "T[q] D[it's] C:SELECT 1 ZI");
    assert_eq!(c.query("SELECT 'x\ty' AS v"), "T[v] D[x\ty] C:SELECT 1 ZI");
    assert_eq!(c.query("SELECT 'a\\b' AS v"), "T[v] D[a\\b] C:SELECT 1 ZI");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_extended_query_binds_describes_and_suspends() {
    let dir = scratch("pg_extended");
    let shared = open(&dir);
    let port = pg_listener(&shared, None);
    let mut c = PgClient::connect(port);
    c.startup(None);
    assert_eq!(c.query("CREATE TABLE sus (id INTEGER)"), "C:CREATE TABLE ZI");
    assert_eq!(c.query("INSERT INTO sus (id) VALUES (1), (2), (3)"), "C:INSERT 0 3 ZI");

    let hello = b"hello".as_slice();
    assert_eq!(
        c.batch(&[
            pg_parse("s1", "SELECT $1", &[]),
            pg_bind("", "s1", &[0], &[Some(hello)]),
            (b'D', pg_named(b'P', "")),
            pg_execute("", 0),
        ]),
        "1 2 T[?column?] D[hello] C:SELECT 1 ZI"
    );

    let forty_two = 42i32.to_be_bytes();
    assert_eq!(
        c.batch(&[
            pg_parse("s2", "SELECT $1 AS a, $2 AS b", &[23, 25]),
            (b'D', pg_named(b'S', "s2")),
            pg_bind("p2", "s2", &[1, 0], &[Some(forty_two.as_slice()), None]),
            (b'D', pg_named(b'P', "p2")),
            pg_execute("p2", 0),
        ]),
        "1 t2 n 2 T[a,b] D[42,~] C:SELECT 1 ZI"
    );

    assert_eq!(
        c.batch(&[
            pg_parse("", "SELECT id FROM sus ORDER BY id", &[]),
            pg_bind("", "", &[], &[]),
            pg_execute("", 2),
            pg_execute("", 0),
        ]),
        "1 2 T[id] D[1] D[2] s D[3] C:SELECT 3 ZI"
    );

    assert_eq!(
        c.batch(&[
            pg_parse("", "DELETE FROM sus WHERE id = 99", &[]),
            pg_bind("", "", &[], &[]),
            (b'D', pg_named(b'P', "")),
            pg_execute("", 0),
        ]),
        "1 2 n C:DELETE 0 ZI"
    );
    assert_eq!(
        c.batch(&[pg_parse("", "", &[]), pg_bind("", "", &[], &[]), (b'D', pg_named(b'P', "")), pg_execute("", 0)]),
        "1 2 n I ZI"
    );
    assert_eq!(c.batch(&[(b'C', pg_named(b'S', "s1")), pg_execute("nope", 0)]), "3 E:34000 ZI");
    assert_eq!(
        c.batch(&[pg_parse("bad", "SELECT * FROM nothing", &[]), pg_bind("", "bad", &[], &[]), pg_execute("", 0)]),
        "1 2 E:42P01 ZI"
    );
    assert_eq!(c.batch(&[pg_bind("", "ghost", &[], &[])]), "E:26000 ZI");
    assert_eq!(
        c.batch(&[pg_parse("a1", "SELECT $2", &[]), pg_bind("", "a1", &[], &[Some(b"x".as_slice())])]),
        "1 E:42P02 ZI"
    );
    assert_eq!(c.batch(&[(b'D', pg_named(b'S', "ghost"))]), "E:26000 ZI");
    assert_eq!(c.batch(&[(b'D', pg_named(b'P', "ghost"))]), "E:34000 ZI");
    assert_eq!(
        c.batch(&[pg_parse("", "SELECT * FROM nothing2", &[]), pg_bind("", "", &[], &[]), (b'D', pg_named(b'P', ""))]),
        "1 2 E:42P01 ZI"
    );
    assert_eq!(c.batch(&[(b'P', vec![])]), "E:08P01 ZI");
    assert_eq!(c.batch(&[(b'B', vec![])]), "E:08P01 ZI");
    assert_eq!(c.batch(&[(b'D', vec![])]), "E:08P01 ZI");
    assert_eq!(c.batch(&[(b'E', vec![])]), "E:08P01 ZI");
    assert_eq!(c.batch(&[(b'F', vec![])]), "E:0A000 ZI");
    assert_eq!(c.batch(&[(b'H', vec![])]), "ZI");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_password_gate_rejects_wrong_and_accepts_right() {
    let dir = scratch("pg_password");
    let shared = open(&dir);
    let port = pg_listener(&shared, Some("hunter2"));
    let mut bad = PgClient::connect(port);
    bad.send(None, &PgClient::startup_body("sdb"));
    let first = bad.try_read().expect("auth request");
    bad.send(Some(b'p'), &cstring("nope"));
    let mut msgs = vec![first];
    msgs.extend(bad.drain());
    assert_eq!(pg_summary(&msgs), "R3 E:28P01");

    let mut good = PgClient::connect(port);
    assert_eq!(good.startup(Some("hunter2")), "R3 R0 S S S S S K ZI");
    assert_eq!(good.query("SELECT 2"), "T[?column?] D[2] C:SELECT 1 ZI");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_rejects_unsupported_and_malformed_frames() {
    let dir = scratch("pg_frames");
    let shared = open(&dir);
    let port = pg_listener(&shared, None);

    let mut old = PgClient::connect(port);
    let mut body = 196609i32.to_be_bytes().to_vec();
    body.extend_from_slice(b"user\0sdb\0\0");
    old.send(None, &body);
    assert_eq!(pg_summary(&old.drain()), "E:08P01");

    let mut cancel = PgClient::connect(port);
    let mut req = 16i32.to_be_bytes().to_vec();
    req.extend_from_slice(&80877102i32.to_be_bytes());
    req.extend_from_slice(&[0u8; 8]);
    cancel.raw(&req);
    assert!(cancel.drain().is_empty());

    let mut huge = PgClient::connect(port);
    huge.raw(&20000i32.to_be_bytes());
    assert!(huge.drain().is_empty());

    let mut short = PgClient::connect(port);
    short.startup(None);
    short.raw(&[b'Q', 0, 0, 0, 3]);
    assert!(short.drain().is_empty());

    let mut quit = PgClient::connect(port);
    quit.startup(None);
    quit.send(Some(b'X'), &[]);
    assert!(quit.drain().is_empty());

    let mut c = PgClient::connect(port);
    c.startup(None);
    c.raw(&[b'F', 0, 0, 0, 4]);
    c.raw(&[b'Q', 0, 0, 0, 4]);
    c.send(Some(b'S'), &[]);
    assert_eq!(pg_summary(&c.until_ready()), "E:0A000 ZI");
    assert_eq!(c.query("SELECT 3"), "T[?column?] D[3] C:SELECT 1 ZI");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_ddl_answers_command_tags() {
    let dir = scratch("pg_ddl");
    let shared = open(&dir);
    let port = pg_listener(&shared, None);
    let mut c = PgClient::connect(port);
    c.startup(None);
    assert_eq!(c.query("CREATE TABLE d (a INTEGER, b TEXT)"), "C:CREATE TABLE ZI");
    assert_eq!(c.query("CREATE UNIQUE INDEX d_a ON d (a)"), "C:CREATE INDEX ZI");
    assert_eq!(c.query("CREATE INDEX d_b ON d (b)"), "C:CREATE INDEX ZI");
    assert_eq!(c.query("ALTER TABLE d ADD COLUMN c INTEGER"), "C:ALTER TABLE ZI");
    assert_eq!(c.query("DROP INDEX d_b"), "C:DROP INDEX ZI");
    assert_eq!(c.query("EXPLAIN SELECT a FROM d WHERE a = 1"), "T[detail] D[INDEX d.d_a] C:SELECT 1 ZI");
    assert_eq!(c.query("SELECT a FROM d"), "T[a] C:SELECT 0 ZI");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pg_helpers_map_states_values_and_parameters() {
    assert_eq!(pg::sqlstate("no_table", ""), "42P01");
    assert_eq!(pg::sqlstate("no_column", ""), "42703");
    assert_eq!(pg::sqlstate("syntax", ""), "42601");
    assert_eq!(pg::sqlstate("arity", ""), "42601");
    assert_eq!(pg::sqlstate("exists", ""), "42P07");
    assert_eq!(pg::sqlstate("ambiguous", ""), "42702");
    assert_eq!(pg::sqlstate("index", ""), "42704");
    assert_eq!(pg::sqlstate("type", ""), "22P02");
    assert_eq!(pg::sqlstate("constraint", "UNIQUE constraint failed: t"), "23505");
    assert_eq!(pg::sqlstate("constraint", "NOT NULL constraint failed: t.c"), "23502");
    assert_eq!(pg::sqlstate("constraint", "FOREIGN KEY constraint failed: t"), "23503");
    assert_eq!(pg::sqlstate("constraint", "other"), "23514");
    assert_eq!(pg::sqlstate("readonly", ""), "25006");
    assert_eq!(pg::sqlstate("txn", ""), "25001");
    assert_eq!(pg::sqlstate("busy", "transaction conflict, rolled back"), "40001");
    assert_eq!(pg::sqlstate("busy", "database is locked"), "55P03");
    assert_eq!(pg::sqlstate("full", ""), "53100");
    assert_eq!(pg::sqlstate("io", ""), "58030");
    assert_eq!(pg::sqlstate("mystery", ""), "XX000");

    assert_eq!(pg::decode_field("a\\tb\\nc\\\\d\\x").as_deref(), Some("a\tb\nc\\d\\x"));
    assert_eq!(pg::decode_field("a\\").as_deref(), Some("a\\"));
    assert_eq!(pg::decode_field("\\N"), None);

    assert_eq!(pg::param_count("SELECT $2, $1, '$9', $12"), 12);
    assert_eq!(pg::param_count("SELECT 1"), 0);

    let bound = |sql: &str, vals: &[Option<&str>], oids: &[u32]| {
        let vals: Vec<Option<String>> = vals.iter().map(|v| v.map(String::from)).collect();
        pg::bind_params(sql, &vals, oids)
    };
    assert_eq!(
        bound("SELECT $1, '$2', $2, $1", &[Some("it's"), Some("5")], &[0, 23]),
        Ok("SELECT 'it''s', '$2', 5, 'it''s'".to_string())
    );
    assert_eq!(bound("SELECT $1", &[None], &[]), Ok("SELECT NULL".to_string()));
    assert_eq!(bound("SELECT $1, $2", &[Some("-7"), Some("5")], &[25, 25]), Ok("SELECT '-7', '5'".to_string()));
    assert_eq!(bound("SELECT $1", &[Some("007")], &[0]), Ok("SELECT '007'".to_string()));
    assert!(bound("SELECT $0", &[Some("x")], &[]).is_err());
    assert!(bound("SELECT $3", &[Some("x")], &[]).is_err());

    assert_eq!(pg::param_text(&[0, 0, 0, 42], 1, 23), "42");
    assert_eq!(pg::param_text(&[0xff, 0xff, 0xff, 0xfe], 1, 23), "-2");
    assert_eq!(pg::param_text(&[0, 42], 1, 21), "42");
    assert_eq!(pg::param_text(&[0, 0, 0, 0, 0, 0, 0, 9], 1, 20), "9");
    assert_eq!(pg::param_text(b"abc", 1, 23), "abc");
    assert_eq!(pg::param_text(b"hi", 0, 25), "hi");

    assert_eq!(pg::split_statements("a;'b;c';d"), vec!["a", "'b;c'", "d"]);

    assert_eq!(pg::command_tag("INSERT", &[], 3), "INSERT 0 3");
    assert_eq!(pg::command_tag("UPDATE", &[], 2), "UPDATE 2");
    assert_eq!(pg::command_tag("DELETE", &[], 1), "DELETE 1");
    assert_eq!(pg::command_tag("CREATE", &words(&["CREATE", "UNIQUE"]), 0), "CREATE INDEX");
    assert_eq!(pg::command_tag("CREATE", &words(&["CREATE", "TABLE"]), 0), "CREATE TABLE");
    assert_eq!(pg::command_tag("DROP", &words(&["DROP", "INDEX"]), 0), "DROP INDEX");
    assert_eq!(pg::command_tag("ALTER", &words(&["ALTER", "TABLE"]), 0), "ALTER TABLE");
    assert_eq!(pg::command_tag("START", &[], 0), "BEGIN");
    assert_eq!(pg::command_tag("END", &[], 0), "COMMIT");
    assert_eq!(pg::command_tag("ROLLBACK", &[], 0), "ROLLBACK");
    assert_eq!(pg::command_tag("WITH", &[], 0), "WITH");
}

fn fake_peer(reply: Vec<u8>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind peer");
    let addr = listener.local_addr().expect("peer addr").to_string();
    thread::spawn(move || {
        if let Some(Ok(mut s)) = listener.incoming().next() {
            let mut line = String::new();
            let _ = BufReader::new(s.try_clone().expect("clone")).read_line(&mut line);
            let _ = s.write_all(&reply);
        }
    });
    addr
}

fn fake_upstream(reply: Vec<u8>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr").to_string();
    thread::spawn(move || {
        if let Some(Ok(mut s)) = listener.incoming().next() {
            let mut line = String::new();
            let _ = BufReader::new(s.try_clone().expect("clone")).read_line(&mut line);
            let _ = s.write_all(&reply);
        }
    });
    addr
}

/// Replication state is process-wide: the first call fixes the config and
/// every caller shares that store, which is never clustered.
fn repl_store() -> Arc<Shared> {
    let dir = env::temp_dir().join("sdb-unit-repl").to_string_lossy().into_owned();
    let _ = fs::create_dir_all(&dir);
    let shared = open(&dir);
    repl::start(5780, &dir, None, Arc::clone(&shared));
    shared
}

#[test]
fn replay_applies_whole_groups_and_stops_at_a_torn_group() {
    let mut tables = BTreeMap::new();
    let whole = b"C\tr\tid\tINTEGER\nI\tr\t1\nG\t2\nI\tr\t2\nI\tr\t3\n";
    let mut data = whole.to_vec();
    data.extend_from_slice(b"G\t3\nI\tr\t9\n");
    assert_eq!(repl::replay_all(&mut tables, &data), whole.len());
    assert_eq!(tables["r"].rows.len(), 3);
}

#[test]
fn replication_state_reports_role_lsn_and_peers() {
    let shared = repl_store();
    assert!(repl::state_line().starts_with("STATE primary "));
    let role = repl::show_role();
    assert!(role.starts_with("ROWS 2\nname\tvalue\nrole\tprimary\nlsn\t"), "role was {}", role);
    repl::publish(7);
    repl::await_sync(7);
    assert_eq!(repl::query(&fake_peer(b"STATE primary 42 7\n".to_vec())), Some((true, 42, 7)));
    assert_eq!(repl::query(&fake_peer(b"nonsense\n".to_vec())), None);
    assert_eq!(repl::query("127.0.0.1:1"), None);
    let mut heard = Instant::now();
    repl::elect(&mut heard);
    assert!(repl::is_primary());
    drop(shared);
}

#[test]
fn replica_session_applies_frames_and_reports_protocol_events() {
    let dir = scratch("repl_session");
    let shared = open(&dir);
    let body = b"C\trs\tid\tINTEGER\nI\trs\t5\n";
    let mut frame = format!("D {}\n", body.len()).into_bytes();
    frame.extend_from_slice(body);

    let mut heard = Instant::now();
    let mut contacted = false;
    repl::session(&shared, &fake_upstream(frame), &mut heard, &mut contacted);
    assert!(contacted);
    assert_eq!(lock(&shared).tables["rs"].rows.len(), 1);

    let mut contacted = false;
    repl::session(&shared, &fake_upstream(b"H\n".to_vec()), &mut heard, &mut contacted);
    assert!(contacted);

    let mut contacted = false;
    repl::session(&shared, &fake_upstream(b"NOTPRIMARY\n".to_vec()), &mut heard, &mut contacted);
    assert!(contacted);

    let mut contacted = false;
    repl::session(&shared, &fake_upstream(b"XYZ\n".to_vec()), &mut heard, &mut contacted);
    assert!(!contacted);

    let mut contacted = false;
    repl::session(&shared, &fake_upstream(b"D 50\nabc".to_vec()), &mut heard, &mut contacted);
    assert!(!contacted);

    let mut contacted = false;
    repl::session(&shared, "127.0.0.1:1", &mut heard, &mut contacted);
    assert!(!contacted);

    repl::session(&shared, &fake_upstream(b"RESET\n".to_vec()), &mut heard, &mut contacted);
    assert!(lock(&shared).tables.is_empty());
    assert_eq!(lock(&shared).log.len, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn replica_apply_appends_to_the_log_and_replays() {
    let dir = scratch("repl_apply");
    let shared = open(&dir);
    let body = b"C\tap\tid\tINTEGER\nI\tap\t8\n";
    let len = repl::apply(&shared, body).expect("apply");
    assert_eq!(len, lock(&shared).log.len);
    assert_eq!(lock(&shared).tables["ap"].rows.len(), 1);
    repl::reset_local(&shared);
    assert!(lock(&shared).tables.is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn serve_replica_tells_a_follower_to_reset_when_it_is_ahead() {
    let _shared = repl_store();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind replica");
    let port = listener.local_addr().expect("replica addr").port();
    thread::spawn(move || {
        if let Some(Ok(s)) = listener.incoming().next() {
            let r = BufReader::new(s.try_clone().expect("clone"));
            repl::serve_replica(s, r, u64::MAX);
        }
    });
    let c = TcpStream::connect(("127.0.0.1", port)).expect("connect replica");
    c.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line).expect("reset answer");
    assert_eq!(line, "RESET\n");
}
