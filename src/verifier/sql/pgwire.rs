//! Stage 20 (PostgreSQL wire protocol): protocol 3.0 on `--pg-port`
//! (docs/PROPOSAL-sdb-sql.md, "20. PostgreSQL wire protocol").
//!
//! The harness acts as a stock frontend:
//!
//! - SSLRequest -> `N`; Startup -> AuthenticationOk (or a cleartext
//!   password request when `SDB_SQL_PASSWORD` is set), ParameterStatus
//!   (`server_version`, `client_encoding=UTF8`), BackendKeyData,
//!   ReadyForQuery `I`;
//! - simple Query: RowDescription/DataRow/CommandComplete and
//!   ReadyForQuery, with `I`/`T`/`E` tracking BEGIN/COMMIT/errors;
//! - SQLSTATE errors (`42P01` missing table, `42601` syntax, `23505`
//!   duplicate key);
//! - extended Parse/Bind/Describe/Execute/Sync with one text parameter
//!   (`SELECT $1`), plus the stock `psql` cross-check in the pipeline.
//!
//! The lite variant grades the simple-query protocol only.

use super::wire::describe_read_error;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PgConfig {
    pub user: String,
    pub password: Option<String>,
    pub timeout: Duration,
    pub extended: bool,
    /// Whether the scenario may use ticket 9's PRIMARY KEY and grade the
    /// duplicate-key SQLSTATE; false when the ladder did not pass ticket 9.
    pub unique_keys: bool,
}

impl PgConfig {
    pub fn full(password: Option<String>) -> Self {
        Self {
            user: "sdb".to_string(),
            password,
            timeout: Duration::from_secs(10),
            extended: true,
            unique_keys: true,
        }
    }

    pub fn lite(password: Option<String>) -> Self {
        Self {
            extended: false,
            ..Self::full(password)
        }
    }
}

fn be_i16(n: i16) -> [u8; 2] {
    n.to_be_bytes()
}
fn be_i32(n: i32) -> [u8; 4] {
    n.to_be_bytes()
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

#[derive(Debug, Default)]
struct Outcome {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    tag: String,
    error: Option<(String, String)>,
    txn: u8,
}

#[derive(Debug)]
struct PgConn {
    stream: TcpStream,
    timeout: Duration,
    step: &'static str,
    saw_cleartext: bool,
    server_version: Option<String>,
    client_encoding: Option<String>,
}

impl PgConn {
    fn connect(addr: &str, cfg: &PgConfig) -> Result<Self, String> {
        let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_read_timeout(Some(cfg.timeout))
            .map_err(|e| format!("timeout: {e}"))?;
        stream
            .set_write_timeout(Some(cfg.timeout))
            .map_err(|e| format!("write timeout: {e}"))?;
        stream.set_nodelay(true).ok();
        // SSLRequest: the server must refuse (we do not speak TLS).
        stream
            .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
            .map_err(|e| format!("SSLRequest: {e}"))?;
        let mut ssl = [0u8; 1];
        stream
            .read_exact(&mut ssl)
            .map_err(|e| describe_read_error(&e, cfg.timeout, "the SSLRequest startup step"))?;
        if ssl != [b'N'] {
            return Err(format!(
                "SSLRequest answered {:?}, expected N",
                ssl[0] as char
            ));
        }
        // StartupMessage 3.0.
        let mut body = Vec::new();
        body.extend_from_slice(&be_i32(196608));
        body.extend_from_slice(&cstr("user"));
        body.extend_from_slice(&cstr(&cfg.user));
        body.extend_from_slice(&cstr("database"));
        body.extend_from_slice(&cstr(&cfg.user));
        body.push(0);
        // StartupMessage has no type byte: length + body.
        let mut startup = be_i32((body.len() + 4) as i32).to_vec();
        startup.extend_from_slice(&body);
        stream
            .write_all(&startup)
            .map_err(|e| format!("startup: {e}"))?;
        let mut c = PgConn {
            stream,
            timeout: cfg.timeout,
            step: "the startup step",
            saw_cleartext: false,
            server_version: None,
            client_encoding: None,
        };
        loop {
            let Some((t, b)) = c.read_msg()? else {
                return Err("startup: connection closed".to_string());
            };
            match t {
                b'R' => {
                    if b.len() < 4 {
                        return Err("startup Authentication message is short".to_string());
                    }
                    let code = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    if code == 3 {
                        c.saw_cleartext = true;
                        let Some(pw) = &cfg.password else {
                            return Err("the server requested a password but none is configured"
                                .to_string());
                        };
                        let mut msg = pw.as_bytes().to_vec();
                        msg.push(0);
                        Self::write_msg(&mut c.stream, b'p', &msg)?;
                    } else if code != 0 {
                        return Err(format!("unsupported authentication method {code}"));
                    }
                }
                b'S' => {
                    let fields: Vec<&[u8]> = b.split(|&x| x == 0).collect();
                    if fields.len() >= 2 {
                        let k = String::from_utf8_lossy(fields[0]).to_string();
                        let v = String::from_utf8_lossy(fields[1]).to_string();
                        if k == "server_version" {
                            c.server_version = Some(v);
                        } else if k == "client_encoding" {
                            c.client_encoding = Some(v);
                        }
                    }
                }
                b'K' => {}
                b'Z' => break,
                b'E' => {
                    let (code, msg) = parse_error(&b);
                    return Err(format!("startup failed: {code} {msg}"));
                }
                _ => {}
            }
        }
        if cfg.password.is_some() && !c.saw_cleartext {
            return Err("a password is configured but the server did not ask for it".to_string());
        }
        if c.server_version.is_none() {
            return Err("startup is missing ParameterStatus server_version".to_string());
        }
        if c.client_encoding.as_deref() != Some("UTF8") {
            return Err(format!(
                "client_encoding is {:?}, expected UTF8",
                c.client_encoding
            ));
        }
        Ok(c)
    }

    fn write_msg(stream: &mut TcpStream, t: u8, body: &[u8]) -> Result<(), String> {
        let mut buf = Vec::with_capacity(body.len() + 5);
        buf.push(t);
        buf.extend_from_slice(&be_i32((body.len() + 4) as i32));
        buf.extend_from_slice(body);
        stream.write_all(&buf).map_err(|e| format!("write: {e}"))
    }

    fn read_msg(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        let mut t = [0u8; 1];
        match self.stream.read_exact(&mut t) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(describe_read_error(&e, self.timeout, self.step)),
        }
        let mut l = [0u8; 4];
        self.stream
            .read_exact(&mut l)
            .map_err(|e| describe_read_error(&e, self.timeout, self.step))?;
        let n = i32::from_be_bytes(l);
        if !(4..=64 * 1024 * 1024).contains(&n) {
            return Err(format!("bad message length {n}"));
        }
        let mut body = vec![0u8; (n - 4) as usize];
        self.stream
            .read_exact(&mut body)
            .map_err(|e| describe_read_error(&e, self.timeout, self.step))?;
        Ok(Some((t[0], body)))
    }

    fn send_msg(&mut self, t: u8, body: &[u8]) -> Result<(), String> {
        Self::write_msg(&mut self.stream, t, body)
    }

    fn read_outcome(&mut self) -> Result<Outcome, String> {
        let mut o = Outcome::default();
        loop {
            let Some((t, b)) = self.read_msg()? else {
                return Err("connection closed before ReadyForQuery".to_string());
            };
            match t {
                b'T' => o.columns = parse_rowdesc(&b)?,
                b'D' => o.rows.push(parse_datarow(&b)?),
                b'C' => {
                    o.tag = b
                        .split(|&x| x == 0)
                        .next()
                        .map(|s| String::from_utf8_lossy(s).to_string())
                        .unwrap_or_default()
                }
                b'E' => o.error = Some(parse_error(&b)),
                b'Z' => {
                    if b.len() == 1 {
                        o.txn = b[0];
                    }
                    return Ok(o);
                }
                b'1' | b'2' | b'3' | b'n' | b't' | b'I' => {}
                _ => {}
            }
        }
    }

    fn simple(&mut self, sql: &str) -> Result<Outcome, String> {
        self.step = "the simple-query step";
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send_msg(b'Q', &body)?;
        self.read_outcome()
    }

    fn extended(&mut self, sql: &str, params: &[&str]) -> Result<Outcome, String> {
        self.step = "the extended-query step";
        // Parse.
        let mut p = Vec::new();
        p.extend_from_slice(&cstr(""));
        p.extend_from_slice(&cstr(sql));
        p.extend_from_slice(&be_i16(params.len() as i16));
        for _ in params {
            p.extend_from_slice(&be_i32(25)); // text
        }
        self.send_msg(b'P', &p)?;
        // Bind.
        let mut b = Vec::new();
        b.extend_from_slice(&cstr("")); // portal
        b.extend_from_slice(&cstr("")); // statement
        b.extend_from_slice(&be_i16(0)); // parameter formats
        b.extend_from_slice(&be_i16(params.len() as i16));
        for v in params {
            b.extend_from_slice(&be_i32(v.len() as i32));
            b.extend_from_slice(v.as_bytes());
        }
        b.extend_from_slice(&be_i16(0)); // result formats
        self.send_msg(b'B', &b)?;
        // Describe the portal, execute, sync.
        let mut d = vec![b'P'];
        d.extend_from_slice(&cstr(""));
        self.send_msg(b'D', &d)?;
        let mut e = Vec::new();
        e.extend_from_slice(&cstr(""));
        e.extend_from_slice(&be_i32(0));
        self.send_msg(b'E', &e)?;
        self.send_msg(b'S', &[])?;
        self.read_outcome()
    }
}

/// Connect with a deliberately wrong password and return the SQLSTATE of
/// the ErrorResponse (the prompt requires 28P01).
fn wrong_password_probe(addr: &str, cfg: &PgConfig) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("probe connect: {e}"))?;
    stream
        .set_read_timeout(Some(cfg.timeout))
        .map_err(|e| format!("probe timeout: {e}"))?;
    stream
        .set_write_timeout(Some(cfg.timeout))
        .map_err(|e| format!("probe write timeout: {e}"))?;
    stream
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .map_err(|e| format!("probe SSLRequest: {e}"))?;
    let mut ssl = [0u8; 1];
    stream
        .read_exact(&mut ssl)
        .map_err(|e| format!("probe SSLRequest answer: {e}"))?;
    if ssl != [b'N'] {
        return Err("the probe's SSLRequest was not refused".to_string());
    }
    let mut body = Vec::new();
    body.extend_from_slice(&be_i32(196608));
    body.extend_from_slice(&cstr("user"));
    body.extend_from_slice(&cstr(&cfg.user));
    body.extend_from_slice(&cstr("database"));
    body.extend_from_slice(&cstr(&cfg.user));
    body.push(0);
    let mut startup = be_i32((body.len() + 4) as i32).to_vec();
    startup.extend_from_slice(&body);
    stream
        .write_all(&startup)
        .map_err(|e| format!("probe startup: {e}"))?;
    let mut sent = false;
    loop {
        let mut t = [0u8; 1];
        match stream.read_exact(&mut t) {
            Ok(()) => {}
            Err(_) => return Err("the server closed before an auth error".to_string()),
        }
        let mut l = [0u8; 4];
        stream
            .read_exact(&mut l)
            .map_err(|e| format!("probe length: {e}"))?;
        let n = i32::from_be_bytes(l);
        if !(4..=64 * 1024 * 1024).contains(&n) {
            return Err(format!("probe message length {n} is outside 4..=64 MiB"));
        }
        let mut b = vec![0u8; (n - 4) as usize];
        stream
            .read_exact(&mut b)
            .map_err(|e| format!("probe body: {e}"))?;
        match t[0] {
            b'R' if b.len() >= 4 && i32::from_be_bytes([b[0], b[1], b[2], b[3]]) == 3 => {
                let mut msg = b"sdb-wrong-password".to_vec();
                msg.push(0);
                let mut out = vec![b'p'];
                out.extend_from_slice(&be_i32((msg.len() + 4) as i32));
                out.extend_from_slice(&msg);
                stream
                    .write_all(&out)
                    .map_err(|e| format!("probe password: {e}"))?;
                sent = true;
            }
            b'R' if b.len() >= 4 && i32::from_be_bytes([b[0], b[1], b[2], b[3]]) == 0 => {
                return Err(if sent {
                    "the server accepted the wrong password".to_string()
                } else {
                    "the server accepted the connection before a password".to_string()
                })
            }
            b'E' => {
                let (code, _) = parse_error(&b);
                return Ok(code);
            }
            b'Z' => {
                return Err(if sent {
                    "the server accepted the wrong password".to_string()
                } else {
                    "the server did not request a password".to_string()
                })
            }
            _ => {}
        }
    }
}

fn parse_error(b: &[u8]) -> (String, String) {
    let mut code = String::new();
    let mut msg = String::new();
    let mut i = 0;
    while i < b.len() && b[i] != 0 {
        let key = b[i];
        i += 1;
        let end = b[i..]
            .iter()
            .position(|&x| x == 0)
            .map(|x| i + x)
            .unwrap_or(b.len());
        let val = String::from_utf8_lossy(&b[i..end]).to_string();
        match key {
            b'C' => code = val,
            b'M' => msg = val,
            _ => {}
        }
        i = end + 1;
    }
    (code, msg)
}

fn parse_rowdesc(b: &[u8]) -> Result<Vec<String>, String> {
    if b.len() < 2 {
        return Err("short RowDescription".to_string());
    }
    let n = i16::from_be_bytes([b[0], b[1]]);
    let mut pos = 2;
    let mut names = Vec::new();
    for _ in 0..n {
        let Some(end) = b[pos..].iter().position(|&x| x == 0).map(|x| pos + x) else {
            return Err("unterminated RowDescription name".to_string());
        };
        names.push(String::from_utf8_lossy(&b[pos..end]).to_string());
        pos = end + 1 + 18; // table/attnum/type/size/mod/format
        if pos > b.len() {
            return Err("truncated RowDescription".to_string());
        }
    }
    Ok(names)
}

fn parse_datarow(b: &[u8]) -> Result<Vec<Option<String>>, String> {
    if b.len() < 2 {
        return Err("short DataRow".to_string());
    }
    let n = i16::from_be_bytes([b[0], b[1]]);
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        if pos + 4 > b.len() {
            return Err("truncated DataRow".to_string());
        }
        let len = i32::from_be_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]);
        pos += 4;
        if len < 0 {
            out.push(None);
            continue;
        }
        let len = len as usize;
        if pos + len > b.len() {
            return Err("truncated DataRow value".to_string());
        }
        out.push(Some(
            String::from_utf8_lossy(&b[pos..pos + len]).to_string(),
        ));
        pos += len;
    }
    Ok(out)
}

fn one_row(o: &Outcome) -> Option<&Vec<Option<String>>> {
    o.rows.first()
}

fn expect_no_error(o: &Outcome, what: &str) -> Result<(), String> {
    if let Some((c, m)) = &o.error {
        Err(format!("{what}: unexpected error {c} {m}"))
    } else {
        Ok(())
    }
}

fn expect_error(o: &Outcome, code: &str, what: &str) -> Result<(), String> {
    match &o.error {
        Some((c, _)) if c == code => Ok(()),
        Some((c, m)) => Err(format!("{what}: SQLSTATE {c} ({m}), expected {code}")),
        None => Err(format!("{what}: no error, expected SQLSTATE {code}")),
    }
}

fn cell(o: &Outcome, what: &str) -> Result<String, String> {
    match one_row(o).and_then(|r| r.first()).and_then(|v| v.clone()) {
        Some(v) => Ok(v),
        None => Err(format!("{what}: no value")),
    }
}

/// The stage-20 contract. The caller owns the PG port, auth/password and
/// the scratch store.
pub fn scenario(addr: &str, cfg: &PgConfig) -> Result<String, String> {
    if cfg.password.is_some() {
        let code = wrong_password_probe(addr, cfg)?;
        if code != "28P01" {
            return Err(format!(
                "a wrong password answered SQLSTATE {code}, expected 28P01"
            ));
        }
    }
    let c = PgConn::connect(addr, cfg)?;
    let table = format!("pg_{:x}", 0x20u64);
    let mut c2 = c;

    let o = c2.simple("SELECT 1")?;
    expect_no_error(&o, "SELECT 1")?;
    if o.columns.len() != 1 || cell(&o, "SELECT 1")? != "1" || o.txn != b'I' {
        return Err(format!(
            "SELECT 1 answered columns {:?}, rows {:?}, txn {}",
            o.columns, o.rows, o.txn as char
        ));
    }

    let ddl = if cfg.unique_keys {
        format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, v TEXT)")
    } else {
        format!("CREATE TABLE {table} (id INTEGER, v TEXT)")
    };
    expect_no_error(&c2.simple(&ddl)?, "CREATE TABLE")?;
    expect_no_error(
        &c2.simple(&format!("INSERT INTO {table} VALUES (1, 'hello')"))?,
        "INSERT",
    )?;
    let o = c2.simple(&format!("SELECT v FROM {table} WHERE id = 1"))?;
    expect_no_error(&o, "SELECT v")?;
    if cell(&o, "SELECT v")? != "hello" {
        return Err(format!("SELECT v answered {:?}", o.rows));
    }

    let o = c2.simple("BEGIN")?;
    expect_no_error(&o, "BEGIN")?;
    if o.txn != b'T' {
        return Err(format!("after BEGIN ReadyForQuery is {}", o.txn as char));
    }
    expect_no_error(
        &c2.simple(&format!("INSERT INTO {table} VALUES (2, 'b')"))?,
        "transaction INSERT",
    )?;
    let o = c2.simple(&format!("SELECT COUNT(*) FROM {table}"))?;
    expect_no_error(&o, "transaction COUNT")?;
    if cell(&o, "transaction COUNT")? != "2" {
        return Err(format!("transaction COUNT answered {:?}", o.rows));
    }
    let o = c2.simple("COMMIT")?;
    expect_no_error(&o, "COMMIT")?;
    if o.txn != b'I' {
        return Err(format!("after COMMIT ReadyForQuery is {}", o.txn as char));
    }

    // SQLSTATE mapping.
    let o = c2.simple(&format!("SELECT * FROM {table}_missing"))?;
    expect_error(&o, "42P01", "missing table")?;
    if o.txn != b'I' {
        return Err("a failed statement outside a transaction left ReadyForQuery != I".to_string());
    }
    let o = c2.simple("SELEC 1")?;
    expect_error(&o, "42601", "syntax error")?;
    if cfg.unique_keys {
        let o = c2.simple(&format!("INSERT INTO {table} VALUES (1, 'dup')"))?;
        expect_error(&o, "23505", "duplicate key")?;
    }
    // The connection stays usable after errors.
    let o = c2.simple("SELECT 1")?;
    expect_no_error(&o, "SELECT 1 after errors")?;
    if cell(&o, "SELECT 1 after errors")? != "1" {
        return Err("SELECT 1 after errors did not answer 1".to_string());
    }

    if cfg.extended {
        let o = c2.extended("SELECT $1", &["hello"])?;
        expect_no_error(&o, "extended SELECT $1")?;
        if cell(&o, "extended SELECT $1")? != "hello" {
            return Err(format!("extended SELECT $1 answered {:?}", o.rows));
        }
        let o = c2.extended("SELECT 2", &[])?;
        expect_no_error(&o, "extended SELECT 2")?;
        if cell(&o, "extended SELECT 2")? != "2" {
            return Err(format!("extended SELECT 2 answered {:?}", o.rows));
        }
        let o = c2.extended("SELEC 1", &[])?;
        expect_error(&o, "42601", "extended syntax error")?;
        let o = c2.simple("SELECT 1")?;
        expect_no_error(&o, "SELECT 1 after extended error")?;
        if cell(&o, "SELECT 1 after extended error")? != "1" {
            return Err("the connection broke after an extended error".to_string());
        }
    }

    Ok(if cfg.extended {
        "SSL refused; startup/auth/ParameterStatus ok; simple + extended query, transactions and SQLSTATE (42P01/42601/23505) exact".to_string()
    } else {
        "SSL refused; startup/auth/ParameterStatus ok; simple query, transactions and SQLSTATE (42P01/42601/23505) exact".to_string()
    })
}

#[cfg(test)]
mod tests;
