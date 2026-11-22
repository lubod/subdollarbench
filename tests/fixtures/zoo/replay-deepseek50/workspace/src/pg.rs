// PostgreSQL wire protocol (3.0) front end.
//
// The engine itself speaks a line protocol; this module accepts ordinary
// PostgreSQL clients on a second listener and translates both directions.
// Statements are executed through `process_line`, so the two listeners share
// one database, one set of transaction slots and one WAL.

use super::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering as AO};

/// Protocol version 3.0.
const PROTO_V3: i32 = 196608;
const SSL_REQUEST: i32 = 80877103;
const GSSENC_REQUEST: i32 = 80877104;
const CANCEL_REQUEST: i32 = 80877102;

/// Upper bound for a single frontend message; larger frames close the
/// connection rather than letting a peer allocate arbitrary memory.
const MAX_FRAME: usize = 64 * 1024 * 1024;

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

struct PgSlot;

impl Drop for PgSlot {
    fn drop(&mut self) {
        CONNS_OPEN.fetch_sub(1, AO::SeqCst);
    }
}

/// Start the PG listener on its own thread.
pub fn init(port: u16, db: Arc<Mutex<Db>>, password: Option<String>) {
    thread::spawn(move || {
        let addr = format!("0.0.0.0:{}", port);
        let listener = loop {
            match TcpListener::bind(&addr) {
                Ok(l) => break l,
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        };
        for stream in listener.incoming() {
            let s = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let taken = CONNS_OPEN.fetch_add(1, AO::SeqCst);
            if taken >= MAX_CONNS as u64 {
                CONNS_OPEN.fetch_sub(1, AO::SeqCst);
                // Clean close: the peer sees EOF rather than an SQL error.
                let _ = s.shutdown(Shutdown::Both);
                continue;
            }
            let d = Arc::clone(&db);
            let pw = password.clone();
            let spawned = thread::Builder::new()
                .stack_size(16 * 1024 * 1024)
                .spawn(move || {
                    let _slot = PgSlot;
                    handle_conn(s, d, pw);
                });
            if spawned.is_err() {
                CONNS_OPEN.fetch_sub(1, AO::SeqCst);
            }
        }
    });
}

// ------------------------------------------------------------- framing ---

fn put_msg(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

fn read_exact(sock: &mut TcpStream, buf: &mut [u8]) -> bool {
    match sock.read_exact(buf) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Read one startup packet. Returns (protocol code, parameter bytes).
fn read_startup(sock: &mut TcpStream) -> Option<(i32, Vec<u8>)> {
    let mut lenb = [0u8; 4];
    if !read_exact(sock, &mut lenb) {
        return None;
    }
    let len = i32::from_be_bytes(lenb);
    if len < 8 || len as usize > MAX_FRAME {
        return None;
    }
    let mut body = vec![0u8; len as usize - 4];
    if !read_exact(sock, &mut body) {
        return None;
    }
    let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    Some((code, body[4..].to_vec()))
}

/// Read one regular frontend message: tag byte plus length-prefixed body.
fn read_msg(sock: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    if !read_exact(sock, &mut h) {
        return None;
    }
    let len = i32::from_be_bytes([h[1], h[2], h[3], h[4]]);
    if len < 4 || len as usize > MAX_FRAME {
        return None;
    }
    let mut body = vec![0u8; len as usize - 4];
    if !read_exact(sock, &mut body) {
        return None;
    }
    Some((h[0], body))
}

// ------------------------------------------------------------ messages ---

fn send_error(out: &mut Vec<u8>, code: &str, msg: &str) {
    let mut b = Vec::with_capacity(msg.len() + 32);
    b.push(b'S');
    b.extend_from_slice(b"ERROR\0");
    b.push(b'V');
    b.extend_from_slice(b"ERROR\0");
    b.push(b'C');
    b.extend_from_slice(&cstr(code));
    b.push(b'M');
    // Keep the wire free of embedded newlines/terminators.
    let clean: String = msg.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    b.extend_from_slice(&cstr(&clean));
    b.push(0);
    put_msg(out, b'E', &b);
}

fn send_ready(out: &mut Vec<u8>, status: u8) {
    put_msg(out, b'Z', &[status]);
}

fn send_auth_ok(out: &mut Vec<u8>) {
    put_msg(out, b'R', &0i32.to_be_bytes());
}

fn send_auth_cleartext(out: &mut Vec<u8>) {
    put_msg(out, b'R', &3i32.to_be_bytes());
}

fn send_param(out: &mut Vec<u8>, name: &str, value: &str) {
    let mut b = Vec::with_capacity(name.len() + value.len() + 2);
    b.extend_from_slice(&cstr(name));
    b.extend_from_slice(&cstr(value));
    put_msg(out, b'S', &b);
}

fn send_backend_key(out: &mut Vec<u8>, pid: i32) {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&pid.to_be_bytes());
    b.extend_from_slice(&0x5db5u32.wrapping_add(pid as u32).to_be_bytes());
    put_msg(out, b'K', &b);
}

fn send_command_complete(out: &mut Vec<u8>, tag: &str) {
    put_msg(out, b'C', &cstr(tag));
}

// -------------------------------------------------------------- helpers ---

fn u16_at(b: &[u8], i: usize) -> (u16, usize) {
    if i + 2 > b.len() {
        return (0, b.len());
    }
    (u16::from_be_bytes([b[i], b[i + 1]]), i + 2)
}

fn i32_at(b: &[u8], i: usize) -> (i32, usize) {
    if i + 4 > b.len() {
        return (0, b.len());
    }
    (i32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]), i + 4)
}

/// Read a NUL-terminated string starting at `i`; returns (string, next index).
fn take_cstr(b: &[u8], i: usize) -> (String, usize) {
    let mut j = i;
    while j < b.len() && b[j] != 0 {
        j += 1;
    }
    let s = String::from_utf8_lossy(&b[i..j.min(b.len())]).into_owned();
    (s, (j + 1).min(b.len()))
}

fn valid_ident(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------- sql staging ---

/// Remove the `pg_catalog.` qualifier outside quoted text so the engine's
/// builtins (which live in a flat namespace) can answer compatibility
/// queries issued by clients such as psql.
fn strip_pg_catalog(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut last = 0usize;
    let mut i = 0usize;
    let mut in_s = false;
    let mut in_d = false;
    while i < b.len() {
        if in_s {
            if b[i] == b'\'' {
                if i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_s = false;
            }
            i += 1;
            continue;
        }
        if in_d {
            if b[i] == b'"' {
                if i + 1 < b.len() && b[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_d = false;
            }
            i += 1;
            continue;
        }
        match b[i] {
            b'\'' => {
                in_s = true;
                i += 1;
            }
            b'"' => {
                in_d = true;
                i += 1;
            }
            b'p' | b'P' => {
                let end = i + 11;
                if end <= b.len()
                    && b[i..end].eq_ignore_ascii_case(b"pg_catalog.")
                    && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
                {
                    out.push_str(&sql[last..i]);
                    i = end;
                    last = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[last..]);
    out
}

/// Split a simple-query string into statements on top-level semicolons.
fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_s = false;
    let mut in_d = false;
    while i < b.len() {
        if in_s {
            if b[i] == b'\'' {
                if i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_s = false;
            }
            i += 1;
            continue;
        }
        if in_d {
            if b[i] == b'"' {
                if i + 1 < b.len() && b[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_d = false;
            }
            i += 1;
            continue;
        }
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            i += 2;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        match b[i] {
            b'\'' => {
                in_s = true;
                i += 1;
            }
            b'"' => {
                in_d = true;
                i += 1;
            }
            b';' => {
                out.push(sql[start..i].to_string());
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < sql.len() {
        out.push(sql[start..].to_string());
    }
    out.retain(|s| !s.trim().is_empty());
    out
}

fn numeric_oid(oid: i32) -> bool {
    matches!(oid, 20 | 21 | 23 | 26 | 700 | 701 | 1700)
}

/// Replace `$n` placeholders outside quotes with SQL literals.
fn substitute_params(sql: &str, params: &[Option<Vec<u8>>], oids: &[i32]) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut last = 0usize;
    let mut i = 0usize;
    let mut in_s = false;
    let mut in_d = false;
    while i < b.len() {
        if in_s {
            if b[i] == b'\'' {
                if i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_s = false;
            }
            i += 1;
            continue;
        }
        if in_d {
            if b[i] == b'"' {
                if i + 1 < b.len() && b[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_d = false;
            }
            i += 1;
            continue;
        }
        match b[i] {
            b'\'' => {
                in_s = true;
                i += 1;
            }
            b'"' => {
                in_d = true;
                i += 1;
            }
            b'$' => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j == i + 1 {
                    i += 1;
                    continue;
                }
                let idx: usize = sql[i + 1..j].parse().unwrap_or(0);
                let val = params.get(idx.wrapping_sub(1));
                let oid = oids.get(idx.wrapping_sub(1)).copied().unwrap_or(0);
                out.push_str(&sql[last..i]);
                out.push_str(&literal_for(val.and_then(|v| v.as_ref()), oid));
                i = j;
                last = j;
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[last..]);
    out
}

fn literal_for(v: Option<&Vec<u8>>, oid: i32) -> String {
    let bytes = match v {
        None => return "NULL".to_string(),
        Some(b) => b,
    };
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.trim();
    if numeric_oid(oid) && trimmed.parse::<i64>().is_ok() {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn max_param_index(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut max = 0usize;
    let mut i = 0usize;
    let mut in_s = false;
    let mut in_d = false;
    while i < b.len() {
        if in_s {
            if b[i] == b'\'' {
                if i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_s = false;
            }
            i += 1;
            continue;
        }
        if in_d {
            if b[i] == b'"' {
                if i + 1 < b.len() && b[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_d = false;
            }
            i += 1;
            continue;
        }
        match b[i] {
            b'\'' => {
                in_s = true;
                i += 1;
            }
            b'"' => {
                in_d = true;
                i += 1;
            }
            b'$' => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    if let Ok(n) = sql[i + 1..j].parse::<usize>() {
                        if n > max {
                            max = n;
                        }
                    }
                    i = j;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    max
}

// ---------------------------------------------------------- result wire ---

fn sqlstate_for(class: &str, msg: &str) -> &'static str {
    match class {
        "no_table" => "42P01",
        "no_column" => "42703",
        "syntax" => "42601",
        "arity" => "42601",
        "type" => "42804",
        "exists" => "42P07",
        "ambiguous" => "42702",
        "constraint" => {
            if msg.contains("UNIQUE") || msg.contains("PRIMARY KEY") || msg.contains("unique") {
                "23505"
            } else if msg.contains("NOT NULL") || msg.contains("not null") {
                "23502"
            } else if msg.contains("FOREIGN KEY") || msg.contains("foreign key") {
                "23503"
            } else {
                "23514"
            }
        }
        "txn" => "25P01",
        "busy" => "40001",
        "readonly" => "25006",
        "full" => "53100",
        "limit" => "54000",
        "auth" => "28P01",
        _ => "XX000",
    }
}

fn emit_error_response(resp: &str, out: &mut Vec<u8>) {
    // "ERR <class> <text>"
    let rest = resp.strip_prefix("ERR ").unwrap_or(resp);
    let (class, msg) = match rest.find(' ') {
        Some(p) => (&rest[..p], &rest[p + 1..]),
        None => (rest, rest),
    };
    send_error(out, sqlstate_for(class, msg), msg);
}

fn first_keyword(sql: &str) -> String {
    let s = sql.trim_start();
    let b = s.as_bytes();
    let mut i = 0usize;
    if b.len() >= 2 && b[0] == b'-' && b[1] == b'-' {
        while i < b.len() && b[i] != b'\n' {
            i += 1;
        }
        return first_keyword(&s[i..]);
    }
    while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
        i += 1;
    }
    s[..i].to_ascii_uppercase()
}

fn second_keyword(sql: &str) -> String {
    let s = sql.trim_start();
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    while i < b.len() && !b[i].is_ascii_alphabetic() {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    s[start..i].to_ascii_uppercase()
}

fn command_tag(sql: &str, n: u64) -> String {
    let kw = first_keyword(sql);
    match kw.as_str() {
        "INSERT" => format!("INSERT 0 {}", n),
        "UPDATE" => format!("UPDATE {}", n),
        "DELETE" => format!("DELETE {}", n),
        "SELECT" => format!("SELECT {}", n),
        "CREATE" => {
            let second = second_keyword(sql);
            if second == "INDEX" {
                "CREATE INDEX".to_string()
            } else if second == "TABLE" {
                "CREATE TABLE".to_string()
            } else {
                "CREATE".to_string()
            }
        }
        "DROP" => {
            let second = second_keyword(sql);
            if second == "INDEX" {
                "DROP INDEX".to_string()
            } else if second == "TABLE" {
                "DROP TABLE".to_string()
            } else {
                "DROP".to_string()
            }
        }
        "ALTER" => "ALTER TABLE".to_string(),
        "BEGIN" | "START" => "BEGIN".to_string(),
        "COMMIT" | "END" => "COMMIT".to_string(),
        "ROLLBACK" | "ABORT" => "ROLLBACK".to_string(),
        "SHOW" => "SHOW".to_string(),
        "EXPLAIN" => "EXPLAIN".to_string(),
        "PRAGMA" | "SET" => "SET".to_string(),
        "" => "OK".to_string(),
        other => other.to_string(),
    }
}

/// Split a text-protocol field into its raw value (or NULL).
fn parse_field(f: &str) -> Option<String> {
    if f == "\\N" {
        return None;
    }
    let mut out = String::with_capacity(f.len());
    let mut it = f.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('\\') => out.push('\\'),
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn type_oid(values: &[Option<String>]) -> (i32, i16) {
    let mut any = false;
    let mut all_int = true;
    for v in values {
        if let Some(s) = v {
            any = true;
            if s.parse::<i64>().is_err() {
                all_int = false;
            }
        }
    }
    if any && all_int {
        (20, 8)
    } else {
        (25, -1)
    }
}

/// Translate one engine answer into PG messages. Returns false on ERR.
fn emit_response(resp: &str, sql: &str, out: &mut Vec<u8>) -> bool {
    if resp.starts_with("ERR ") {
        emit_error_response(resp, out);
        return false;
    }
    if resp.starts_with("ROWS ") {
        let mut lines = resp.split('\n');
        let first = lines.next().unwrap_or("");
        let ncols: usize = first[5..].trim().parse().unwrap_or(0);
        let header_line = lines.next().unwrap_or("");
        let mut headers: Vec<String> = if ncols == 0 {
            Vec::new()
        } else {
            header_line.split('\t').map(|s| s.to_string()).collect()
        };
        while headers.len() < ncols {
            headers.push(String::new());
        }
        for h in headers.iter_mut() {
            if !valid_ident(h) {
                *h = "?column?".to_string();
            }
        }
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut nrows = 0u64;
        for line in lines {
            if let Some(rest) = line.strip_prefix("END ") {
                nrows = rest.trim().parse().unwrap_or(0);
                break;
            }
            let fields: Vec<Option<String>> = if ncols == 0 {
                Vec::new()
            } else {
                line.split('\t').map(parse_field).collect()
            };
            let mut row = fields;
            row.resize(ncols, None);
            rows.push(row);
        }
        // RowDescription
        let mut b = Vec::with_capacity(16 + ncols * 32);
        b.extend_from_slice(&(ncols as i16).to_be_bytes());
        for (ci, h) in headers.iter().enumerate() {
            let vals: Vec<Option<String>> = rows.iter().map(|r| r[ci].clone()).collect();
            let (oid, typlen) = type_oid(&vals);
            b.extend_from_slice(&cstr(h));
            b.extend_from_slice(&0i32.to_be_bytes()); // table oid
            b.extend_from_slice(&0i16.to_be_bytes()); // column number
            b.extend_from_slice(&oid.to_be_bytes());
            b.extend_from_slice(&typlen.to_be_bytes());
            b.extend_from_slice(&(-1i32).to_be_bytes()); // typmod
            b.extend_from_slice(&0i16.to_be_bytes()); // text format
        }
        put_msg(out, b'T', &b);
        for row in &rows {
            let mut d = Vec::with_capacity(8 + ncols * 8);
            d.extend_from_slice(&(ncols as i16).to_be_bytes());
            for cell in row {
                match cell {
                    Some(v) => {
                        d.extend_from_slice(&(v.len() as i32).to_be_bytes());
                        d.extend_from_slice(v.as_bytes());
                    }
                    None => d.extend_from_slice(&(-1i32).to_be_bytes()),
                }
            }
            put_msg(out, b'D', &d);
        }
        let kw = first_keyword(sql);
        let tag = if kw == "EXPLAIN" {
            format!("EXPLAIN {}", nrows)
        } else if kw == "SHOW" {
            "SHOW".to_string()
        } else {
            format!("SELECT {}", nrows)
        };
        send_command_complete(out, &tag);
        return true;
    }
    // "OK <n>"
    let n: u64 = resp
        .trim()
        .strip_prefix("OK ")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    send_command_complete(out, &command_tag(sql, n));
    true
}

// --------------------------------------------------------- statement run ---

/// Run one SQL string (possibly several `;`-separated statements) and emit
/// all responses except ReadyForQuery. Returns true when everything worked.
fn run_sql(
    sql: &str,
    db: &Arc<Mutex<Db>>,
    conn: u64,
    out: &mut Vec<u8>,
    failed_txn: &mut bool,
    in_txn_after: &mut bool,
) -> bool {
    let normalized = strip_pg_catalog(sql);
    let stmts = split_statements(&normalized);
    if stmts.is_empty() {
        put_msg(out, b'I', &[]);
        *in_txn_after = txn_open(db, conn);
        return true;
    }
    for st in stmts {
        let s = st.trim();
        if s.is_empty() {
            continue;
        }
        let resp = process_line(s, db, conn);
        let in_txn = txn_open(db, conn);
        *in_txn_after = in_txn;
        match resp {
            Some(r) => {
                if !emit_response(&r, s, out) {
                    if in_txn {
                        *failed_txn = true;
                    }
                    return false;
                }
                if !in_txn {
                    *failed_txn = false;
                }
            }
            None => {
                if !in_txn {
                    *failed_txn = false;
                }
            }
        }
    }
    true
}

/// Parse-only check used by the extended protocol's Parse message. Parameter
/// placeholders are replaced with NULL: their values are irrelevant to the
/// grammar. Name resolution and execution stay at Execute time.
fn validate_sql(sql: &str) -> Result<(), SqErr> {
    let normalized = strip_pg_catalog(sql);
    let stmts = split_statements(&normalized);
    for st in stmts {
        let s = st.trim();
        if s.is_empty() {
            continue;
        }
        let filled = substitute_params(s, &[], &[]);
        let toks = lex(&filled)?;
        if toks.is_empty() {
            continue;
        }
        let mut p = Parser::new(toks, &filled);
        p.parse_statement()?;
        while p.eat_p(';') {}
        if !p.at_end() {
            return Err(SqErr::syntax(
                "You can only execute one statement at a time.",
            ));
        }
    }
    Ok(())
}

fn txn_open(db: &Arc<Mutex<Db>>, conn: u64) -> bool {    match db.lock() {
        Ok(g) => g.txns.contains_key(&conn),
        Err(p) => p.into_inner().txns.contains_key(&conn),
    }
}

fn ready_status(db: &Arc<Mutex<Db>>, conn: u64, failed_txn: bool) -> u8 {
    if txn_open(db, conn) {
        if failed_txn {
            b'E'
        } else {
            b'T'
        }
    } else {
        b'I'
    }
}

// ------------------------------------------------------------- session ---

#[derive(Clone)]
struct Prepared {
    sql: String,
    oids: Vec<i32>,
}

struct Portal {
    stmt: String,
    params: Vec<Option<Vec<u8>>>,
}

fn handle_conn(mut sock: TcpStream, db: Arc<Mutex<Db>>, password: Option<String>) {
    let _ = sock.set_nodelay(true);
    STATS.conns_total.fetch_add(1, AO::Relaxed);
    STATS.conns_current.fetch_add(1, AO::Relaxed);
    let pid = NEXT_PID.fetch_add(1, AO::SeqCst) as i32;

    // --- startup, possibly after refusing SSL/GSS ---
    let mut user = "sdb".to_string();
    let mut database = "sdb".to_string();
    loop {
        let (code, body) = match read_startup(&mut sock) {
            Some(v) => v,
            None => {
                conn_done();
                return;
            }
        };
        match code {
            SSL_REQUEST | GSSENC_REQUEST => {
                if sock.write_all(b"N").is_err() {
                    conn_done();
                    return;
                }
                continue;
            }
            CANCEL_REQUEST => {
                conn_done();
                return;
            }
            PROTO_V3 => {
                let mut i = 0usize;
                while i < body.len() && body[i] != 0 {
                    let (k, ni) = take_cstr(&body, i);
                    i = ni;
                    if i >= body.len() {
                        break;
                    }
                    let (v, ni) = take_cstr(&body, i);
                    i = ni;
                    match k.as_str() {
                        "user" => user = v,
                        "database" => database = v,
                        _ => {}
                    }
                }
                break;
            }
            _ => {
                let mut out = Vec::new();
                send_error(&mut out, "08P01", "unsupported frontend protocol");
                let _ = sock.write_all(&out);
                conn_done();
                return;
            }
        }
    }

    // --- authentication ---
    if let Some(pw) = password.as_ref() {
        let mut out = Vec::new();
        send_auth_cleartext(&mut out);
        if sock.write_all(&out).is_err() {
            conn_done();
            return;
        }
        let supplied = match read_msg(&mut sock) {
            Some((b'p', body)) => {
                let (s, _) = take_cstr(&body, 0);
                Some(s)
            }
            _ => None,
        };
        if supplied.as_deref() != Some(pw.as_str()) {
            let mut e = Vec::new();
            send_error(&mut e, "28P01", "password authentication failed");
            let _ = sock.write_all(&e);
            let _ = sock.flush();
            conn_done();
            return;
        }
    }

    let conn = next_conn_id();
    let mut out = Vec::new();
    send_auth_ok(&mut out);
    send_param(&mut out, "server_version", "16.4");
    send_param(&mut out, "client_encoding", "UTF8");
    send_param(&mut out, "server_encoding", "UTF8");
    send_param(&mut out, "DateStyle", "ISO, MDY");
    send_param(&mut out, "integer_datetimes", "on");
    send_param(&mut out, "standard_conforming_strings", "on");
    send_param(&mut out, "TimeZone", "UTC");
    send_param(&mut out, "application_name", "");
    send_param(&mut out, "is_superuser", "on");
    send_param(&mut out, "session_authorization", &user);
    send_param(&mut out, "database", &database);
    send_backend_key(&mut out, pid);
    send_ready(&mut out, b'I');
    if sock.write_all(&out).is_err() {
        conn_done();
        return;
    }

    // --- steady state ---
    let mut prepared: HashMap<String, Prepared> = HashMap::new();
    let mut portals: HashMap<String, Portal> = HashMap::new();
    let mut failed_txn = false;
    let mut skip_until_sync = false;

    loop {
        let (tag, body) = match read_msg(&mut sock) {
            Some(v) => v,
            None => break,
        };
        let mut out = Vec::new();
        match tag {
            b'Q' => {
                let (sql, _) = take_cstr(&body, 0);
                let mut in_txn = txn_open(&db, conn);
                if !skip_until_sync {
                    run_sql(&sql, &db, conn, &mut out, &mut failed_txn, &mut in_txn);
                }
                skip_until_sync = false;
                send_ready(&mut out, ready_status(&db, conn, failed_txn));
            }
            b'P' => {
                if skip_until_sync {
                    continue;
                }
                let (name, i) = take_cstr(&body, 0);
                let (sql, mut i) = take_cstr(&body, i);
                let (n, ni) = u16_at(&body, i);
                i = ni;
                let mut oids = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let (o, ni) = i32_at(&body, i);
                    i = ni;
                    oids.push(o);
                }
                let need = max_param_index(&sql);
                while oids.len() < need {
                    oids.push(0);
                }
                if let Err(e) = validate_sql(&sql) {
                    emit_error_response(&format!("ERR {} {}", e.class, e.msg), &mut out);
                    skip_until_sync = true;
                } else {
                    prepared.insert(name, Prepared { sql, oids });
                    put_msg(&mut out, b'1', &[]);
                }
            }
            b'B' => {
                if skip_until_sync {
                    continue;
                }
                let (_portal, mut i) = take_cstr(&body, 0);
                let (stmt, ni) = take_cstr(&body, i);
                i = ni;
                let (nfmt, ni) = u16_at(&body, i);
                i = ni;
                let mut _fmts = Vec::with_capacity(nfmt as usize);
                for _ in 0..nfmt {
                    let (f, ni) = u16_at(&body, i);
                    i = ni;
                    _fmts.push(f);
                }
                let (np, ni) = u16_at(&body, i);
                i = ni;
                let mut params: Vec<Option<Vec<u8>>> = Vec::with_capacity(np as usize);
                for _ in 0..np {
                    let (l, ni) = i32_at(&body, i);
                    i = ni;
                    if l < 0 {
                        params.push(None);
                    } else {
                        let end = (i + l as usize).min(body.len());
                        params.push(Some(body[i..end].to_vec()));
                        i = end;
                    }
                }
                let (nres, ni) = u16_at(&body, i);
                i = ni;
                for _ in 0..nres {
                    let (_, n2) = u16_at(&body, i);
                    i = n2;
                }
                if !prepared.contains_key(&stmt) {
                    send_error(
                        &mut out,
                        "26000",
                        "prepared statement does not exist",
                    );
                    skip_until_sync = true;
                } else {
                    portals.insert(
                        _portal,
                        Portal { stmt, params },
                    );
                    put_msg(&mut out, b'2', &[]);
                }
            }
            b'D' => {
                if skip_until_sync {
                    continue;
                }
                if body.is_empty() {
                    put_msg(&mut out, b'n', &[]);
                } else {
                    let kind = body[0];
                    let (name, _) = take_cstr(&body, 1);
                    if kind == b'S' {
                        if let Some(p) = prepared.get(&name) {
                            let mut b = Vec::with_capacity(2 + p.oids.len() * 4);
                            b.extend_from_slice(&(p.oids.len() as i16).to_be_bytes());
                            for o in &p.oids {
                                b.extend_from_slice(&o.to_be_bytes());
                            }
                            put_msg(&mut out, b't', &b);
                        }
                    }
                    // RowDescription may arrive with Execute (contract).
                    put_msg(&mut out, b'n', &[]);
                }
            }
            b'E' => {
                if skip_until_sync {
                    continue;
                }
                let (pname, _i) = take_cstr(&body, 0);
                let (maxrows, _i2) = i32_at(&body, _i);
                let _ = maxrows;
                if let Some(portal) = portals.get(&pname) {
                    if let Some(p) = prepared.get(&portal.stmt) {
                        let sql = substitute_params(&p.sql, &portal.params, &p.oids);
                        let mut in_txn = txn_open(&db, conn);
                        let ok = run_sql(&sql, &db, conn, &mut out, &mut failed_txn, &mut in_txn);
                        if !ok {
                            skip_until_sync = true;
                        }
                    } else {
                        send_error(&mut out, "26000", "prepared statement does not exist");
                        skip_until_sync = true;
                    }
                } else {
                    send_error(&mut out, "34000", "portal does not exist");
                    skip_until_sync = true;
                }
            }
            b'S' => {
                skip_until_sync = false;
                send_ready(&mut out, ready_status(&db, conn, failed_txn));
            }
            b'H' => {}
            b'C' => {
                if body.is_empty() {
                    put_msg(&mut out, b'3', &[]);
                } else if body[0] == b'S' {
                    let (name, _) = take_cstr(&body, 1);
                    prepared.remove(&name);
                    put_msg(&mut out, b'3', &[]);
                } else {
                    let (name, _) = take_cstr(&body, 1);
                    portals.remove(&name);
                    put_msg(&mut out, b'3', &[]);
                }
            }
            b'X' => break,
            _ => {
                // Copy data, function calls and unknown messages: ignore.
            }
        }
        if !out.is_empty() && sock.write_all(&out).is_err() {
            break;
        }
    }
    conn_done();
}

fn conn_done() {
    STATS.conns_current.fetch_sub(1, AO::Relaxed);
}
