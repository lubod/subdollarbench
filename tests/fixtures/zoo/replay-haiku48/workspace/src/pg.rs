use super::{bump, run, ConnGuard, Session, Shared, STATS, ST_CONN_CURRENT, ST_CONN_TOTAL};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process;
use std::sync::atomic::Ordering as AtomOrd;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const T_TEXT: u32 = 25;
const MAX_MSG: i32 = 1 << 30;
const SSL_REQUEST: i32 = 80877103;
const GSS_REQUEST: i32 = 80877104;
const CANCEL_REQUEST: i32 = 80877102;
const PROTO_3: i32 = 196608;
const PARAMS: [(&str, &str); 5] = [
    ("server_version", "16.0"),
    ("client_encoding", "UTF8"),
    ("DateStyle", "ISO, MDY"),
    ("integer_datetimes", "on"),
    ("standard_conforming_strings", "on"),
];

enum Outcome {
    Rows(Rows),
    Done(String),
    Err(&'static str, String),
    Empty,
}

struct Rows {
    cols: Vec<String>,
    rows: VecDeque<Vec<Option<String>>>,
    total: usize,
    desc: bool,
}

struct Prepared {
    sql: String,
    oids: Vec<u32>,
}

struct Portal {
    sql: String,
    outs: Option<VecDeque<Outcome>>,
}

struct Pg {
    sock: TcpStream,
    rd: BufReader<TcpStream>,
    out: Vec<u8>,
    sess: Session,
    aborted: bool,
    skip: bool,
    stmts: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
}

pub fn serve(port: u16, db: Arc<Shared>, password: Option<String>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind pg port {}: {}", port, e);
            process::exit(1);
        }
    };
    thread::spawn(move || {
        for s in listener.incoming().flatten() {
            bump(ST_CONN_TOTAL);
            STATS[ST_CONN_CURRENT].fetch_add(1, AtomOrd::SeqCst);
            let db = Arc::clone(&db);
            let pw = password.clone();
            thread::spawn(move || {
                let _guard = ConnGuard;
                handle(s, db, pw)
            });
        }
    });
}

pub(crate) fn handle(stream: TcpStream, db: Arc<Shared>, password: Option<String>) {
    let _ = stream.set_nodelay(true);
    let Ok(clone) = stream.try_clone() else { return };
    let mut pg = Pg {
        sock: stream,
        rd: BufReader::new(clone),
        out: Vec::new(),
        sess: Session { shared: db, txn: None },
        aborted: false,
        skip: false,
        stmts: HashMap::new(),
        portals: HashMap::new(),
    };
    if pg.startup(password) {
        pg.run_loop();
    }
    let _ = pg.flush();
}

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, p: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.b.get(self.p..end)?;
        self.p = end;
        Some(s)
    }

    fn i16(&mut self) -> Option<i16> {
        let s = self.take(2)?;
        Some(i16::from_be_bytes([s[0], s[1]]))
    }

    fn i32(&mut self) -> Option<i32> {
        let s = self.take(4)?;
        Some(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn cstr(&mut self) -> Option<String> {
        let rest = self.b.get(self.p..)?;
        let z = rest.iter().position(|&c| c == 0)?;
        let s = String::from_utf8_lossy(&rest[..z]).into_owned();
        self.p += z + 1;
        Some(s)
    }
}

fn put(out: &mut Vec<u8>, t: u8, body: &[u8]) {
    out.push(t);
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
}

fn put_error(out: &mut Vec<u8>, sev: &str, code: &str, msg: &str) {
    let mut b = Vec::new();
    for (k, v) in [(b'S', sev), (b'V', sev), (b'C', code), (b'M', msg)] {
        b.push(k);
        b.extend_from_slice(v.as_bytes());
        b.push(0);
    }
    b.push(0);
    put(out, b'E', &b);
}

fn put_desc(out: &mut Vec<u8>, cols: &[String]) {
    let mut b = (cols.len() as i16).to_be_bytes().to_vec();
    for c in cols {
        b.extend_from_slice(c.as_bytes());
        b.push(0);
        b.extend_from_slice(&0i32.to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes());
        b.extend_from_slice(&T_TEXT.to_be_bytes());
        b.extend_from_slice(&(-1i16).to_be_bytes());
        b.extend_from_slice(&(-1i32).to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes());
    }
    put(out, b'T', &b);
}

fn put_row(out: &mut Vec<u8>, row: &[Option<String>]) {
    let mut b = (row.len() as i16).to_be_bytes().to_vec();
    for f in row {
        match f {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(s) => {
                b.extend_from_slice(&(s.len() as i32).to_be_bytes());
                b.extend_from_slice(s.as_bytes());
            }
        }
    }
    put(out, b'D', &b);
}

fn put_complete(out: &mut Vec<u8>, tag: &str) {
    let mut b = tag.as_bytes().to_vec();
    b.push(0);
    put(out, b'C', &b);
}

fn emit_all(out: &mut Vec<u8>, o: Outcome) -> bool {
    match o {
        Outcome::Rows(r) => {
            put_desc(out, &r.cols);
            for row in &r.rows {
                put_row(out, row);
            }
            put_complete(out, &format!("SELECT {}", r.total));
        }
        Outcome::Done(tag) => put_complete(out, &tag),
        Outcome::Empty => put(out, b'I', &[]),
        Outcome::Err(code, msg) => {
            put_error(out, "ERROR", code, &msg);
            return true;
        }
    }
    false
}

fn read_startup(rd: &mut BufReader<TcpStream>) -> Option<Vec<u8>> {
    let mut lenb = [0u8; 4];
    rd.read_exact(&mut lenb).ok()?;
    let len = i32::from_be_bytes(lenb);
    if !(8..=10000).contains(&len) {
        return None;
    }
    let mut body = vec![0u8; len as usize - 4];
    rd.read_exact(&mut body).ok()?;
    Some(body)
}

pub(crate) fn sqlstate(class: &str, msg: &str) -> &'static str {
    match class {
        "no_table" => "42P01",
        "no_column" => "42703",
        "syntax" | "arity" => "42601",
        "exists" => "42P07",
        "ambiguous" => "42702",
        "index" => "42704",
        "type" => "22P02",
        "constraint" if msg.starts_with("UNIQUE") => "23505",
        "constraint" if msg.starts_with("NOT NULL") => "23502",
        "constraint" if msg.starts_with("FOREIGN KEY") => "23503",
        "constraint" => "23514",
        "readonly" => "25006",
        "txn" => "25001",
        "busy" if msg.contains("conflict") => "40001",
        "busy" => "55P03",
        "full" => "53100",
        "io" => "58030",
        _ => "XX000",
    }
}

pub(crate) fn decode_field(f: &str) -> Option<String> {
    if f == "\\N" {
        return None;
    }
    let mut s = String::with_capacity(f.len());
    let mut it = f.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            s.push(c);
            continue;
        }
        match it.next() {
            Some('t') => s.push('\t'),
            Some('n') => s.push('\n'),
            Some('\\') => s.push('\\'),
            Some(o) => {
                s.push('\\');
                s.push(o);
            }
            None => s.push('\\'),
        }
    }
    Some(s)
}

pub(crate) fn command_tag(verb: &str, words: &[String], n: usize) -> String {
    let second = words.get(1).map(String::as_str).unwrap_or("");
    match verb {
        "INSERT" => format!("INSERT 0 {}", n),
        "UPDATE" => format!("UPDATE {}", n),
        "DELETE" => format!("DELETE {}", n),
        "CREATE" if second == "UNIQUE" => "CREATE INDEX".to_string(),
        "CREATE" => format!("CREATE {}", second),
        "DROP" => format!("DROP {}", second),
        "ALTER" => "ALTER TABLE".to_string(),
        "BEGIN" | "START" => "BEGIN".to_string(),
        "COMMIT" | "END" => "COMMIT".to_string(),
        "ROLLBACK" => "ROLLBACK".to_string(),
        other => other.to_string(),
    }
}

fn parse_frame(out: &str, verb: &str, words: &[String]) -> Outcome {
    let first = out.lines().next().unwrap_or("");
    if let Some(n) = first.strip_prefix("OK ") {
        let n = n.trim().parse().unwrap_or(0);
        return Outcome::Done(command_tag(verb, words, n));
    }
    if first.starts_with("ROWS ") {
        let mut lines: Vec<&str> = out.strip_suffix('\n').unwrap_or(out).split('\n').collect();
        if lines.len() < 3 {
            return Outcome::Err("XX000", "malformed result".to_string());
        }
        lines.pop();
        let cols: Vec<String> = lines[1].split('\t').map(String::from).collect();
        let rows: VecDeque<Vec<Option<String>>> = lines[2..]
            .iter()
            .map(|l| l.split('\t').map(decode_field).collect())
            .collect();
        return Outcome::Rows(Rows {
            cols,
            total: rows.len(),
            rows,
            desc: false,
        });
    }
    if let Some(rest) = first.strip_prefix("ERR ") {
        let (class, msg) = rest.split_once(' ').unwrap_or((rest, ""));
        return Outcome::Err(sqlstate(class, msg), msg.to_string());
    }
    Outcome::Err("XX000", first.to_string())
}

pub(crate) fn split_statements(sql: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_str = false;
    for (i, c) in sql.char_indices() {
        match c {
            '\'' => in_str = !in_str,
            ';' if !in_str => {
                out.push(&sql[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&sql[start..]);
    out
}

fn exec_one(sess: &mut Session, aborted: &mut bool, stmt: &str) -> Outcome {
    let words: Vec<String> = stmt
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
        .take(3)
        .map(|w| w.to_ascii_uppercase())
        .collect();
    let verb = words.first().cloned().unwrap_or_default();
    if *aborted {
        if matches!(verb.as_str(), "COMMIT" | "END" | "ROLLBACK") {
            *aborted = false;
            run(sess, "ROLLBACK");
            return Outcome::Done("ROLLBACK".to_string());
        }
        return Outcome::Err(
            "25P02",
            "current transaction is aborted, commands ignored until end of transaction block".to_string(),
        );
    }
    match verb.as_str() {
        "SET" | "RESET" => return Outcome::Done("SET".to_string()),
        "DISCARD" => return Outcome::Done("DISCARD ALL".to_string()),
        _ => {}
    }
    let frame = run(sess, stmt);
    let o = parse_frame(&frame, &verb, &words);
    if matches!(o, Outcome::Err(..)) && sess.txn.is_some() {
        *aborted = true;
    }
    if sess.txn.is_none() {
        *aborted = false;
    }
    o
}

fn exec_sql(sess: &mut Session, aborted: &mut bool, sql: &str) -> Vec<Outcome> {
    let mut outs = Vec::new();
    for stmt in split_statements(sql) {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        let o = exec_one(sess, aborted, stmt);
        let failed = matches!(o, Outcome::Err(..));
        outs.push(o);
        if failed {
            break;
        }
    }
    if outs.is_empty() {
        outs.push(Outcome::Empty);
    }
    outs
}

pub(crate) fn param_count(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut in_str = false;
    let mut max = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b'$' if !in_str && i + 1 < b.len() && b[i + 1].is_ascii_digit() => {
                let mut j = i + 1;
                let mut n = 0usize;
                while j < b.len() && b[j].is_ascii_digit() {
                    n = n.saturating_mul(10).saturating_add((b[j] - b'0') as usize);
                    j += 1;
                }
                max = max.max(n);
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    max
}

fn literal(v: &Option<String>, oid: u32) -> String {
    match v {
        None => "NULL".to_string(),
        Some(s) => {
            let canonical_int = s.parse::<i64>().ok().is_some_and(|n| n.to_string() == *s);
            if canonical_int && (matches!(oid, 20 | 21 | 23) || oid == 0) {
                s.clone()
            } else {
                format!("'{}'", s.replace('\'', "''"))
            }
        }
    }
}

pub(crate) fn bind_params(sql: &str, vals: &[Option<String>], oids: &[u32]) -> Result<String, String> {
    let b = sql.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            in_str = !in_str;
        } else if !in_str && c == b'$' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            let mut n = 0usize;
            while j < b.len() && b[j].is_ascii_digit() {
                n = n.saturating_mul(10).saturating_add((b[j] - b'0') as usize);
                j += 1;
            }
            if n == 0 || n > vals.len() {
                return Err(format!("there is no parameter ${}", n));
            }
            let oid = oids.get(n - 1).copied().unwrap_or(0);
            out.extend_from_slice(literal(&vals[n - 1], oid).as_bytes());
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| "invalid parameter encoding".to_string())
}

pub(crate) fn param_text(v: &[u8], fmt: i16, oid: u32) -> String {
    if fmt == 1 {
        match (oid, v.len()) {
            (23, 4) => return i32::from_be_bytes([v[0], v[1], v[2], v[3]]).to_string(),
            (21, 2) => return i16::from_be_bytes([v[0], v[1]]).to_string(),
            (20, 8) => {
                let mut a = [0u8; 8];
                a.copy_from_slice(v);
                return i64::from_be_bytes(a).to_string();
            }
            _ => {}
        }
    }
    String::from_utf8_lossy(v).into_owned()
}

fn parse_body(body: &[u8]) -> Option<(String, String, Vec<u32>)> {
    let mut r = Rd::new(body);
    let name = r.cstr()?;
    let sql = r.cstr()?;
    let n = r.i16()?.max(0) as usize;
    let mut oids = Vec::with_capacity(n);
    for _ in 0..n {
        oids.push(r.i32()? as u32);
    }
    Some((name, sql, oids))
}

type BindBody = (String, String, Vec<i16>, Vec<Option<Vec<u8>>>);

fn bind_body(body: &[u8]) -> Option<BindBody> {
    let mut r = Rd::new(body);
    let portal = r.cstr()?;
    let stmt = r.cstr()?;
    let nf = r.i16()?.max(0) as usize;
    let mut fmts = Vec::with_capacity(nf);
    for _ in 0..nf {
        fmts.push(r.i16()?);
    }
    let np = r.i16()?.max(0) as usize;
    let mut vals = Vec::with_capacity(np);
    for _ in 0..np {
        let len = r.i32()?;
        if len < 0 {
            vals.push(None);
        } else {
            vals.push(Some(r.take(len as usize)?.to_vec()));
        }
    }
    Some((portal, stmt, fmts, vals))
}

impl Pg {
    fn flush(&mut self) -> io::Result<()> {
        if !self.out.is_empty() {
            self.sock.write_all(&self.out)?;
            self.out.clear();
        }
        Ok(())
    }

    fn read_msg(&mut self) -> Option<(u8, Vec<u8>)> {
        self.flush().ok()?;
        let mut hdr = [0u8; 5];
        self.rd.read_exact(&mut hdr).ok()?;
        let len = i32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
        if !(4..=MAX_MSG).contains(&len) {
            return None;
        }
        let mut body = vec![0u8; len as usize - 4];
        self.rd.read_exact(&mut body).ok()?;
        Some((hdr[0], body))
    }

    fn status(&self) -> u8 {
        if self.sess.txn.is_none() {
            b'I'
        } else if self.aborted {
            b'E'
        } else {
            b'T'
        }
    }

    fn ready(&mut self) {
        let st = self.status();
        put(&mut self.out, b'Z', &[st]);
    }

    fn fail(&mut self, code: &str, msg: &str) {
        put_error(&mut self.out, "ERROR", code, msg);
        self.skip = true;
    }

    fn startup(&mut self, password: Option<String>) -> bool {
        let body = loop {
            let Some(body) = read_startup(&mut self.rd) else { return false };
            let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
            match code {
                SSL_REQUEST | GSS_REQUEST => {
                    if self.sock.write_all(b"N").is_err() {
                        return false;
                    }
                }
                CANCEL_REQUEST => return false,
                PROTO_3 => break body,
                _ => {
                    put_error(&mut self.out, "FATAL", "08P01", "unsupported frontend protocol");
                    return false;
                }
            }
        };
        let mut user = String::new();
        let mut r = Rd::new(&body[4..]);
        while let Some(key) = r.cstr() {
            if key.is_empty() {
                break;
            }
            let Some(val) = r.cstr() else { break };
            if key == "user" {
                user = val;
            }
        }
        if let Some(pw) = password {
            put(&mut self.out, b'R', &3i32.to_be_bytes());
            let Some((t, msg)) = self.read_msg() else { return false };
            let got = Rd::new(&msg).cstr().unwrap_or_default();
            if t != b'p' || got != pw {
                put_error(
                    &mut self.out,
                    "FATAL",
                    "28P01",
                    &format!("password authentication failed for user \"{}\"", user),
                );
                return false;
            }
        }
        put(&mut self.out, b'R', &0i32.to_be_bytes());
        for (k, v) in PARAMS {
            let mut b = k.as_bytes().to_vec();
            b.push(0);
            b.extend_from_slice(v.as_bytes());
            b.push(0);
            put(&mut self.out, b'S', &b);
        }
        let secret = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as i32)
            .unwrap_or(0);
        let mut b = (process::id() as i32).to_be_bytes().to_vec();
        b.extend_from_slice(&secret.to_be_bytes());
        put(&mut self.out, b'K', &b);
        self.ready();
        true
    }

    fn run_loop(&mut self) {
        while let Some((t, body)) = self.read_msg() {
            if t == b'X' {
                break;
            }
            if t == b'S' {
                self.skip = false;
                self.ready();
                continue;
            }
            if self.skip {
                continue;
            }
            match t {
                b'Q' => {
                    let sql = Rd::new(&body).cstr().unwrap_or_default();
                    self.simple(&sql);
                }
                b'P' => self.parse(&body),
                b'B' => self.bind(&body),
                b'D' => self.describe(&body),
                b'E' => self.execute(&body),
                b'C' => self.close(&body),
                b'H' => {}
                _ => self.fail("0A000", "unsupported message"),
            }
        }
    }

    fn simple(&mut self, sql: &str) {
        for o in exec_sql(&mut self.sess, &mut self.aborted, sql) {
            if emit_all(&mut self.out, o) {
                break;
            }
        }
        self.ready();
    }

    fn parse(&mut self, body: &[u8]) {
        let Some((name, sql, oids)) = parse_body(body) else {
            return self.fail("08P01", "invalid parse message");
        };
        self.stmts.insert(name, Prepared { sql, oids });
        put(&mut self.out, b'1', &[]);
    }

    fn bind(&mut self, body: &[u8]) {
        let Some((portal, stmt, fmts, raw)) = bind_body(body) else {
            return self.fail("08P01", "invalid bind message");
        };
        let (sql, oids) = match self.stmts.get(&stmt) {
            Some(p) => (p.sql.clone(), p.oids.clone()),
            None => {
                return self.fail("26000", &format!("prepared statement \"{}\" does not exist", stmt));
            }
        };
        let vals: Vec<Option<String>> = raw
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let fmt = match fmts.len() {
                    0 => 0,
                    1 => fmts[0],
                    _ => fmts.get(i).copied().unwrap_or(0),
                };
                v.as_ref()
                    .map(|b| param_text(b, fmt, oids.get(i).copied().unwrap_or(0)))
            })
            .collect();
        match bind_params(&sql, &vals, &oids) {
            Ok(sql) => {
                self.portals.insert(portal, Portal { sql, outs: None });
                put(&mut self.out, b'2', &[]);
            }
            Err(msg) => self.fail("42P02", &msg),
        }
    }

    fn ensure(&mut self, p: &mut Portal) {
        if p.outs.is_none() {
            p.outs = Some(exec_sql(&mut self.sess, &mut self.aborted, &p.sql).into());
        }
    }

    fn describe(&mut self, body: &[u8]) {
        let Some(&kind) = body.first() else {
            return self.fail("08P01", "invalid describe message");
        };
        let name = Rd::new(&body[1..]).cstr().unwrap_or_default();
        if kind == b'S' {
            let (sql, oids) = match self.stmts.get(&name) {
                Some(p) => (p.sql.clone(), p.oids.clone()),
                None => {
                    return self.fail("26000", &format!("prepared statement \"{}\" does not exist", name));
                }
            };
            let n = param_count(&sql).max(oids.len());
            let mut b = (n as i16).to_be_bytes().to_vec();
            for i in 0..n {
                let oid = oids.get(i).copied().unwrap_or(0);
                let oid = if oid == 0 { T_TEXT } else { oid };
                b.extend_from_slice(&oid.to_be_bytes());
            }
            put(&mut self.out, b't', &b);
            put(&mut self.out, b'n', &[]);
        } else {
            let Some(mut p) = self.portals.remove(&name) else {
                return self.fail("34000", &format!("portal \"{}\" does not exist", name));
            };
            self.ensure(&mut p);
            let mut err = None;
            match p.outs.as_mut().and_then(|o| o.front_mut()) {
                Some(Outcome::Rows(r)) => {
                    put_desc(&mut self.out, &r.cols);
                    r.desc = true;
                }
                Some(Outcome::Err(code, msg)) => err = Some((*code, msg.clone())),
                _ => put(&mut self.out, b'n', &[]),
            }
            if let Some((code, msg)) = err {
                self.fail(code, &msg);
            }
            self.portals.insert(name, p);
        }
    }

    fn execute(&mut self, body: &[u8]) {
        let mut r = Rd::new(body);
        let (Some(name), Some(maxrows)) = (r.cstr(), r.i32()) else {
            return self.fail("08P01", "invalid execute message");
        };
        let Some(mut p) = self.portals.remove(&name) else {
            return self.fail("34000", &format!("portal \"{}\" does not exist", name));
        };
        self.ensure(&mut p);
        let limit = if maxrows > 0 { maxrows as usize } else { usize::MAX };
        let outs = p.outs.get_or_insert_with(VecDeque::new);
        let mut suspended = false;
        let mut failure = None;
        while let Some(o) = outs.pop_front() {
            match o {
                Outcome::Rows(mut rs) => {
                    if !rs.desc {
                        put_desc(&mut self.out, &rs.cols);
                        rs.desc = true;
                    }
                    let n = limit.min(rs.rows.len());
                    for row in rs.rows.drain(..n) {
                        put_row(&mut self.out, &row);
                    }
                    if rs.rows.is_empty() {
                        put_complete(&mut self.out, &format!("SELECT {}", rs.total));
                    } else {
                        outs.push_front(Outcome::Rows(rs));
                        suspended = true;
                        break;
                    }
                }
                Outcome::Done(tag) => put_complete(&mut self.out, &tag),
                Outcome::Empty => put(&mut self.out, b'I', &[]),
                Outcome::Err(code, msg) => {
                    failure = Some((code, msg));
                    break;
                }
            }
        }
        if suspended {
            put(&mut self.out, b's', &[]);
        }
        if let Some((code, msg)) = failure {
            self.fail(code, &msg);
        }
        self.portals.insert(name, p);
    }

    fn close(&mut self, body: &[u8]) {
        let kind = body.first().copied().unwrap_or(0);
        let name = Rd::new(body.get(1..).unwrap_or(&[])).cstr().unwrap_or_default();
        if kind == b'S' {
            self.stmts.remove(&name);
        } else {
            self.portals.remove(&name);
        }
        put(&mut self.out, b'3', &[]);
    }
}
