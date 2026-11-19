// sdb - a small SQL database speaking a line-oriented text protocol.
// Standard library only: hand-written lexer, parser and storage engine.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- limits ---

/// Largest statement line accepted, in bytes. A line over this gets
/// `ERR limit` and is discarded up to its newline so the connection can
/// carry on with the next statement.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Maximum simultaneously open connections. Accepted connections occupy a
/// slot until their thread exits.
const MAX_CONNS: usize = 256;
/// Maximum syntactic nesting (parentheses) accepted by the parser. The
/// recursive-descent parser rejects anything deeper before it can exhaust
/// the thread stack.
const MAX_NEST: usize = 1000;
/// Joined rows retained for a statement with no deadline. Past this the
/// statement is refused with `ERR limit` instead of exhausting the store.
const JOIN_ROW_CAP: usize = 4_000_000;

/// `SDB_SQL_STATEMENT_TIMEOUT_MS`; 0 means "no budget" (every other ticket).
static STMT_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
/// Open connection count for the cap above.
static CONNS_OPEN: AtomicU64 = AtomicU64::new(0);
/// Connection ids are unique across both listeners: they key transaction
/// state, so the text and PG front ends must never share one.
static NEXT_CONN: AtomicU64 = AtomicU64::new(0);

fn next_conn_id() -> u64 {
    NEXT_CONN.fetch_add(1, AtomicOrdering::Relaxed) + 1
}

thread_local! {
    /// Per-connection deadline for the statement currently running. Each
    /// connection has its own thread, so this never races with another
    /// client's statement.
    static DEADLINE: std::cell::Cell<Option<Instant>> =
        const { std::cell::Cell::new(None) };
}

/// Arms the per-statement deadline for the lifetime of the guard.
struct DeadlineGuard;

impl DeadlineGuard {
    fn arm() -> DeadlineGuard {
        let ms = STMT_TIMEOUT_MS.load(AtomicOrdering::Relaxed);
        if ms > 0 {
            DEADLINE.with(|d| d.set(Some(Instant::now() + Duration::from_millis(ms))));
        }
        DeadlineGuard
    }
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        DEADLINE.with(|d| d.set(None));
    }
}

#[inline]
fn expired() -> bool {
    DEADLINE.with(|d| match d.get() {
        Some(dl) => Instant::now() >= dl,
        None => false,
    })
}

/// Called from execution loops: turns an elapsed statement budget into a
/// normal `ERR limit` answer instead of a partial result.
#[inline]
fn tick() -> Result<(), SqErr> {
    if expired() {
        Err(SqErr::new("limit", "statement timeout"))
    } else {
        Ok(())
    }
}

fn limit_err(msg: &str) -> SqErr {
    SqErr::new("limit", msg)
}

/// True when the current statement runs under a wall-clock budget. A runaway
/// join then gets cancelled by the deadline instead of being refused by the
/// in-memory result cap: the workload has to be allowed to run.
#[inline]
fn stmt_budget_armed() -> bool {
    STMT_TIMEOUT_MS.load(AtomicOrdering::Relaxed) > 0
}

// --------------------------------------------------------------- metrics ---

/// Process-wide service counters behind `SHOW STATS`. Atomics keep the hot
/// statement path free of extra lock traffic.
struct Stats {
    statements: AtomicU64,
    selects: AtomicU64,
    inserts: AtomicU64,
    updates: AtomicU64,
    deletes: AtomicU64,
    commits: AtomicU64,
    rollbacks: AtomicU64,
    errors: AtomicU64,
    conns_current: AtomicU64,
    conns_total: AtomicU64,
}

static STATS: Stats = Stats {
    statements: AtomicU64::new(0),
    selects: AtomicU64::new(0),
    inserts: AtomicU64::new(0),
    updates: AtomicU64::new(0),
    deletes: AtomicU64::new(0),
    commits: AtomicU64::new(0),
    rollbacks: AtomicU64::new(0),
    errors: AtomicU64::new(0),
    conns_current: AtomicU64::new(0),
    conns_total: AtomicU64::new(0),
};

impl Stats {
    fn get(v: &AtomicU64) -> u64 {
        v.load(AtomicOrdering::Relaxed)
    }

    fn show(&self) -> String {
        let rows: [(&str, u64); 10] = [
            ("commits", Stats::get(&self.commits)),
            ("connections_current", Stats::get(&self.conns_current)),
            ("connections_total", Stats::get(&self.conns_total)),
            ("deletes", Stats::get(&self.deletes)),
            ("errors", Stats::get(&self.errors)),
            ("inserts", Stats::get(&self.inserts)),
            ("rollbacks", Stats::get(&self.rollbacks)),
            ("selects", Stats::get(&self.selects)),
            ("statements", Stats::get(&self.statements)),
            ("updates", Stats::get(&self.updates)),
        ];
        let mut out = String::with_capacity(256);
        out.push_str("ROWS 2\nname\tvalue\n");
        for (name, value) in rows {
            out.push_str(name);
            out.push('\t');
            out.push_str(&itoa(value as i64));
            out.push('\n');
        }
        out.push_str("END 10\n");
        out
    }

    /// Book-keeping for one non-empty statement line: the total count plus
    /// the counter matching the statement's first keyword.
    fn record_statement(&self, s: &str) {
        self.statements.fetch_add(1, AtomicOrdering::Relaxed);
        let b = s.as_bytes();
        let mut i = 0usize;
        while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
            i += 1;
        }
        if i == 0 {
            return;
        }
        let counter = match &s[..i].to_ascii_lowercase()[..] {
            "select" => &self.selects,
            "insert" => &self.inserts,
            "update" => &self.updates,
            "delete" => &self.deletes,
            "commit" => &self.commits,
            "rollback" => &self.rollbacks,
            _ => return,
        };
        counter.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

// ---------------------------------------------------------------- values ---

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Int,
    Text,
}

impl Ty {
    fn from_name(s: &str) -> Option<Ty> {
        match s {
            "integer" | "int" => Some(Ty::Int),
            "text" => Some(Ty::Text),
            _ => None,
        }
    }
    fn code(self) -> &'static str {
        match self {
            Ty::Int => "i",
            Ty::Text => "t",
        }
    }
    fn from_code(s: &str) -> Option<Ty> {
        match s {
            "i" => Some(Ty::Int),
            "t" => Some(Ty::Text),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    I(i64),
    T(String),
}

fn key_of(v: &Value) -> Option<Key> {
    match v {
        Value::Int(i) => Some(Key::I(*i)),
        Value::Text(s) => Some(Key::T(s.clone())),
        _ => None,
    }
}

fn fmt_real(f: f64) -> String {
    if f == 0.0 {
        return "0.0".to_string();
    }
    if f.is_finite() && f == f.trunc() && f.abs() < 1e15 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
}

fn push_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("\\N"),
        Value::Int(i) => {
            let buf = itoa(*i);
            out.push_str(&buf);
        }
        Value::Real(f) => out.push_str(&fmt_real(*f)),
        Value::Text(s) => escape_text(s, out),
    }
}

fn itoa(v: i64) -> String {
    v.to_string()
}

// ---------------------------------------------------------------- errors ---

#[derive(Debug, Clone)]
struct SqErr {
    class: &'static str,
    msg: String,
}

impl SqErr {
    fn new(class: &'static str, msg: impl Into<String>) -> SqErr {
        SqErr { class, msg: msg.into() }
    }
    fn syntax(msg: &str) -> SqErr {
        SqErr::new("syntax", msg)
    }
    fn response(&self) -> String {
        let mut m = String::with_capacity(self.msg.len());
        for c in self.msg.chars() {
            if c == '\n' || c == '\r' {
                m.push(' ');
            } else {
                m.push(c);
            }
        }
        format!("ERR {} {}\n", self.class, m)
    }
}

fn no_table(name: &str) -> SqErr {
    SqErr::new("no_table", format!("no such table: {}", name))
}

fn no_column(name: &str) -> SqErr {
    SqErr::new("no_column", format!("no such column: {}", name))
}

fn err_arity(msg: String) -> SqErr {
    SqErr::new("arity", msg)
}

fn err_type(msg: String) -> SqErr {
    SqErr::new("type", msg)
}

fn err_txn(msg: &str) -> SqErr {
    SqErr::new("txn", msg)
}

fn full_err() -> SqErr {
    SqErr::new("full", "database or disk is full")
}

/// Free bytes available to this process on the filesystem holding `dir`.
///
/// `statvfs(3)` is the only portable way to ask a Unix kernel how much room
/// a write has left; std exposes no such call. The struct layout below is the
/// Linux `struct statvfs` ABI (all counters are 64-bit words). When the call
/// fails we return `None` and the caller falls back to handling the write
/// error itself, so a wrong guess can never reject a healthy write.
fn fs_available(dir: &std::path::Path) -> Option<u64> {
    #[repr(C)]
    struct StatVfs {
        f_bsize: u64,
        f_frsize: u64,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_favail: u64,
        f_fsid: u64,
        f_flag: u64,
        f_namemax: u64,
        f_spare: [u32; 6],
    }
    extern "C" {
        fn statvfs(path: *const std::os::raw::c_char, buf: *mut StatVfs) -> std::os::raw::c_int;
    }
    let c = std::ffi::CString::new(dir.to_str()?).ok()?;
    let mut st = std::mem::MaybeUninit::<StatVfs>::zeroed();
    let rc = unsafe { statvfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    let unit = if st.f_frsize != 0 { st.f_frsize } else { st.f_bsize };
    Some(st.f_bavail.saturating_mul(unit))
}

/// True when `e` is the kernel saying the volume is out of room.
fn is_no_space(e: &std::io::Error) -> bool {
    match e.raw_os_error() {
        Some(28) | Some(122) | Some(27) => true, // ENOSPC, EDQUOT, EFBIG
        _ => e.kind() == std::io::ErrorKind::StorageFull,
    }
}

fn constraint_err(msg: &str) -> SqErr {
    SqErr::new("constraint", msg)
}

/// A foreign key resolved against its parent table for one statement.
#[derive(Clone, Debug)]
struct RFk {
    cols: Vec<usize>,
    parent: String,
    pcols: Vec<usize>,
}

// ---------------------------------------------------------------- lexer ---

#[derive(Clone, Debug)]
enum Tk {
    Ident(String),
    Int(i64),
    Real(f64),
    Str(String),
    P(char),
}

#[derive(Clone, Debug)]
struct Token {
    tk: Tk,
    start: usize,
    end: usize,
}

fn lex(src: &str) -> Result<Vec<Token>, SqErr> {
    // Byte-oriented: the lexer must not build a per-character table, which
    // for a multi-megabyte multi-row INSERT would cost far more than the
    // statement itself.
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut toks: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let max_tokens = 3_000_000usize;
    while i < n {
        // A multi-megabyte statement must not run past the statement budget
        // while it is still being tokenised.
        tick()?;
        let c = bytes[i];
        if c == b' ' || c == b'\t' || c == b'\r' || c == b'\n' {
            i += 1;
            continue;
        }
        if toks.len() > max_tokens {
            return Err(SqErr::syntax("statement too complex"));
        }
        let st = i;
        if c.is_ascii_alphabetic() || c == b'_' {
            i += 1;
            while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let text = src[st..i].to_ascii_lowercase();
            toks.push(Token { tk: Tk::Ident(text), start: st, end: i });
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && i + 1 < n && bytes[i + 1].is_ascii_digit()) {
            let mut is_real = false;
            while i < n && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < n && bytes[i] == b'.' {
                is_real = true;
                i += 1;
                while i < n && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
                let mut j = i + 1;
                if j < n && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < n && bytes[j].is_ascii_digit() {
                    is_real = true;
                    i = j;
                    while i < n && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text = &src[st..i];
            let tk = if is_real {
                match text.parse::<f64>() {
                    Ok(v) => Tk::Real(v),
                    Err(_) => return Err(SqErr::syntax("unrecognized token")),
                }
            } else {
                match text.parse::<i64>() {
                    Ok(v) => Tk::Int(v),
                    Err(_) => match text.parse::<f64>() {
                        Ok(v) => Tk::Real(v),
                        Err(_) => return Err(SqErr::syntax("unrecognized token")),
                    },
                }
            };
            toks.push(Token { tk, start: st, end: i });
            continue;
        }
        if c == b'\'' {
            i += 1;
            let mut s = String::new();
            let mut seg = i;
            let mut closed = false;
            while i < n {
                if bytes[i] == b'\'' {
                    if i + 1 < n && bytes[i + 1] == b'\'' {
                        s.push_str(&src[seg..i]);
                        s.push('\'');
                        i += 2;
                        seg = i;
                        continue;
                    }
                    s.push_str(&src[seg..i]);
                    i += 1;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(SqErr::syntax("unrecognized token"));
            }
            toks.push(Token { tk: Tk::Str(s), start: st, end: i });
            continue;
        }
        if c == b'"' {
            i += 1;
            let mut s = String::new();
            let mut seg = i;
            let mut closed = false;
            while i < n {
                if bytes[i] == b'"' {
                    if i + 1 < n && bytes[i + 1] == b'"' {
                        s.push_str(&src[seg..i]);
                        s.push('"');
                        i += 2;
                        seg = i;
                        continue;
                    }
                    s.push_str(&src[seg..i]);
                    i += 1;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(SqErr::syntax("unrecognized token"));
            }
            toks.push(Token { tk: Tk::Ident(s.to_lowercase()), start: st, end: i });
            continue;
        }
        if c == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
            break;
        }
        let p = match c {
            b'(' | b')' | b',' | b';' | b'*' | b'.' | b'+' | b'-' | b'/' | b'%' => c as char,
            b'=' => '=',
            b'|' if i + 1 < n && bytes[i + 1] == b'|' => {
                i += 1;
                '|'
            }
            b'<' => {
                if i + 1 < n && bytes[i + 1] == b'=' {
                    i += 1;
                    'L'
                } else if i + 1 < n && bytes[i + 1] == b'>' {
                    i += 1;
                    '!'
                } else {
                    '<'
                }
            }
            b'>' => {
                if i + 1 < n && bytes[i + 1] == b'=' {
                    i += 1;
                    'G'
                } else {
                    '>'
                }
            }
            b'!' if i + 1 < n && bytes[i + 1] == b'=' => {
                i += 1;
                '!'
            }
            _ => return Err(SqErr::syntax("unrecognized token")),
        };
        i += 1;
        toks.push(Token { tk: Tk::P(p), start: st, end: i });
    }
    Ok(toks)
}

// ---------------------------------------------------------------- ast ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AggKind {
    Count,
    Sum,
    Min,
    Max,
}

impl AggKind {
    fn label(self) -> &'static str {
        match self {
            AggKind::Count => "COUNT",
            AggKind::Sum => "SUM",
            AggKind::Min => "MIN",
            AggKind::Max => "MAX",
        }
    }
}

#[derive(Clone, Debug)]
enum Expr {
    Lit(Value),
    Col { qual: Option<String>, name: String },
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    Fn(String, Vec<Expr>),
    /// kind, argument (None is `COUNT(*)`; other kinds always carry one)
    Agg(AggKind, Option<Box<Expr>>),
    /// `(SELECT ...)` used as a value.
    Subquery(Box<Select>),
    /// `EXISTS (SELECT ...)`.
    Exists(Box<Select>),
    /// `expr [NOT] IN (a, b, ...)`.
    InList(Box<Expr>, Vec<Expr>, bool),
    /// `expr [NOT] IN (SELECT ...)`.
    InSub(Box<Expr>, Box<Select>, bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Debug)]
enum SelItem {
    /// `*` or `qualifier.*`
    Star(Option<String>),
    Expr { e: Expr, header: String, alias: bool },
}

#[derive(Clone, Debug)]
struct FromItem {
    name: String,
    alias: Option<String>,
    on: Option<Expr>,
    using: Option<Vec<String>>,
    left: bool,
}

#[derive(Clone, Debug, Default)]
struct Select {
    distinct: bool,
    items: Vec<SelItem>,
    from: Option<String>,
    from_alias: Option<String>,
    joins: Vec<FromItem>,
    filter: Option<Expr>,
    group: Vec<Expr>,
    having: Option<Expr>,
    order: Vec<(Expr, bool)>,
    limit: Option<usize>,
    offset: usize,
}

#[derive(Clone, Debug)]
struct ColDef {
    name: String,
    ty: Ty,
    notnull: bool,
    default: Option<Expr>,
    pk: bool,
    pk_desc: bool,
    /// The declared type name was exactly INTEGER (a rowid alias requires
    /// that spelling, not the INT shorthand).
    decl_int: bool,
    unique: bool,
    /// Parent table and optional parent column list.
    references: Option<(String, Vec<String>)>,
}

#[derive(Clone, Debug)]
enum TblCon {
    Pk(Vec<String>),
    Unique(Vec<String>),
    Foreign(Vec<String>, String, Vec<String>),
}

enum Stmt {
    Create { name: String, cols: Vec<ColDef>, cons: Vec<TblCon> },
    CreateIndex { name: String, table: String, col: String, unique: bool, if_not_exists: bool },
    DropIndex { name: String, if_exists: bool },
    AlterAdd { table: String, name: String, ty: Ty, default: Option<Expr>, if_not_exists: bool },
    AlterDrop { table: String, name: String, if_exists: bool },
    AlterRenameColumn { table: String, old: String, new: String },
    AlterRenameTable { table: String, new: String },
    Insert { table: String, cols: Option<Vec<String>>, rows: Vec<Vec<Expr>>, select: Option<Box<Select>> },
    Update { table: String, sets: Vec<(String, Expr)>, filter: Option<Expr> },
    Delete { table: String, filter: Option<Expr> },
    Select(Box<Select>),
    Explain(Box<Stmt>),
    ShowStats,
    ShowRole,
    Begin,
    Commit,
    Rollback,
    Pragma { name: String, arg: Option<String> },
    Noop,
}

// ---------------------------------------------------------------- parser ---

struct Parser<'a> {
    toks: Vec<Token>,
    pos: usize,
    src: &'a str,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(toks: Vec<Token>, src: &'a str) -> Parser<'a> {
        Parser { toks, pos: 0, src, depth: 0 }
    }

    fn peek(&self) -> Option<&Tk> {
        self.toks.get(self.pos).map(|t| &t.tk)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn bump(&mut self) -> Option<Tk> {
        let t = self.toks.get(self.pos).map(|t| t.tk.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat_p(&mut self, c: char) -> bool {
        if let Some(Tk::P(p)) = self.peek() {
            if *p == c {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn expect_p(&mut self, c: char) -> Result<(), SqErr> {
        if self.eat_p(c) {
            Ok(())
        } else {
            Err(SqErr::syntax("syntax error"))
        }
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if let Some(Tk::Ident(s)) = self.peek() {
            if s == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn peek_is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tk::Ident(s)) if s == kw)
    }

    /// True when the token at `off` is the keyword `kw`.
    fn tok_is_kw(&self, off: usize, kw: &str) -> bool {
        matches!(self.toks.get(off).map(|t| &t.tk), Some(Tk::Ident(s)) if s == kw)
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), SqErr> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(SqErr::syntax("syntax error"))
        }
    }

    fn ident(&mut self) -> Result<String, SqErr> {
        match self.bump() {
            Some(Tk::Ident(s)) => Ok(s),
            _ => Err(SqErr::syntax("syntax error")),
        }
    }

    fn token_start(&self) -> usize {
        self.toks.get(self.pos).map(|t| t.start).unwrap_or(self.src.len())
    }

    fn last_end(&self) -> usize {
        if self.pos == 0 {
            0
        } else {
            self.toks[self.pos - 1].end
        }
    }

    fn raw_since(&self, start: usize) -> String {
        let end = self.last_end().min(self.src.len());
        if start <= end {
            self.src[start..end].trim().to_string()
        } else {
            String::new()
        }
    }

    fn parse_statement(&mut self) -> Result<Stmt, SqErr> {
        // EXPLAIN nests statements, and a subquery statement can nest too;
        // charge the same depth budget the expression parser uses.
        if self.depth > MAX_NEST {
            return Err(limit_err("nesting too deep"));
        }
        self.depth += 1;
        let r = self.parse_statement_inner();
        self.depth -= 1;
        r
    }

    fn parse_statement_inner(&mut self) -> Result<Stmt, SqErr> {
        match self.peek() {
            Some(Tk::Ident(s)) => match s.as_str() {
                "create" => {
                    self.pos += 1;
                    self.parse_create()
                }
                "drop" => {
                    self.pos += 1;
                    self.parse_drop()
                }
                "alter" => {
                    self.pos += 1;
                    self.parse_alter()
                }
                "insert" => {
                    self.pos += 1;
                    self.parse_insert()
                }
                "select" => {
                    self.pos += 1;
                    self.parse_select()
                }
                "update" => {
                    self.pos += 1;
                    self.parse_update()
                }
                "delete" => {
                    self.pos += 1;
                    self.parse_delete()
                }
                "explain" => {
                    self.pos += 1;
                    if self.eat_kw("query") {
                        self.expect_kw("plan")?;
                    }
                    let inner = self.parse_statement()?;
                    Ok(Stmt::Explain(Box::new(inner)))
                }
                // Session settings: accept and ignore. PostgreSQL clients
                // (and libpq's startup) may issue these; they never affect
                // the engine's answers.
                "set" => {
                    self.pos += 1;
                    while !self.at_end() && !self.peek_matches_p(';') {
                        self.pos += 1;
                    }
                    Ok(Stmt::Noop)
                }
                "show" => {
                    self.pos += 1;
                    if self.eat_kw("role") {
                        Ok(Stmt::ShowRole)
                    } else {
                        self.expect_kw("stats")?;
                        Ok(Stmt::ShowStats)
                    }
                }
                "begin" => {
                    self.pos += 1;
                    // BEGIN [TRANSACTION | DEFERRED | IMMEDIATE | EXCLUSIVE]
                    if !(self.eat_kw("transaction")
                        || self.eat_kw("deferred")
                        || self.eat_kw("immediate")
                        || self.eat_kw("exclusive"))
                    {
                        // leave any other trailing token for the caller's
                        // end-of-statement check (it becomes a syntax error)
                    }
                    Ok(Stmt::Begin)
                }
                "commit" | "end" => {
                    self.pos += 1;
                    self.eat_kw("transaction");
                    Ok(Stmt::Commit)
                }
                "rollback" => {
                    self.pos += 1;
                    self.eat_kw("transaction");
                    Ok(Stmt::Rollback)
                }
                "pragma" => {
                    self.pos += 1;
                    let name = self.ident()?;
                    let mut arg = None;
                    if self.eat_p('=') || self.eat_p('(') {
                        match self.bump() {
                            Some(Tk::Ident(v)) => arg = Some(v),
                            Some(Tk::Int(v)) => arg = Some(v.to_string()),
                            Some(Tk::Real(v)) => arg = Some(v.to_string()),
                            Some(Tk::Str(v)) => arg = Some(v),
                            _ => return Err(SqErr::syntax("syntax error")),
                        }
                        self.eat_p(')');
                    }
                    Ok(Stmt::Pragma { name, arg })
                }
                _ => Err(SqErr::syntax("syntax error")),
            },
            _ => Err(SqErr::syntax("syntax error")),
        }
    }

    fn parse_create(&mut self) -> Result<Stmt, SqErr> {
        let unique = self.eat_kw("unique");
        if unique || self.peek_is_kw("index") {
            self.expect_kw("index")?;
            let mut if_not_exists = false;
            if self.eat_kw("if") {
                self.expect_kw("not")?;
                self.expect_kw("exists")?;
                if_not_exists = true;
            }
            let name = self.ident()?;
            self.expect_kw("on")?;
            let table = self.ident()?;
            self.expect_p('(')?;
            let col = self.ident()?;
            self.expect_p(')')?;
            return Ok(Stmt::CreateIndex { name, table, col, unique, if_not_exists });
        }
        self.expect_kw("table")?;
        let name = self.ident()?;
        self.expect_p('(')?;
        let mut cols: Vec<ColDef> = Vec::new();
        let mut cons: Vec<TblCon> = Vec::new();
        loop {
            if self.peek().is_none() {
                return Err(SqErr::syntax("incomplete input"));
            }
            if self.eat_p(')') {
                if cols.is_empty() {
                    return Err(SqErr::syntax("syntax error"));
                }
                break;
            }
            // Table-level constraint?
            let is_tbl_con = match self.peek() {
                Some(Tk::Ident(s)) => {
                    s == "primary" || s == "unique" || s == "foreign" || s == "constraint" || s == "check"
                }
                _ => false,
            };
            if is_tbl_con {
                if self.eat_kw("constraint") {
                    let _ = self.ident()?;
                }
                if self.eat_kw("primary") {
                    self.expect_kw("key")?;
                    let list = self.parse_col_list()?;
                    cons.push(TblCon::Pk(list));
                } else if self.eat_kw("unique") {
                    let list = self.parse_col_list()?;
                    cons.push(TblCon::Unique(list));
                } else if self.eat_kw("foreign") {
                    self.expect_kw("key")?;
                    let list = self.parse_col_list()?;
                    self.expect_kw("references")?;
                    let parent = self.ident()?;
                    let plist = if self.peek_matches_p('(') {
                        self.parse_col_list()?
                    } else {
                        Vec::new()
                    };
                    self.parse_fk_tail();
                    cons.push(TblCon::Foreign(list, parent, plist));
                } else {
                    // CHECK (...) is accepted and ignored: it is not part of
                    // this campaign's contract.
                    self.expect_kw("check")?;
                    self.expect_p('(')?;
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.bump() {
                            Some(Tk::P('(')) => depth += 1,
                            Some(Tk::P(')')) => depth -= 1,
                            Some(_) => {}
                            None => return Err(SqErr::syntax("incomplete input")),
                        }
                    }
                }
            } else {
                let cname = self.ident()?;
                let tname = self.ident()?;
                let ty = match Ty::from_name(&tname) {
                    Some(t) => t,
                    None => {
                        return Err(SqErr::syntax(&format!("unknown datatype for {}.{}: \"{}\"", name, cname, tname)))
                    }
                };
                let mut def = ColDef {
                    name: cname,
                    ty,
                    notnull: false,
                    default: None,
                    pk: false,
                    pk_desc: false,
                    decl_int: tname == "integer",
                    unique: false,
                    references: None,
                };
                loop {
                    if self.eat_kw("constraint") {
                        let _ = self.ident()?;
                        continue;
                    }
                    if self.eat_kw("primary") {
                        self.expect_kw("key")?;
                        def.pk = true;
                        if self.eat_kw("desc") {
                            def.pk_desc = true;
                        } else {
                            let _ = self.eat_kw("asc");
                        }
                        let _ = self.eat_kw("autoincrement");
                        continue;
                    }
                    if self.eat_kw("not") {
                        self.expect_kw("null")?;
                        def.notnull = true;
                        continue;
                    }
                    if self.eat_kw("null") {
                        continue;
                    }
                    if self.eat_kw("unique") {
                        def.unique = true;
                        continue;
                    }
                    if self.eat_kw("default") {
                        if self.peek().is_none() {
                            return Err(SqErr::syntax("incomplete input"));
                        }
                        // Parenthesized literal defaults are accepted.
                        let paren = self.eat_p('(');
                        let e = self.parse_expr()?;
                        if paren {
                            self.expect_p(')')?;
                        }
                        def.default = Some(e);
                        continue;
                    }
                    if self.eat_kw("references") {
                        let parent = self.ident()?;
                        let plist = if self.peek_matches_p('(') {
                            self.parse_col_list()?
                        } else {
                            Vec::new()
                        };
                        def.references = Some((parent, plist));
                        self.parse_fk_tail();
                        continue;
                    }
                    if self.eat_kw("collate") {
                        let _ = self.ident()?;
                        continue;
                    }
                    if self.eat_kw("check") {
                        self.expect_p('(')?;
                        let mut depth = 1usize;
                        while depth > 0 {
                            match self.bump() {
                                Some(Tk::P('(')) => depth += 1,
                                Some(Tk::P(')')) => depth -= 1,
                                Some(_) => {}
                                None => return Err(SqErr::syntax("incomplete input")),
                            }
                        }
                        continue;
                    }
                    break;
                }
                cols.push(def);
            }
            if self.eat_p(',') {
                continue;
            }
            self.expect_p(')')?;
            break;
        }
        Ok(Stmt::Create { name, cols, cons })
    }

    fn parse_drop(&mut self) -> Result<Stmt, SqErr> {
        self.expect_kw("index")?;
        let mut if_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("exists")?;
            if_exists = true;
        }
        let name = self.ident()?;
        Ok(Stmt::DropIndex { name, if_exists })
    }

    /// `ALTER TABLE t ADD [COLUMN] [IF NOT EXISTS] name TYPE [DEFAULT lit]`
    /// `ALTER TABLE t DROP [COLUMN] [IF EXISTS] name`
    /// `ALTER TABLE t RENAME [COLUMN old] TO new`
    fn parse_alter(&mut self) -> Result<Stmt, SqErr> {
        self.expect_kw("table")?;
        let table = self.ident()?;
        if self.eat_kw("add") {
            self.eat_kw("column");
            let mut if_not_exists = false;
            if self.eat_kw("if") {
                self.expect_kw("not")?;
                self.expect_kw("exists")?;
                if_not_exists = true;
            }
            let name = self.ident()?;
            let tname = self.ident()?;
            let ty = match Ty::from_name(&tname) {
                Some(t) => t,
                None => {
                    return Err(SqErr::syntax(&format!(
                        "unknown datatype for {}.{}: \"{}\"",
                        table, name, tname
                    )))
                }
            };
            let mut default = None;
            if self.eat_kw("default") {
                if self.peek().is_none() {
                    return Err(SqErr::syntax("incomplete input"));
                }
                let paren = self.eat_p('(');
                let e = self.parse_expr()?;
                if paren {
                    self.expect_p(')')?;
                }
                default = Some(e);
            }
            return Ok(Stmt::AlterAdd { table, name, ty, default, if_not_exists });
        }
        if self.eat_kw("drop") {
            self.eat_kw("column");
            let mut if_exists = false;
            if self.eat_kw("if") {
                self.expect_kw("exists")?;
                if_exists = true;
            }
            let name = self.ident()?;
            return Ok(Stmt::AlterDrop { table, name, if_exists });
        }
        if self.eat_kw("rename") {
            if self.eat_kw("column") {
                let old = self.ident()?;
                self.expect_kw("to")?;
                let new = self.ident()?;
                return Ok(Stmt::AlterRenameColumn { table, old, new });
            }
            self.expect_kw("to")?;
            let new = self.ident()?;
            return Ok(Stmt::AlterRenameTable { table, new });
        }
        Err(SqErr::syntax("syntax error"))
    }

    fn peek_matches_p(&self, c: char) -> bool {
        matches!(self.peek(), Some(Tk::P(p)) if *p == c)
    }

    /// Parse `( name, name, ... )`, used by table constraints.
    fn parse_col_list(&mut self) -> Result<Vec<String>, SqErr> {
        self.expect_p('(')?;
        let mut list = Vec::new();
        loop {
            if self.eat_p(')') {
                break;
            }
            list.push(self.ident()?);
            if self.eat_p(',') {
                continue;
            }
            self.expect_p(')')?;
            break;
        }
        if list.is_empty() {
            return Err(SqErr::syntax("syntax error"));
        }
        Ok(list)
    }

    /// Consume the optional tail of a foreign-key clause (`ON DELETE ...`,
    /// `MATCH ...`, `DEFERRABLE ...`). Only the syntax is accepted; the
    /// campaign's contract is restrict semantics, which is what enforcement
    /// does regardless of the clause.
    fn parse_fk_tail(&mut self) {
        loop {
            if self.eat_kw("on") {
                let _ = self.eat_kw("delete") || self.eat_kw("update");
                if self.eat_kw("cascade") || self.eat_kw("restrict") {
                    continue;
                }
                if self.eat_kw("set") {
                    let _ = self.eat_kw("null") || self.eat_kw("default");
                    continue;
                }
                let _ = self.eat_kw("no");
                let _ = self.eat_kw("action");
                continue;
            }
            if self.eat_kw("match") {
                let _ = self.ident();
                continue;
            }
            if self.eat_kw("deferrable") {
                if self.eat_kw("initially") {
                    let _ = self.eat_kw("deferred") || self.eat_kw("immediate");
                }
                continue;
            }
            let not_deferrable = matches!(self.peek(), Some(Tk::Ident(s)) if s == "not")
                && matches!(
                    self.toks.get(self.pos + 1).map(|t| &t.tk),
                    Some(Tk::Ident(s)) if s == "deferrable"
                );
            if not_deferrable {
                self.pos += 2;
                if self.eat_kw("initially") {
                    let _ = self.eat_kw("deferred") || self.eat_kw("immediate");
                }
                continue;
            }
            break;
        }
    }

    fn parse_insert(&mut self) -> Result<Stmt, SqErr> {
        self.expect_kw("into")?;
        let table = self.ident()?;
        let mut cols: Option<Vec<String>> = None;
        if self.eat_p('(') {
            let mut list = Vec::new();
            loop {
                if self.eat_p(')') {
                    break;
                }
                let c = self.ident()?;
                list.push(c);
                if self.eat_p(',') {
                    continue;
                }
                self.expect_p(')')?;
                break;
            }
            if list.is_empty() {
                return Err(SqErr::syntax("syntax error"));
            }
            cols = Some(list);
        }
        if self.peek_is_kw("select") {
            self.pos += 1;
            let st = self.parse_select()?;
            let sel = match st {
                Stmt::Select(s) => s,
                _ => return Err(SqErr::syntax("syntax error")),
            };
            return Ok(Stmt::Insert { table, cols, rows: Vec::new(), select: Some(sel) });
        }
        if !self.eat_kw("values") {
            if self.eat_kw("default") {
                self.expect_kw("values")?;
                return Ok(Stmt::Insert {
                    table,
                    cols: Some(Vec::new()),
                    rows: vec![Vec::new()],
                    select: None,
                });
            }
            return Err(SqErr::syntax("near \"INSERT\": syntax error"));
        }
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        loop {
            self.expect_p('(')?;
            let mut vals: Vec<Expr> = Vec::new();
            let mut first = true;
            loop {
                if self.eat_p(')') {
                    if first {
                        return Err(SqErr::syntax("syntax error"));
                    }
                    break;
                }
                first = false;
                let e = self.parse_expr()?;
                vals.push(e);
                if self.eat_p(',') {
                    continue;
                }
                self.expect_p(')')?;
                break;
            }
            rows.push(vals);
            if self.eat_p(',') {
                continue;
            }
            break;
        }
        Ok(Stmt::Insert { table, cols, rows, select: None })
    }

    fn parse_update(&mut self) -> Result<Stmt, SqErr> {
        let table = self.ident()?;
        self.expect_kw("set")?;
        let mut sets: Vec<(String, Expr)> = Vec::new();
        loop {
            if self.peek().is_none() {
                return Err(SqErr::syntax("incomplete input"));
            }
            let col = self.ident()?;
            self.expect_p('=')?;
            let e = self.parse_expr()?;
            sets.push((col, e));
            if self.eat_p(',') {
                continue;
            }
            break;
        }
        let mut filter = None;
        if self.eat_kw("where") {
            filter = Some(self.parse_expr()?);
        }
        Ok(Stmt::Update { table, sets, filter })
    }

    fn parse_delete(&mut self) -> Result<Stmt, SqErr> {
        self.expect_kw("from")?;
        let table = self.ident()?;
        let mut filter = None;
        if self.eat_kw("where") {
            filter = Some(self.parse_expr()?);
        }
        Ok(Stmt::Delete { table, filter })
    }

    fn parse_select(&mut self) -> Result<Stmt, SqErr> {
        let mut sel = Select::default();
        sel.distinct = self.eat_kw("distinct");
        loop {
            let start = self.token_start();
            let qstar = match (
                self.toks.get(self.pos),
                self.toks.get(self.pos + 1),
                self.toks.get(self.pos + 2),
            ) {
                (
                    Some(Token { tk: Tk::Ident(q), .. }),
                    Some(Token { tk: Tk::P('.'), .. }),
                    Some(Token { tk: Tk::P('*'), .. }),
                ) => Some(q.clone()),
                _ => None,
            };
            if let Some(q) = qstar {
                self.pos += 3;
                sel.items.push(SelItem::Star(Some(q)));
            } else if self.eat_p('*') {
                sel.items.push(SelItem::Star(None));
            } else {
                let e = self.parse_expr()?;
                let mut header = self.raw_since(start);
                let mut alias = false;
                if self.eat_kw("as") {
                    let _ = self.ident()?;
                    let t = self.toks[self.pos - 1].clone();
                    header = self.src[t.start..t.end].to_string();
                    alias = true;
                } else if let Some(Tk::Ident(s)) = self.peek() {
                    if !is_reserved(s) {
                        let t = self.toks[self.pos].clone();
                        self.pos += 1;
                        header = self.src[t.start..t.end].to_string();
                        alias = true;
                    }
                }
                sel.items.push(SelItem::Expr { e, header, alias });
            }
            if self.eat_p(',') {
                continue;
            }
            break;
        }
        if sel.items.is_empty() {
            return Err(SqErr::syntax("incomplete input"));
        }
        if self.eat_kw("from") {
            let (name, alias) = self.parse_table_ref()?;
            sel.from = Some(name);
            sel.from_alias = alias;
            loop {
                let mut left = false;
                let is_join = if self.eat_p(',') {
                    true
                } else if self.eat_kw("join") {
                    true
                } else if self.eat_kw("inner") {
                    self.expect_kw("join")?;
                    true
                } else if self.eat_kw("left") {
                    self.eat_kw("outer");
                    self.expect_kw("join")?;
                    left = true;
                    true
                } else if self.eat_kw("cross") {
                    self.expect_kw("join")?;
                    true
                } else {
                    false
                };
                if !is_join {
                    break;
                }
                let (jname, jalias) = self.parse_table_ref()?;
                let mut on = None;
                let mut using = None;
                if self.eat_kw("on") {
                    on = Some(self.parse_expr()?);
                } else if self.eat_kw("using") {
                    self.expect_p('(')?;
                    let mut cols = Vec::new();
                    loop {
                        cols.push(self.ident()?);
                        if self.eat_p(',') {
                            continue;
                        }
                        break;
                    }
                    self.expect_p(')')?;
                    using = Some(cols);
                }
                sel.joins.push(FromItem { name: jname, alias: jalias, on, using, left });
            }
        }
        loop {
            if self.eat_kw("where") {
                if sel.filter.is_some() {
                    return Err(SqErr::syntax("syntax error"));
                }
                sel.filter = Some(self.parse_expr()?);
            } else if self.eat_kw("order") {
                self.expect_kw("by")?;
                loop {
                    let e = self.parse_expr()?;
                    let mut desc = false;
                    if self.eat_kw("desc") {
                        desc = true;
                    } else {
                        self.eat_kw("asc");
                    }
                    sel.order.push((e, desc));
                    if self.eat_p(',') {
                        continue;
                    }
                    break;
                }
            } else if self.eat_kw("group") {
                self.expect_kw("by")?;
                loop {
                    sel.group.push(self.parse_expr()?);
                    if self.eat_p(',') {
                        continue;
                    }
                    break;
                }
            } else if self.eat_kw("having") {
                if sel.having.is_some() {
                    return Err(SqErr::syntax("syntax error"));
                }
                sel.having = Some(self.parse_expr()?);
            } else if self.eat_kw("limit") {
                let a = self.parse_limit_num()?;
                if self.eat_p(',') {
                    let b = self.parse_limit_num()?;
                    sel.offset = a;
                    sel.limit = Some(b);
                } else {
                    sel.limit = Some(a);
                    if self.eat_kw("offset") {
                        sel.offset = self.parse_limit_num()?;
                    }
                }
            } else {
                break;
            }
        }
        if sel.from.is_none() && sel.items.iter().any(|i| matches!(i, SelItem::Star(_))) {
            return Err(SqErr::syntax("no tables specified"));
        }
        Ok(Stmt::Select(Box::new(sel)))
    }

    fn parse_table_ref(&mut self) -> Result<(String, Option<String>), SqErr> {
        let name = self.ident()?;
        let mut alias = None;
        if self.eat_kw("as") {
            alias = Some(self.ident()?);
        } else if let Some(Tk::Ident(s)) = self.peek() {
            if !is_reserved(s) {
                let s = s.clone();
                self.pos += 1;
                alias = Some(s);
            }
        }
        Ok((name, alias))
    }

    fn parse_agg_call(&mut self, kind: AggKind) -> Result<Expr, SqErr> {
        let label = kind.label();
        if self.eat_p(')') {
            return Err(err_arity(format!(
                "wrong number of arguments to function {}()",
                label
            )));
        }
        if let Some(Tk::P('*')) = self.peek() {
            self.pos += 1;
            self.expect_p(')')?;
            if kind == AggKind::Count {
                return Ok(Expr::Agg(kind, None));
            }
            return Err(err_arity(format!(
                "wrong number of arguments to function {}()",
                label
            )));
        }
        let mut args = vec![self.parse_expr()?];
        while self.eat_p(',') {
            args.push(self.parse_expr()?);
        }
        self.expect_p(')')?;
        if args.len() != 1 {
            return Err(err_arity(format!(
                "wrong number of arguments to function {}()",
                label
            )));
        }
        Ok(Expr::Agg(kind, Some(Box::new(args.remove(0)))))
    }

    fn parse_limit_num(&mut self) -> Result<usize, SqErr> {
        match self.bump() {
            Some(Tk::Int(v)) if v >= 0 => Ok(v as usize),
            _ => Err(SqErr::syntax("syntax error")),
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, SqErr> {
        if self.depth > MAX_NEST {
            return Err(limit_err("nesting too deep"));
        }
        self.depth += 1;
        let r = self.parse_or();
        self.depth -= 1;
        r
    }

    fn parse_or(&mut self) -> Result<Expr, SqErr> {
        let mut items = vec![self.parse_and()?];
        while self.eat_kw("or") {
            items.push(self.parse_and()?);
        }
        Ok(fold_balanced(Op::Or, items))
    }

    fn parse_and(&mut self) -> Result<Expr, SqErr> {
        let mut items = vec![self.parse_not()?];
        while self.eat_kw("and") {
            items.push(self.parse_not()?);
        }
        Ok(fold_balanced(Op::And, items))
    }

    fn parse_not(&mut self) -> Result<Expr, SqErr> {
        if self.depth > MAX_NEST {
            return Err(limit_err("nesting too deep"));
        }
        self.depth += 1;
        let r = self.parse_not_inner();
        self.depth -= 1;
        r
    }

    fn parse_not_inner(&mut self) -> Result<Expr, SqErr> {
        if self.eat_kw("not") {
            // Go through parse_not: it is the frame that accounts for the
            // depth budget, so a long NOT chain cannot overflow the stack.
            let e = self.parse_not()?;
            return Ok(Expr::Not(Box::new(e)));
        }
        self.parse_eq()
    }

    // Level 6: = <> != IS [NOT] NULL, [NOT] IN (left associative)
    fn parse_eq(&mut self) -> Result<Expr, SqErr> {
        let mut l = self.parse_rel()?;
        loop {
            let op = match self.peek() {
                Some(Tk::P('=')) => Some(Op::Eq),
                Some(Tk::P('!')) => Some(Op::Ne),
                _ => None,
            };
            if let Some(op) = op {
                self.pos += 1;
                let r = self.parse_rel()?;
                l = Expr::Bin(op, Box::new(l), Box::new(r));
                continue;
            }
            if self.eat_kw("is") {
                let neg = self.eat_kw("not");
                self.expect_kw("null")?;
                l = Expr::IsNull(Box::new(l), neg);
                continue;
            }
            let neg = if self.peek_is_kw("not") && self.tok_is_kw(self.pos + 1, "in") {
                self.pos += 2;
                true
            } else if self.eat_kw("in") {
                false
            } else {
                break;
            };
            l = self.parse_in_rhs(l, neg)?;
        }
        Ok(l)
    }

    /// Parse the right-hand side of `[NOT] IN`, with `IN` already consumed.
    fn parse_in_rhs(&mut self, l: Expr, neg: bool) -> Result<Expr, SqErr> {
        self.expect_p('(')?;
        if self.peek_is_kw("select") {
            self.pos += 1;
            let st = self.parse_select()?;
            let sel = match st {
                Stmt::Select(s) => s,
                _ => return Err(SqErr::syntax("syntax error")),
            };
            self.expect_p(')')?;
            return Ok(Expr::InSub(Box::new(l), sel, neg));
        }
        let mut list: Vec<Expr> = Vec::new();
        if !self.eat_p(')') {
            loop {
                list.push(self.parse_expr()?);
                if self.eat_p(',') {
                    continue;
                }
                self.expect_p(')')?;
                break;
            }
        }
        Ok(Expr::InList(Box::new(l), list, neg))
    }

    // Level 5: < <= > >= (left associative)
    fn parse_rel(&mut self) -> Result<Expr, SqErr> {
        let first = self.parse_add()?;
        let mut chain: Vec<(Op, Expr)> = Vec::new();
        loop {
            let op = match self.peek() {
                Some(Tk::P('<')) => Op::Lt,
                Some(Tk::P('L')) => Op::Le,
                Some(Tk::P('>')) => Op::Gt,
                Some(Tk::P('G')) => Op::Ge,
                _ => break,
            };
            self.pos += 1;
            chain.push((op, self.parse_add()?));
        }
        fold_left(first, chain)
    }

    fn parse_add(&mut self) -> Result<Expr, SqErr> {
        let first = self.parse_mul()?;
        let mut chain: Vec<(Op, Expr)> = Vec::new();
        loop {
            let op = if self.eat_p('+') {
                Op::Add
            } else if self.eat_p('-') {
                Op::Sub
            } else {
                break;
            };
            chain.push((op, self.parse_mul()?));
        }
        fold_left(first, chain)
    }

    fn parse_mul(&mut self) -> Result<Expr, SqErr> {
        let first = self.parse_concat()?;
        let mut chain: Vec<(Op, Expr)> = Vec::new();
        loop {
            let op = if self.eat_p('*') {
                Op::Mul
            } else if self.eat_p('/') {
                Op::Div
            } else if self.eat_p('%') {
                Op::Mod
            } else {
                break;
            };
            chain.push((op, self.parse_concat()?));
        }
        fold_left(first, chain)
    }

    // Level 2: || (tighter than * / % per the spec, left associative)
    fn parse_concat(&mut self) -> Result<Expr, SqErr> {
        let first = self.parse_unary()?;
        let mut chain: Vec<(Op, Expr)> = Vec::new();
        loop {
            if !self.eat_p('|') {
                break;
            }
            chain.push((Op::Concat, self.parse_unary()?));
        }
        fold_left(first, chain)
    }

    fn parse_unary(&mut self) -> Result<Expr, SqErr> {
        if self.depth > MAX_NEST {
            return Err(limit_err("nesting too deep"));
        }
        self.depth += 1;
        let r = self.parse_unary_inner();
        self.depth -= 1;
        r
    }

    fn parse_unary_inner(&mut self) -> Result<Expr, SqErr> {
        if self.eat_p('-') {
            let e = self.parse_unary()?;
            return Ok(Expr::Neg(Box::new(e)));
        }
        if self.eat_p('+') {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, SqErr> {
        match self.bump() {
            Some(Tk::Int(v)) => Ok(Expr::Lit(Value::Int(v))),
            Some(Tk::Real(v)) => Ok(Expr::Lit(Value::Real(v))),
            Some(Tk::Str(s)) => Ok(Expr::Lit(Value::Text(s))),
            Some(Tk::P('(')) => {
                if self.peek_is_kw("select") {
                    self.pos += 1;
                    let st = self.parse_select()?;
                    let sel = match st {
                        Stmt::Select(s) => s,
                        _ => return Err(SqErr::syntax("syntax error")),
                    };
                    self.expect_p(')')?;
                    return Ok(Expr::Subquery(sel));
                }
                let e = self.parse_expr()?;
                self.expect_p(')')?;
                Ok(e)
            }
            Some(Tk::Ident(s)) => match s.as_str() {
                "null" => Ok(Expr::Lit(Value::Null)),
                "true" => Ok(Expr::Lit(Value::Int(1))),
                "false" => Ok(Expr::Lit(Value::Int(0))),
                "exists" if self.peek_matches_p('(') => {
                    self.pos += 1;
                    if !self.peek_is_kw("select") {
                        return Err(SqErr::syntax("syntax error"));
                    }
                    self.pos += 1;
                    let st = self.parse_select()?;
                    let sel = match st {
                        Stmt::Select(b) => b,
                        _ => return Err(SqErr::syntax("syntax error")),
                    };
                    self.expect_p(')')?;
                    Ok(Expr::Exists(sel))
                }
                "from" | "where" | "order" | "group" | "having" | "limit" | "offset"
                | "union" | "into" | "values" | "as" | "by" | "table" | "create" | "distinct"
                | "insert" | "select" | "join" | "on" | "using" | "asc" | "desc" | "outer"
                | "cross" | "inner" | "left" => {
                    Err(SqErr::syntax("syntax error"))
                }
                _ => {
                    if let Some(Tk::P('(')) = self.peek() {
                        self.pos += 1;
                        let agg = match s.as_str() {
                            "count" => Some(AggKind::Count),
                            "sum" => Some(AggKind::Sum),
                            "min" => Some(AggKind::Min),
                            "max" => Some(AggKind::Max),
                            _ => None,
                        };
                        if let Some(kind) = agg {
                            return self.parse_agg_call(kind);
                        }
                        let mut args: Vec<Expr> = Vec::new();
                        if !self.eat_p(')') {
                            loop {
                                args.push(self.parse_expr()?);
                                if self.eat_p(',') {
                                    continue;
                                }
                                self.expect_p(')')?;
                                break;
                            }
                        }
                        return Ok(Expr::Fn(s, args));
                    }
                    if let Some(Tk::P('.')) = self.peek() {
                        self.pos += 1;
                        let name = self.ident()?;
                        Ok(Expr::Col { qual: Some(s), name })
                    } else {
                        Ok(Expr::Col { qual: None, name: s })
                    }
                }
            },
            _ => Err(SqErr::syntax("syntax error")),
        }
    }
}

fn fold_balanced(op: Op, mut items: Vec<Expr>) -> Expr {
    if items.len() == 1 {
        return items.remove(0);
    }
    let right = items.split_off(items.len() / 2);
    let l = fold_balanced(op, items);
    let r = fold_balanced(op, right);
    Expr::Bin(op, Box::new(l), Box::new(r))
}

fn fold_left(first: Expr, chain: Vec<(Op, Expr)>) -> Result<Expr, SqErr> {
    if chain.is_empty() {
        return Ok(first);
    }
    if chain.len() > 4096 {
        return Err(SqErr::syntax("expression too complex"));
    }
    let mut e = first;
    for (op, rhs) in chain {
        e = Expr::Bin(op, Box::new(e), Box::new(rhs));
    }
    Ok(e)
}

fn is_reserved(s: &str) -> bool {
    matches!(
        s,
        "from"
            | "where"
            | "order"
            | "distinct"
            | "group"
            | "having"
            | "limit"
            | "offset"
            | "union"
            | "and"
            | "or"
            | "not"
            | "is"
            | "null"
            | "asc"
            | "desc"
            | "join"
            | "on"
            | "as"
            | "values"
            | "into"
            | "select"
            | "insert"
            | "create"
            | "table"
            | "update"
            | "delete"
            | "by"
            | "set"
            | "using"
            | "left"
            | "inner"
            | "outer"
            | "cross"
    )
}

// ---------------------------------------------------------------- engine ---

#[derive(Clone, Debug)]
struct Column {
    name: String,
    lower: String,
    ty: Ty,
    /// Qualifier (table name or alias) for columns in a joined row space.
    q: Option<String>,
    /// Rejects NULL values.
    notnull: bool,
    /// Value used when the column is omitted from an INSERT column list.
    def: Option<Value>,
}

/// A foreign key: child columns in this table refer to parent columns of
/// another (or the same) table. Column names are resolved at DML time so a
/// table may be created before its parent.
#[derive(Clone, Debug)]
struct Fk {
    cols: Vec<usize>,
    parent: String,
    pcols: Vec<String>,
}

/// A named secondary index created by `CREATE [UNIQUE] INDEX`.
#[derive(Clone, Debug)]
struct IndexDef {
    name: String,
    lower: String,
    col: usize,
    unique: bool,
}

/// Constraint metadata for a table: primary key, unique constraints and
/// foreign keys. Keeping it in one struct means join row-spaces can simply
/// carry an empty default.
#[derive(Clone, Debug, Default)]
struct Schema {
    /// Primary key columns (empty when the table has none).
    pk: Vec<usize>,
    /// Unique constraints other than the primary key.
    uniq: Vec<Vec<usize>>,
    fks: Vec<Fk>,
    /// Explicitly created named indexes.
    idx_defs: Vec<IndexDef>,
    /// The primary key is a single `INTEGER` column and therefore a rowid
    /// alias: NULL/omitted means "assign max + 1".
    pk_auto: bool,
    /// Cached max value of a single INTEGER PRIMARY KEY (rowid alias), so
    /// autocommit inserts do not rescan the table.
    pk_max: i64,
    pk_max_ok: bool,
}

impl Schema {
    /// True when this table's primary key is a single INTEGER column, i.e. a
    /// SQLite rowid alias: NULL means "assign max + 1".
    fn rowid_pk(&self) -> Option<usize> {
        if self.pk_auto {
            self.pk.first().copied()
        } else {
            None
        }
    }
}

/// A physical secondary index over one column: keys map to row positions and
/// NULL values are kept apart (they are never equal to anything).
#[derive(Clone, Debug, Default)]
struct PhysIndex {
    map: BTreeMap<Key, Vec<u32>>,
    nulls: Vec<u32>,
}

// ------------------------------------------------------------ row store ---
//
// Tables keep their rows either fully in memory (small tables: the fast path
// every earlier ticket uses) or in an append-only spill file under the data
// directory.  A spill file holds length-prefixed binary rows; the position of
// every row is kept in two compact vectors (8 + 4 bytes per row), so memory
// stays proportional to the row *count* rather than to the row *bytes*.  The
// file is only a read cache derived from the write-ahead log, which remains
// the durable source of truth, so a restart simply rebuilds it.

static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);
static SPILL_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Approximate byte budget for in-memory rows; computed from the cgroup limit
/// at startup so a 64 MiB container spills early while a 2 GiB one keeps
/// ordinary tables resident.
static MEM_BUDGET: AtomicU64 = AtomicU64::new(24 * 1024 * 1024);

fn set_mem_budget(limit: Option<u64>) {
    if let Ok(v) = env::var("SDB_MEM_BUDGET") {
        if let Ok(n) = v.parse::<u64>() {
            MEM_BUDGET.store(n.max(256 * 1024), AtomicOrdering::Relaxed);
            return;
        }
    }
    let b = match limit {
        // Leave most of a tight container budget for transient work (a
        // multi-megabyte INSERT, page cache) but let a roomy one keep whole
        // tables resident as before.
        Some(l) => (l / 6).max(2 * 1024 * 1024).min(256 * 1024 * 1024),
        None => 32 * 1024 * 1024,
    };
    MEM_BUDGET.store(b, AtomicOrdering::Relaxed);
}

/// Read the container's memory limit, if the cgroup exposes one.
fn cgroup_limit() -> Option<u64> {
    let read = |p: &str| -> Option<u64> {
        fs::read_to_string(p).ok().and_then(|s| {
            let t = s.trim();
            if t == "max" {
                None
            } else {
                t.parse::<u64>().ok().filter(|v| *v > 0 && *v < (1u64 << 60))
            }
        })
    };
    read("/sys/fs/cgroup/memory.max")
        .or_else(|| read("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
}

fn mem_budget() -> usize {
    MEM_BUDGET.load(AtomicOrdering::Relaxed) as usize
}

/// Return free heap pages to the OS.  Long loads allocate and free many
/// large row buffers; glibc keeps those pages in the arena otherwise, which
/// matters a lot under a hard memory cap.
fn trim_memory() {
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe {
        malloc_trim(0);
    }
}

fn spill_dir() -> Option<PathBuf> {
    match SPILL_DIR.lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

fn set_spill_dir(p: PathBuf) {
    // A previous process's spill files are worthless: every row is rebuilt
    // from the log on startup.
    let _ = fs::remove_dir_all(&p);
    let _ = fs::create_dir_all(&p);
    match SPILL_DIR.lock() {
        Ok(mut g) => *g = Some(p),
        Err(poisoned) => *poisoned.into_inner() = Some(p),
    }
}

const ROW_NULL: u8 = 0;
const ROW_INT: u8 = 1;
const ROW_REAL: u8 = 2;
const ROW_TEXT: u8 = 3;

fn row_bytes(row: &[Value]) -> usize {
    let mut n = 24 + row.len() * std::mem::size_of::<Value>();
    for v in row {
        if let Value::Text(s) = v {
            n += s.len();
        }
    }
    n
}

fn enc_row(row: &[Value], out: &mut Vec<u8>) {
    let len_pos = out.len();
    out.extend_from_slice(&[0u8; 4]);
    let start = out.len();
    for v in row {
        match v {
            Value::Null => out.push(ROW_NULL),
            Value::Int(i) => {
                out.push(ROW_INT);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Real(f) => {
                out.push(ROW_REAL);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Text(s) => {
                out.push(ROW_TEXT);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    let n = (out.len() - start) as u32;
    out[len_pos..len_pos + 4].copy_from_slice(&n.to_le_bytes());
}

fn dec_row(buf: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        match buf[i] {
            ROW_NULL => {
                out.push(Value::Null);
                i += 1;
            }
            ROW_INT if i + 9 <= buf.len() => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[i + 1..i + 9]);
                out.push(Value::Int(i64::from_le_bytes(b)));
                i += 9;
            }
            ROW_REAL if i + 9 <= buf.len() => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[i + 1..i + 9]);
                out.push(Value::Real(f64::from_le_bytes(b)));
                i += 9;
            }
            ROW_TEXT if i + 5 <= buf.len() => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&buf[i + 1..i + 5]);
                let n = u32::from_le_bytes(b) as usize;
                if i + 5 + n > buf.len() {
                    break;
                }
                out.push(Value::Text(String::from_utf8_lossy(&buf[i + 5..i + 5 + n]).into_owned()));
                i += 5 + n;
            }
            _ => break,
        }
    }
    out
}

#[derive(Default)]
struct DiskCache {
    off: u64,
    data: Vec<u8>,
    last: usize,
}

struct DiskRows {
    file: File,
    offs: Vec<u64>,
    lens: Vec<u32>,
    /// End of the file as this image sees it (bytes appended after that by a
    /// rolled-back image are simply unreferenced).
    end: u64,
    cache: RefCell<DiskCache>,
}

impl std::fmt::Debug for DiskRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DiskRows({} rows, {} bytes)", self.offs.len(), self.end)
    }
}

impl Clone for DiskRows {
    fn clone(&self) -> Self {
        let end = self.file.metadata().map(|m| m.len()).unwrap_or(self.end);
        DiskRows {
            file: self.file.try_clone().expect("clone spill file"),
            offs: self.offs.clone(),
            lens: self.lens.clone(),
            end,
            cache: RefCell::new(DiskCache::default()),
        }
    }
}

fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], off + done as u64) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read")),
            Ok(n) => done += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_all_at(f: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        match f.write_at(&buf[done..], off + done as u64) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "write zero")),
            Ok(n) => done += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

const SCAN_BLOCK: usize = 256 * 1024;

impl DiskRows {
    fn get(&self, i: usize) -> Vec<Value> {
        let off = self.offs[i];
        let len = self.lens[i] as usize;
        {
            let c = self.cache.borrow();
            if !c.data.is_empty() && off >= c.off && off + len as u64 <= c.off + c.data.len() as u64 {
                let s = (off - c.off) as usize;
                return dec_row(&c.data[s + 4..s + len]);
            }
        }
        let mut c = self.cache.borrow_mut();
        // Sequential access fills a block; a random probe reads one row.
        if i == c.last + 1 && !c.data.is_empty() {
            let want = SCAN_BLOCK.min((self.end.saturating_sub(off)) as usize);
            let mut buf = vec![0u8; want.max(len)];
            if read_exact_at(&self.file, &mut buf, off).is_ok() {
                c.off = off;
                c.data = buf;
                c.last = i;
                return dec_row(&c.data[4..len]);
            }
        }
        let mut buf = vec![0u8; len];
        if read_exact_at(&self.file, &mut buf, off).is_err() {
            return Vec::new();
        }
        c.last = i;
        dec_row(&buf[4..])
    }

    fn append(&mut self, row: &[Value]) {
        let mut buf = Vec::with_capacity(64);
        enc_row(row, &mut buf);
        if write_all_at(&self.file, &buf, self.end).is_ok() {
            self.offs.push(self.end);
            self.lens.push(buf.len() as u32);
            self.end += buf.len() as u64;
        }
    }

    fn set(&mut self, i: usize, row: &[Value]) {
        let mut buf = Vec::with_capacity(64);
        enc_row(row, &mut buf);
        if write_all_at(&self.file, &buf, self.end).is_ok() {
            self.offs[i] = self.end;
            self.lens[i] = buf.len() as u32;
            self.end += buf.len() as u64;
        }
    }
}

/// A borrowed or materialised row: memory rows are borrowed in place, spill
/// rows are decoded into an owned vector.
enum RowRef<'a> {
    Borrowed(&'a [Value]),
    Owned(Vec<Value>),
}

impl<'a> std::ops::Deref for RowRef<'a> {
    type Target = [Value];
    fn deref(&self) -> &[Value] {
        match self {
            RowRef::Borrowed(s) => s,
            RowRef::Owned(v) => v,
        }
    }
}

struct RowSet {
    mem: Vec<Vec<Value>>,
    disk: Option<DiskRows>,
    bytes: usize,
}

impl std::fmt::Debug for RowSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RowSet({} rows)", self.len())
    }
}

impl Clone for RowSet {
    fn clone(&self) -> Self {
        match &self.disk {
            Some(d) => RowSet { mem: Vec::new(), disk: Some(d.clone()), bytes: 0 },
            None => RowSet { mem: self.mem.clone(), disk: None, bytes: self.bytes },
        }
    }
}

impl Default for RowSet {
    fn default() -> RowSet {
        RowSet::new()
    }
}

impl RowSet {
    fn new() -> RowSet {
        RowSet { mem: Vec::new(), disk: None, bytes: 0 }
    }
    fn from_rows(rows: Vec<Vec<Value>>) -> RowSet {
        let bytes = rows.iter().map(|r| row_bytes(r)).sum();
        RowSet { mem: rows, disk: None, bytes }
    }
    fn len(&self) -> usize {
        match &self.disk {
            Some(d) => d.offs.len(),
            None => self.mem.len(),
        }
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn get(&self, i: usize) -> RowRef<'_> {
        match &self.disk {
            Some(d) => RowRef::Owned(d.get(i)),
            None => RowRef::Borrowed(&self.mem[i]),
        }
    }
    fn disk_mode(&self) -> bool {
        self.disk.is_some()
    }
    fn push(&mut self, row: Vec<Value>) {
        if let Some(d) = &mut self.disk {
            d.append(&row);
            return;
        }
        self.bytes += row_bytes(&row);
        self.mem.push(row);
        if self.bytes > mem_budget() {
            self.spill();
        }
    }
    fn set(&mut self, i: usize, row: Vec<Value>) {
        if let Some(d) = &mut self.disk {
            d.set(i, &row);
            return;
        }
        self.bytes = self.bytes + row_bytes(&row) - row_bytes(&self.mem[i]);
        self.mem[i] = row;
    }
    fn truncate(&mut self, n: usize) {
        match &mut self.disk {
            Some(d) => {
                if n < d.offs.len() {
                    d.offs.truncate(n);
                    d.lens.truncate(n);
                }
            }
            None => {
                if n < self.mem.len() {
                    for r in &self.mem[n..] {
                        self.bytes = self.bytes.saturating_sub(row_bytes(r));
                    }
                    self.mem.truncate(n);
                }
            }
        }
    }
    fn clear(&mut self) {
        self.mem = Vec::new();
        self.disk = None;
        self.bytes = 0;
    }
    /// Keep only the rows for which `f` returns true. `pred` also receives the
    /// row index (callers use it to consult external flags).
    fn retain<F: FnMut(usize, &[Value]) -> bool>(&mut self, mut f: F) {
        match &mut self.disk {
            Some(d) => {
                let n = d.offs.len();
                let mut no: Vec<u64> = Vec::with_capacity(n);
                let mut nl: Vec<u32> = Vec::with_capacity(n);
                for i in 0..n {
                    let row = d.get(i);
                    if f(i, &row) {
                        no.push(d.offs[i]);
                        nl.push(d.lens[i]);
                    }
                }
                d.offs = no;
                d.lens = nl;
            }
            None => {
                let old = std::mem::take(&mut self.mem);
                let mut keep = Vec::with_capacity(old.len());
                self.bytes = 0;
                for (i, row) in old.into_iter().enumerate() {
                    if f(i, &row) {
                        self.bytes += row_bytes(&row);
                        keep.push(row);
                    }
                }
                self.mem = keep;
            }
        }
    }
    fn spill(&mut self) {
        if self.disk.is_some() || self.mem.is_empty() {
            return;
        }
        let dir = match spill_dir() {
            Some(d) => d,
            None => return,
        };
        let seq = SPILL_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let path = dir.join(format!("spill_{}_{}.tbl", std::process::id(), seq));
        let file = match OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path) {
            Ok(f) => f,
            Err(_) => return,
        };
        // The file is only ever reached through this handle, so its directory
        // entry is not needed: unlink it now and the space is reclaimed as
        // soon as the last clone drops, even for a statement that was
        // cancelled half-way through a huge join.
        let _ = fs::remove_file(&path);
        let mut d = DiskRows { file, offs: Vec::with_capacity(self.mem.len()), lens: Vec::with_capacity(self.mem.len()), end: 0, cache: RefCell::new(DiskCache::default()) };
        let mut buf: Vec<u8> = Vec::with_capacity(SCAN_BLOCK);
        for row in &self.mem {
            let start = buf.len();
            enc_row(row, &mut buf);
            d.offs.push(d.end + start as u64);
            d.lens.push((buf.len() - start) as u32);
            if buf.len() >= SCAN_BLOCK {
                let _ = write_all_at(&d.file, &buf, d.end);
                d.end += buf.len() as u64;
                buf.clear();
            }
        }
        if !buf.is_empty() {
            let _ = write_all_at(&d.file, &buf, d.end);
            d.end += buf.len() as u64;
        }
        self.mem = Vec::new();
        self.bytes = 0;
        self.disk = Some(d);
        trim_memory();
    }
    fn push_to_all(&mut self, v: Value) {
        if let Some(d) = &mut self.disk {
            for i in 0..d.offs.len() {
                let mut row = d.get(i);
                row.push(v.clone());
                d.set(i, &row);
            }
        } else {
            for row in &mut self.mem {
                row.push(v.clone());
            }
            self.bytes += self.mem.len() * std::mem::size_of::<Value>();
        }
    }
    fn drop_value_from_all(&mut self, ci: usize) {
        if let Some(d) = &mut self.disk {
            for i in 0..d.offs.len() {
                let mut row = d.get(i);
                if ci < row.len() {
                    row.remove(ci);
                }
                d.set(i, &row);
            }
        } else {
            for row in &mut self.mem {
                if ci < row.len() {
                    row.remove(ci);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Table {
    name: String,
    lower: String,
    /// Alias used by the current query, if any. It hides the table name.
    alias: Option<String>,
    cols: Vec<Column>,
    rows: RowSet,
    idx: HashMap<usize, PhysIndex>,
    sch: Schema,
}

impl Table {
    fn col_index(&self, name: &str) -> Option<usize> {
        let l = name.to_lowercase();
        self.cols.iter().position(|c| c.lower == l)
    }

    /// True when `q` names this row space (the alias for a plain table, or
    /// one of the merged qualifiers for a join).
    fn has_qualifier(&self, q: &str) -> bool {
        let lq = q.to_lowercase();
        if self.cols.iter().any(|c| c.q.is_some()) {
            self.cols.iter().any(|c| c.q.as_deref() == Some(lq.as_str()))
        } else {
            self.alias.as_deref().unwrap_or(self.lower.as_str()) == lq
        }
    }

    /// Resolve a column reference, honoring an optional table qualifier and
    /// reporting ambiguity when an unqualified name matches several columns.
    fn col_lookup(&self, qual: Option<&str>, name: &str) -> Result<Option<usize>, SqErr> {
        let l = name.to_lowercase();
        if let Some(q) = qual {
            let lq = q.to_lowercase();
            if self.cols.iter().any(|c| c.q.is_some()) {
                for (i, c) in self.cols.iter().enumerate() {
                    if c.lower == l && c.q.as_deref() == Some(lq.as_str()) {
                        return Ok(Some(i));
                    }
                }
                return Ok(None);
            }
            // A plain table is qualified by its alias, or by its name when
            // no alias was given.
            let allowed = self.alias.as_deref().unwrap_or(self.lower.as_str());
            if allowed != lq {
                return Ok(None);
            }
            return Ok(self.cols.iter().position(|c| c.lower == l));
        }
        let mut found: Option<usize> = None;
        let mut count = 0usize;
        for (i, c) in self.cols.iter().enumerate() {
            if c.lower == l {
                found = Some(i);
                count += 1;
            }
        }
        if count > 1 {
            return Err(SqErr::new("ambiguous", format!("ambiguous column name: {}", name)));
        }
        Ok(found)
    }
}

fn coerce(v: Value, ty: Ty, tname: &str, cname: &str) -> Result<Value, SqErr> {
    match ty {
        Ty::Int => match v {
            Value::Null | Value::Int(_) => Ok(v),
            Value::Real(f) => {
                if f.is_finite()
                    && f.fract() == 0.0
                    && f >= -9223372036854775808.0
                    && f < 9223372036854775808.0
                {
                    Ok(Value::Int(f as i64))
                } else {
                    Err(err_type(format!("cannot store REAL value in INTEGER column {}.{}", tname, cname)))
                }
            }
            Value::Text(s) => {
                let t = s.trim();
                if let Ok(i) = t.parse::<i64>() {
                    return Ok(Value::Int(i));
                }
                if let Ok(f) = t.parse::<f64>() {
                    if f.is_finite()
                        && f.fract() == 0.0
                        && f >= -9223372036854775808.0
                        && f < 9223372036854775808.0
                    {
                        return Ok(Value::Int(f as i64));
                    }
                    return Err(err_type(format!(
                        "cannot store REAL value in INTEGER column {}.{}",
                        tname, cname
                    )));
                }
                Err(err_type(format!("cannot store TEXT value in INTEGER column {}.{}", tname, cname)))
            }
        },
        Ty::Text => match v {
            Value::Null | Value::Text(_) => Ok(v),
            Value::Int(i) => Ok(Value::Text(i.to_string())),
            Value::Real(f) => Ok(Value::Text(fmt_real(f))),
        },
    }
}

fn affinity(v: Value, ty: Ty) -> Value {
    match ty {
        Ty::Int => match v {
            Value::Text(s) => {
                let t = s.trim();
                if let Ok(i) = t.parse::<i64>() {
                    Value::Int(i)
                } else if let Ok(f) = t.parse::<f64>() {
                    if f.is_finite() && f.fract() == 0.0 && f >= -9223372036854775808.0 && f < 9223372036854775808.0
                    {
                        Value::Int(f as i64)
                    } else {
                        Value::Real(f)
                    }
                } else {
                    Value::Text(s)
                }
            }
            other => other,
        },
        Ty::Text => match v {
            Value::Int(i) => Value::Text(i.to_string()),
            Value::Real(f) => Value::Text(fmt_real(f)),
            other => other,
        },
    }
}

// resolved expression
#[derive(Clone)]
enum RExpr {
    Lit(Value),
    Col(usize),
    /// A column of an enclosing query: (levels up, column index in that
    /// scope's row space).
    Outer(usize, usize),
    Neg(Box<RExpr>),
    Not(Box<RExpr>),
    Bin(Op, Box<RExpr>, Box<RExpr>),
    IsNull(Box<RExpr>, bool),
    Fn(String, Vec<RExpr>),
    Agg(usize),
    /// Scalar subquery.
    Sub(std::rc::Rc<SubPlan>),
    /// `EXISTS (subquery)`.
    Exists(std::rc::Rc<SubPlan>),
    /// `expr [NOT] IN (list)`.
    InList(Box<RExpr>, Vec<RExpr>, bool),
    /// `expr [NOT] IN (subquery)`.
    InSub(Box<RExpr>, std::rc::Rc<SubPlan>, bool),
}

#[derive(Clone)]
struct AggSpec {
    kind: AggKind,
    arg: Option<RExpr>,
}

fn resolve_expr(e: &Expr, t: &Table, db: &Db, outers: &[&Table]) -> Result<RExpr, SqErr> {
    let mut aggs: Vec<AggSpec> = Vec::new();
    resolve_expr_ag(e, t, db, outers, &mut aggs, false)
}

fn resolve_expr_ag(
    e: &Expr,
    t: &Table,
    db: &Db,
    outers: &[&Table],
    aggs: &mut Vec<AggSpec>,
    allow_agg: bool,
) -> Result<RExpr, SqErr> {
    resolve_expr_alias(e, t, db, outers, aggs, allow_agg, None)
}

fn unresolved_column(qual: &Option<String>, name: &str) -> SqErr {
    SqErr::new(
        "no_column",
        match qual {
            Some(q) => format!("no such column: {}.{}", q, name),
            None => format!("no such column: {}", name),
        },
    )
}

fn resolve_expr_alias(
    e: &Expr,
    t: &Table,
    db: &Db,
    outers: &[&Table],
    aggs: &mut Vec<AggSpec>,
    allow_agg: bool,
    aliases: Option<&HashMap<String, RExpr>>,
) -> Result<RExpr, SqErr> {
    match e {
        Expr::Lit(v) => Ok(RExpr::Lit(v.clone())),
        Expr::Col { qual, name } => {
            match t.col_lookup(qual.as_deref(), name)? {
                Some(i) => Ok(RExpr::Col(i)),
                None => {
                    // Result aliases are only a fallback for names that do
                    // not exist among the source columns (HAVING).
                    if qual.is_none() {
                        if let Some(map) = aliases {
                            if let Some(r) = map.get(&name.to_lowercase()) {
                                return Ok(r.clone());
                            }
                        }
                    }
                    // A qualifier that names the local row space stops
                    // here: the column is simply missing from it.
                    if let Some(q) = qual {
                        if t.has_qualifier(q) {
                            return Err(unresolved_column(qual, name));
                        }
                    }
                    // Correlated reference: search the enclosing scopes,
                    // innermost first.
                    for (lvl, ot) in outers.iter().enumerate() {
                        match ot.col_lookup(qual.as_deref(), name)? {
                            Some(i) => return Ok(RExpr::Outer(lvl, i)),
                            None => {
                                if let Some(q) = qual {
                                    if ot.has_qualifier(q) {
                                        return Err(unresolved_column(qual, name));
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    Err(unresolved_column(qual, name))
                }
            }
        }
        Expr::Neg(x) => Ok(RExpr::Neg(Box::new(resolve_expr_alias(
            x, t, db, outers, aggs, allow_agg, aliases,
        )?))),
        Expr::Not(x) => Ok(RExpr::Not(Box::new(resolve_expr_alias(
            x, t, db, outers, aggs, allow_agg, aliases,
        )?))),
        Expr::IsNull(x, n) => Ok(RExpr::IsNull(
            Box::new(resolve_expr_alias(x, t, db, outers, aggs, allow_agg, aliases)?),
            *n,
        )),
        Expr::Bin(op, a, b) => Ok(RExpr::Bin(
            *op,
            Box::new(resolve_expr_alias(a, t, db, outers, aggs, allow_agg, aliases)?),
            Box::new(resolve_expr_alias(b, t, db, outers, aggs, allow_agg, aliases)?),
        )),
        Expr::Agg(kind, arg) => {
            // Resolve the argument first: an unknown column inside the
            // aggregate is reported before "misuse of aggregate", matching
            // the reference.
            let ra = match arg {
                Some(a) => Some(resolve_expr_alias(a, t, db, outers, aggs, false, None)?),
                None => None,
            };
            if !allow_agg {
                return Err(SqErr::syntax("misuse of aggregate function"));
            }
            aggs.push(AggSpec { kind: *kind, arg: ra });
            Ok(RExpr::Agg(aggs.len() - 1))
        }
        Expr::Fn(name, args) => {
            let ra: Vec<RExpr> = args
                .iter()
                .map(|a| resolve_expr_alias(a, t, db, outers, aggs, allow_agg, aliases))
                .collect::<Result<_, _>>()?;
            match name.as_str() {
                "length" | "upper" | "lower" | "abs" => {
                    if ra.len() != 1 {
                        return Err(err_arity(format!(
                            "wrong number of arguments to function {}()",
                            name
                        )));
                    }
                }
                "coalesce" => {
                    if ra.len() < 2 {
                        return Err(err_arity(format!(
                            "wrong number of arguments to function {}()",
                            name
                        )));
                    }
                }
                // Small compatibility surface for ordinary PostgreSQL
                // clients (psql probes set_config/version at startup).
                "set_config" => {
                    if ra.len() != 3 {
                        return Err(err_arity(format!(
                            "wrong number of arguments to function {}()",
                            name
                        )));
                    }
                }
                "current_setting" | "quote_ident" | "quote_literal" | "pg_get_userbyid"
                | "current_schemas" | "pg_encoding_to_char" => {
                    if ra.len() != 1 {
                        return Err(err_arity(format!(
                            "wrong number of arguments to function {}()",
                            name
                        )));
                    }
                }
                "format_type" => {
                    if ra.is_empty() || ra.len() > 2 {
                        return Err(err_arity(format!(
                            "wrong number of arguments to function {}()",
                            name
                        )));
                    }
                }
                "version" | "current_schema" | "current_database" | "current_user"
                | "session_user" | "user" | "pg_backend_pid" | "pg_is_in_recovery" => {
                    if !ra.is_empty() {
                        return Err(err_arity(format!(
                            "wrong number of arguments to function {}()",
                            name
                        )));
                    }
                }
                _ => {
                    return Err(SqErr::new("syntax", format!("no such function: {}", name)));
                }
            }
            Ok(RExpr::Fn(name.clone(), ra))
        }
        Expr::Subquery(sel) => {
            let plan = build_subplan(sel, db, t, outers)?;
            if plan.ncols != 1 {
                return Err(err_arity(format!(
                    "sub-select returns {} columns - expected 1",
                    plan.ncols
                )));
            }
            Ok(RExpr::Sub(std::rc::Rc::new(plan)))
        }
        Expr::Exists(sel) => {
            let plan = build_subplan(sel, db, t, outers)?;
            Ok(RExpr::Exists(std::rc::Rc::new(plan)))
        }
        Expr::InList(l, items, neg) => {
            let rl = resolve_expr_alias(l, t, db, outers, aggs, allow_agg, aliases)?;
            let mut ritems = Vec::with_capacity(items.len());
            for it in items {
                ritems.push(resolve_expr_alias(it, t, db, outers, aggs, allow_agg, aliases)?);
            }
            Ok(RExpr::InList(Box::new(rl), ritems, *neg))
        }
        Expr::InSub(l, sel, neg) => {
            let rl = resolve_expr_alias(l, t, db, outers, aggs, allow_agg, aliases)?;
            let plan = build_subplan(sel, db, t, outers)?;
            if plan.ncols != 1 {
                return Err(err_arity(format!(
                    "sub-select returns {} columns - expected 1",
                    plan.ncols
                )));
            }
            Ok(RExpr::InSub(Box::new(rl), std::rc::Rc::new(plan), *neg))
        }
    }
}

fn neg_value(v: Value) -> Value {
    match v {
        Value::Int(i) => Value::Int(i.wrapping_neg()),
        Value::Real(f) => {
            if f == 9223372036854775808.0 {
                Value::Int(i64::MIN)
            } else {
                Value::Real(-f)
            }
        }
        other => other,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Int(i) => *i != 0,
        Value::Real(f) => *f != 0.0,
        Value::Text(s) => s.parse::<f64>().map(|f| f != 0.0).unwrap_or(false),
    }
}

fn cmp_values(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Real(y)) => (*x as f64).partial_cmp(y),
        (Value::Real(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Int(_), Value::Text(_)) | (Value::Real(_), Value::Text(_)) => Some(Ordering::Less),
        (Value::Text(_), Value::Int(_)) | (Value::Text(_), Value::Real(_)) => Some(Ordering::Greater),
    }
}

fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => cmp_values(a, b).unwrap_or(Ordering::Equal),
    }
}

fn as_tribool(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(*i != 0),
        Value::Real(f) => Some(*f != 0.0),
        Value::Text(s) => Some(s.parse::<f64>().map(|f| f != 0.0).unwrap_or(false)),
    }
}

fn bool_val(b: Option<bool>) -> Value {
    match b {
        Some(true) => Value::Int(1),
        Some(false) => Value::Int(0),
        None => Value::Null,
    }
}

fn eval(e: &RExpr, row: &[Value], t: &Table, outers: &[&[Value]]) -> Value {
    eval_g(e, row, t, None, outers)
}

/// Evaluate a `[NOT] IN` test with three-valued logic.
fn in_result(left: &Value, vals: impl Iterator<Item = Value>, neg: bool) -> Value {
    let mut any = false;
    let mut has_null = false;
    let mut matched = false;
    for v in vals {
        any = true;
        if matches!(v, Value::Null) {
            has_null = true;
            continue;
        }
        if matches!(left, Value::Null) {
            continue;
        }
        if let Some(Ordering::Equal) = cmp_values(left, &v) {
            matched = true;
            break;
        }
    }
    let mut res: Option<bool> = if !any {
        Some(false)
    } else if matched {
        Some(true)
    } else if has_null || matches!(left, Value::Null) {
        None
    } else {
        Some(false)
    };
    if neg {
        res = res.map(|b| !b);
    }
    bool_val(res)
}

fn eval_g(e: &RExpr, row: &[Value], t: &Table, aggs: Option<&[Value]>, outers: &[&[Value]]) -> Value {
    match e {
        RExpr::Lit(v) => v.clone(),
        RExpr::Col(i) => row.get(*i).cloned().unwrap_or(Value::Null),
        RExpr::Outer(l, c) => outers
            .get(*l)
            .and_then(|r| r.get(*c))
            .cloned()
            .unwrap_or(Value::Null),
        RExpr::Agg(i) => match aggs {
            Some(vals) => vals.get(*i).cloned().unwrap_or(Value::Null),
            None => Value::Null,
        },
        RExpr::Neg(x) => match eval_g(x, row, t, aggs, outers) {
            Value::Null => Value::Null,
            v => neg_value(v),
        },
        RExpr::Not(x) => {
            let v = eval_g(x, row, t, aggs, outers);
            bool_val(as_tribool(&v).map(|b| !b))
        }
        RExpr::IsNull(x, neg) => {
            let isnull = matches!(eval_g(x, row, t, aggs, outers), Value::Null);
            Value::Int(if isnull != *neg { 1 } else { 0 })
        }
        RExpr::Fn(name, args) => eval_fn(name, args, row, t, aggs, outers),
        RExpr::InList(l, items, neg) => {
            let lv = eval_g(l, row, t, aggs, outers);
            let ty = match &**l {
                RExpr::Col(i) => t.cols.get(*i).map(|c| c.ty),
                _ => None,
            };
            let vals: Vec<Value> = items
                .iter()
                .map(|i| {
                    let v = eval_g(i, row, t, aggs, outers);
                    match ty {
                        Some(ty) => affinity(v, ty),
                        None => v,
                    }
                })
                .collect();
            in_result(&lv, vals.into_iter(), *neg)
        }
        RExpr::InSub(l, plan, neg) => {
            let lv = eval_g(l, row, t, aggs, outers);
            let ty = match &**l {
                RExpr::Col(i) => t.cols.get(*i).map(|c| c.ty),
                _ => None,
            };
            let outer_rows = shift_outers(row, outers);
            let rows = plan.rows(&outer_rows);
            let vals: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let v = r.first().cloned().unwrap_or(Value::Null);
                    match ty {
                        Some(ty) => affinity(v, ty),
                        None => v,
                    }
                })
                .collect();
            in_result(&lv, vals.into_iter(), *neg)
        }
        RExpr::Sub(plan) => {
            let outer_rows = shift_outers(row, outers);
            let rows = plan.rows(&outer_rows);
            rows.first()
                .and_then(|r| r.first())
                .cloned()
                .unwrap_or(Value::Null)
        }
        RExpr::Exists(plan) => {
            let outer_rows = shift_outers(row, outers);
            Value::Int(if plan.exists(&outer_rows) { 1 } else { 0 })
        }
        RExpr::Bin(op, a, b) => {
            match op {
                Op::And => {
                    let l = as_tribool(&eval_g(a, row, t, aggs, outers));
                    if l == Some(false) {
                        return Value::Int(0);
                    }
                    let r = as_tribool(&eval_g(b, row, t, aggs, outers));
                    if r == Some(false) {
                        return Value::Int(0);
                    }
                    return bool_val(if l == Some(true) && r == Some(true) {
                        Some(true)
                    } else {
                        None
                    });
                }
                Op::Or => {
                    let l = as_tribool(&eval_g(a, row, t, aggs, outers));
                    if l == Some(true) {
                        return Value::Int(1);
                    }
                    let r = as_tribool(&eval_g(b, row, t, aggs, outers));
                    if r == Some(true) {
                        return Value::Int(1);
                    }
                    return bool_val(if l == Some(false) && r == Some(false) {
                        Some(false)
                    } else {
                        None
                    });
                }
                _ => {}
            }
            let lv = eval_g(a, row, t, aggs, outers);
            let rv = eval_g(b, row, t, aggs, outers);
            match op {
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                    if matches!(lv, Value::Null) || matches!(rv, Value::Null) {
                        return Value::Null;
                    }
                    let mut lv = lv;
                    let mut rv = rv;
                    if let RExpr::Col(i) = **a {
                        rv = affinity(rv, t.cols[i].ty);
                        lv = affinity(lv, t.cols[i].ty);
                    } else if let RExpr::Col(i) = **b {
                        lv = affinity(lv, t.cols[i].ty);
                        rv = affinity(rv, t.cols[i].ty);
                    }
                    let ord = cmp_values(&lv, &rv);
                    let res = match (ord, op) {
                        (None, _) => false,
                        (Some(o), Op::Eq) => o == Ordering::Equal,
                        (Some(o), Op::Ne) => o != Ordering::Equal,
                        (Some(o), Op::Lt) => o == Ordering::Less,
                        (Some(o), Op::Le) => o != Ordering::Greater,
                        (Some(o), Op::Gt) => o == Ordering::Greater,
                        (Some(o), Op::Ge) => o != Ordering::Less,
                        _ => false,
                    };
                    Value::Int(if res { 1 } else { 0 })
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => arith(*op, lv, rv),
                Op::Concat => {
                    if matches!(lv, Value::Null) || matches!(rv, Value::Null) {
                        return Value::Null;
                    }
                    let mut s = String::new();
                    push_text(&lv, &mut s);
                    push_text(&rv, &mut s);
                    Value::Text(s)
                }
                Op::And | Op::Or => Value::Int(0),
            }
        }
    }
}

/// Build the outer-row stack seen inside a subquery whose enclosing row is
/// `row`.
fn shift_outers<'a>(row: &'a [Value], outers: &[&'a [Value]]) -> Vec<&'a [Value]> {
    let mut v: Vec<&[Value]> = Vec::with_capacity(outers.len() + 1);
    v.push(row);
    v.extend_from_slice(outers);
    v
}

fn eval_fn(
    name: &str,
    args: &[RExpr],
    row: &[Value],
    t: &Table,
    aggs: Option<&[Value]>,
    outers: &[&[Value]],
) -> Value {
    match name {
        "coalesce" => {
            for a in args {
                let v = eval_g(a, row, t, aggs, outers);
                if !matches!(v, Value::Null) {
                    return v;
                }
            }
            Value::Null
        }
        "length" => match eval_g(&args[0], row, t, aggs, outers) {
            Value::Null => Value::Null,
            Value::Text(s) => Value::Int(s.chars().count() as i64),
            v => {
                let mut s = String::new();
                push_text(&v, &mut s);
                Value::Int(s.len() as i64)
            }
        },
        "upper" => match eval_g(&args[0], row, t, aggs, outers) {
            Value::Null => Value::Null,
            v => {
                let mut s = String::new();
                push_text(&v, &mut s);
                Value::Text(s.to_ascii_uppercase())
            }
        },
        "lower" => match eval_g(&args[0], row, t, aggs, outers) {
            Value::Null => Value::Null,
            v => {
                let mut s = String::new();
                push_text(&v, &mut s);
                Value::Text(s.to_ascii_lowercase())
            }
        },
        "abs" => match eval_g(&args[0], row, t, aggs, outers) {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Int(i.wrapping_abs()),
            Value::Real(f) => Value::Real(f.abs()),
            Value::Text(s) => match s.trim().parse::<f64>() {
                Ok(f) => Value::Real(f.abs()),
                Err(_) => Value::Real(0.0),
            },
        },
        // --- PostgreSQL compatibility helpers (psql startup probes) ---
        "set_config" => eval_g(&args[1], row, t, aggs, outers),
        "current_setting" => Value::Text(String::new()),
        "version" => Value::Text(
            "PostgreSQL 16.4 (sdb) on x86_64-pc-linux-gnu, compiled by rustc".to_string(),
        ),
        "current_schema" => Value::Text("public".to_string()),
        "current_database" => Value::Text("sdb".to_string()),
        "current_user" | "session_user" | "user" => Value::Text("sdb".to_string()),
        "pg_backend_pid" => Value::Int(1),
        "pg_is_in_recovery" => Value::Int(0),
        "pg_get_userbyid" => Value::Text("sdb".to_string()),
        "current_schemas" => Value::Text(String::new()),
        "pg_encoding_to_char" => Value::Text("UTF8".to_string()),
        "format_type" => Value::Text("text".to_string()),
        "quote_ident" => match eval_g(&args[0], row, t, aggs, outers) {
            Value::Null => Value::Null,
            v => {
                let mut s = String::new();
                push_text(&v, &mut s);
                Value::Text(s)
            }
        },
        "quote_literal" => match eval_g(&args[0], row, t, aggs, outers) {
            Value::Null => Value::Null,
            v => {
                let mut s = String::new();
                push_text(&v, &mut s);
                Value::Text(format!("'{}'", s.replace('\'', "''")))
            }
        },
        _ => Value::Null,
    }
}

fn push_text(v: &Value, out: &mut String) {
    match v {
        Value::Null => {}
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Real(f) => out.push_str(&fmt_real(*f)),
        Value::Text(s) => out.push_str(s),
    }
}

fn arith(op: Op, a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Int(x), Value::Int(y)) => match op {
            Op::Add => Value::Int(x.wrapping_add(y)),
            Op::Sub => Value::Int(x.wrapping_sub(y)),
            Op::Mul => Value::Int(x.wrapping_mul(y)),
            Op::Div => {
                if y == 0 {
                    Value::Null
                } else {
                    Value::Int(x.wrapping_div(y))
                }
            }
            Op::Mod => {
                if y == 0 {
                    Value::Null
                } else {
                    Value::Int(x.wrapping_rem(y))
                }
            }
            _ => Value::Null,
        },
        (x, y) => {
            let fx = num_coerce(&x);
            let fy = num_coerce(&y);
            if let (Value::Int(x), Value::Int(y)) = (&fx, &fy) {
                return match op {
                    Op::Add => Value::Int(x.wrapping_add(*y)),
                    Op::Sub => Value::Int(x.wrapping_sub(*y)),
                    Op::Mul => Value::Int(x.wrapping_mul(*y)),
                    Op::Div => {
                        if *y == 0 {
                            Value::Null
                        } else {
                            Value::Int(x.wrapping_div(*y))
                        }
                    }
                    Op::Mod => {
                        if *y == 0 {
                            Value::Null
                        } else {
                            Value::Int(x.wrapping_rem(*y))
                        }
                    }
                    _ => Value::Null,
                };
            }
            let fx = as_f64(&fx);
            let fy = as_f64(&fy);
            match (fx, fy) {
                (Some(x), Some(y)) => match op {
                    Op::Add => Value::Real(x + y),
                    Op::Sub => Value::Real(x - y),
                    Op::Mul => Value::Real(x * y),
                    Op::Div => {
                        if y == 0.0 {
                            Value::Null
                        } else {
                            Value::Real(x / y)
                        }
                    }
                    Op::Mod => {
                        if y == 0.0 {
                            Value::Null
                        } else {
                            Value::Real(x % y)
                        }
                    }
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
    }
}

// Numeric coercion: text that looks like an integer becomes one,
// other numeric text becomes REAL and non-numeric text becomes 0.
fn num_coerce(v: &Value) -> Value {
    match v {
        Value::Text(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Value::Int(i)
            } else if let Ok(f) = t.parse::<f64>() {
                Value::Real(f)
            } else {
                Value::Int(0)
            }
        }
        other => other.clone(),
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Real(f) => Some(*f),
        Value::Text(s) => s.trim().parse::<f64>().ok(),
        Value::Null => None,
    }
}

// ---------------------------------------------------------------- storage ---

struct Wal {
    file: Arc<File>,
    segs: Vec<Vec<u8>>,
    pos: u64,
    cap: u64,
    seq: u64,
    durable: u64,
    syncing: bool,
    /// Token range of a batch whose bytes could not be written (disk full).
    /// Those statements were rolled back out of the log and must report the
    /// error to whoever is waiting on them.
    fail_lo: u64,
    fail_upto: u64,
}

struct WalShared {
    m: Mutex<Wal>,
    cv: Condvar,
}

impl WalShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Wal> {
        match self.m.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }
    fn wait<'a>(&self, g: std::sync::MutexGuard<'a, Wal>) -> std::sync::MutexGuard<'a, Wal> {
        match self.cv.wait(g) {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }
    /// Append a record to the shared buffer; returns a durability token.
    fn enqueue(&self, rec: &str) -> u64 {
        self.enqueue_owned(rec.to_string())
    }

    /// Append a record that is already owned (avoids a full copy of a
    /// multi-megabyte INSERT record).
    fn enqueue_owned(&self, mut rec: String) -> u64 {
        let mut g = self.lock();
        rec.push('\n');
        g.segs.push(rec.into_bytes());
        g.seq += 1;
        g.seq
    }

    /// Wait until the record with token `mine` is durable. Records appended
    /// while another thread is syncing are batched into a single fsync.
    fn flush(&self, mine: u64) -> Result<(), SqErr> {
        let mut g = self.lock();
        loop {
            if mine >= g.fail_lo && mine <= g.fail_upto {
                return Err(full_err());
            }
            if g.durable >= mine {
                return Ok(());
            }
            if !g.syncing {
                g.syncing = true;
                let target = g.seq;
                let lo = g.durable + 1;
                let segs = std::mem::take(&mut g.segs);
                let file = g.file.clone();
                let total: usize = segs.iter().map(|s| s.len()).sum();
                let at = g.pos;
                g.pos += total as u64;
                drop(g);
                // Positionless writes: no preallocation, so the file ends
                // exactly at the last complete record and a torn tail is
                // simply an incomplete final line.
                let mut off = at;
                let mut res = Ok(());
                for seg in &segs {
                    if let Err(e) = write_all_at(&file, seg, off) {
                        res = Err(e);
                        break;
                    }
                    off += seg.len() as u64;
                }
                if res.is_ok() {
                    res = file.sync_data();
                }
                drop(segs);
                trim_memory();
                g = self.lock();
                g.syncing = false;
                match res {
                    Ok(()) => {
                        g.durable = target;
                        self.cv.notify_all();
                    }
                    Err(e) => {
                        // The batch is only partly on disk. Cut it back to
                        // the last acknowledged byte so a later append can
                        // never splice rows onto a half-written record, and
                        // let every waiter know its statement was refused.
                        let _ = file.set_len(at);
                        let _ = file.sync_data();
                        g.pos = at;
                        g.fail_lo = lo;
                        g.fail_upto = target;
                        self.cv.notify_all();
                        return Err(if is_no_space(&e) {
                            full_err()
                        } else {
                            SqErr::new("io", format!("write failed: {}", e))
                        });
                    }
                }
            } else {
                g = self.wait(g);
            }
        }
    }
}

/// Per-connection transaction state. Changes are staged in private
/// copy-on-write table images and the matching WAL records are buffered;
/// nothing becomes visible (or durable) until COMMIT.
#[derive(Default)]
struct Txn {
    tables: HashMap<String, Table>,
    recs: Vec<String>,
    /// Committed-state version this transaction first observed. Used to
    /// detect a write that would be based on a stale read.
    snap: u64,
    /// True once the transaction has read committed state, i.e. once a
    /// later write could depend on what it read.
    read_committed: bool,
    /// True while this transaction holds the single-writer lease. The lease
    /// freezes committed state, so a leased transaction can never conflict.
    lease: bool,
    /// Committed table keys hidden by a `RENAME TO` in this transaction.
    /// Commit removes them from the committed image; rollback forgets them.
    hidden: HashSet<String>,
}

struct Db {
    tables: HashMap<String, Table>,
    wal: Option<Arc<WalShared>>,
    wal_bytes: u64,
    gen: u64,
    wal_path: PathBuf,
    snap_path: PathBuf,
    /// Connection whose statement is currently executing; used to route
    /// table reads/writes into that connection's transaction.
    cur: u64,
    /// Open transactions, one per connection at most.
    txns: HashMap<u64, Txn>,
    /// Connection currently holding the single-writer lease: set when a
    /// transaction starts (or performs its first write) and cleared when it
    /// ends. Writes by other connections wait for it (briefly) or are
    /// refused with `ERR busy`; reads never wait, so every reader keeps
    /// seeing the committed image.
    writer: Option<u64>,
    cv: Arc<Condvar>,
    /// Monotonic version of the committed state, bumped by every commit and
    /// every autocommit write. A transaction that read an older version and
    /// then writes is refused (`ERR busy`) instead of losing an update.
    data_gen: u64,
    /// WAL records appended by the statement currently executing, kept only
    /// in cluster mode so they can be shipped to replicas after commit.
    repl_buf: Vec<String>,
}

const WAL_COMPACT_BYTES: u64 = 256 * 1024 * 1024;

/// Read one newline-terminated line into `buf`, bounded by `max` bytes.
/// Returns the number of bytes read (0 at end of file).
fn read_line_capped<R: std::io::BufRead>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize> {
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok(buf.len());
        }
        if let Some(p) = avail.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&avail[..=p]);
            r.consume(p + 1);
            return Ok(buf.len());
        }
        let n = avail.len();
        buf.extend_from_slice(avail);
        r.consume(n);
        if buf.len() > max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long"));
        }
    }
}

fn wal_capacity(logical: u64) -> u64 {
    let want = logical.saturating_mul(2).max(1024 * 1024);
    let unit = 1024 * 1024u64;
    want.div_ceil(unit) * unit
}

fn esc_field(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
}

fn unesc_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn enc_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push('N'),
        Value::Int(i) => {
            out.push('i');
            out.push_str(&i.to_string());
        }
        Value::Real(f) => {
            out.push('r');
            out.push_str(&format!("{:?}", f));
        }
        Value::Text(s) => {
            out.push('t');
            esc_field(s, out);
        }
    }
}

/// Serialize a table's constraint metadata into the extra fields of its `C`
/// record. The plain column fields are unchanged, so records written by older
/// builds (which have no metadata) still load.
fn enc_schema_meta(name: &str, cols: &[Column], sch: &Schema, out: &mut Vec<String>) {
    if cols.iter().any(|c| c.notnull || c.def.is_some())
        || !sch.pk.is_empty()
        || !sch.uniq.is_empty()
        || !sch.fks.is_empty()
    {
        out.push("K".to_string());
    }
    let nn: Vec<String> = cols
        .iter()
        .enumerate()
        .filter(|(_, c)| c.notnull)
        .map(|(i, _)| i.to_string())
        .collect();
    if !nn.is_empty() {
        out.push(format!("n{}", nn.join(",")));
    }
    for (i, c) in cols.iter().enumerate() {
        if let Some(v) = &c.def {
            let mut s = format!("d{}", i);
            enc_value(v, &mut s);
            out.push(s);
        }
    }
    if !sch.pk.is_empty() {
        let l: Vec<String> = sch.pk.iter().map(|i| i.to_string()).collect();
        out.push(format!("p{}", l.join(",")));
    }
    if !sch.pk.is_empty() {
        out.push(if sch.pk_auto { "A".to_string() } else { "B".to_string() });
    }
    for u in &sch.uniq {
        let l: Vec<String> = u.iter().map(|i| i.to_string()).collect();
        out.push(format!("u{}", l.join(",")));
    }
    for fk in &sch.fks {
        let l: Vec<String> = fk.cols.iter().map(|i| i.to_string()).collect();
        out.push(format!("f{}:{}:{}", l.join(","), fk.parent, fk.pcols.join(",")));
    }
    let _ = name;
}

fn parse_idx_list(s: &str) -> Option<Vec<usize>> {
    let mut v = Vec::new();
    if s.is_empty() {
        return Some(v);
    }
    for p in s.split(',') {
        v.push(p.parse::<usize>().ok()?);
    }
    Some(v)
}

/// Restore the constraint metadata emitted by `enc_schema_meta`.
fn dec_schema_meta(fields: &[&str], cols: &mut [Column], sch: &mut Schema) -> bool {
    for f in fields {
        let mut it = f.chars();
        let kind = match it.next() {
            Some(c) => c,
            None => return false,
        };
        let rest = it.as_str();
        match kind {
            'n' => match parse_idx_list(rest) {
                Some(l) => {
                    for i in l {
                        match cols.get_mut(i) {
                            Some(c) => c.notnull = true,
                            None => return false,
                        }
                    }
                }
                None => return false,
            },
            'd' => {
                let split = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
                let idx: usize = match rest[..split].parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                match dec_value(&rest[split..]) {
                    Some(v) => match cols.get_mut(idx) {
                        Some(c) => c.def = Some(v),
                        None => return false,
                    },
                    None => return false,
                }
            }
            'p' => match parse_idx_list(rest) {
                Some(l) => sch.pk = l,
                None => return false,
            },
            'A' => sch.pk_auto = true,
            'B' => sch.pk_auto = false,
            'u' => match parse_idx_list(rest) {
                Some(l) => sch.uniq.push(l),
                None => return false,
            },
            'f' => {
                let mut parts = rest.splitn(3, ':');
                let cl = match parts.next().and_then(parse_idx_list) {
                    Some(v) => v,
                    None => return false,
                };
                let parent = match parts.next() {
                    Some(s) => s.to_string(),
                    None => return false,
                };
                let pc = match parts.next() {
                    Some(s) => s.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect(),
                    None => return false,
                };
                sch.fks.push(Fk { cols: cl, parent, pcols: pc });
            }
            _ => return false,
        }
    }
    true
}

fn dec_value(s: &str) -> Option<Value> {
    let mut it = s.chars();
    let c = it.next()?;
    let rest = it.as_str();
    match c {
        'N' => Some(Value::Null),
        'i' => rest.parse::<i64>().ok().map(Value::Int),
        'r' => rest.parse::<f64>().ok().map(Value::Real),
        't' => Some(Value::Text(unesc_field(rest))),
        _ => None,
    }
}

impl Db {
    fn open(dir: &str) -> Db {
        let _ = fs::create_dir_all(dir);
        let dirp = PathBuf::from(dir);
        set_spill_dir(dirp.join("spill"));
        set_mem_budget(cgroup_limit());
        let snap_path = dirp.join("sdb.snap");
        let wal_path = dirp.join("sdb.wal");
        let mut db = Db {
            tables: HashMap::new(),
            wal: None,
            wal_bytes: 0,
            gen: 0,
            wal_path: wal_path.clone(),
            snap_path: snap_path.clone(),
            cur: 0,
            txns: HashMap::new(),
            writer: None,
            cv: Arc::new(Condvar::new()),
            data_gen: 0,
            repl_buf: Vec::new(),
        };

        // The snapshot is line-oriented text and can be far larger than
        // memory, so it is replayed one record at a time.
        let mut snap_gen = 0u64;
        if let Ok(f) = File::open(&snap_path) {
            let mut r = std::io::BufReader::with_capacity(1 << 20, f);
            let mut line: Vec<u8> = Vec::new();
            let mut first = true;
            let mut ok = true;
            loop {
                line.clear();
                match read_line_capped(&mut r, &mut line, 64 << 20) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                if line.last() != Some(&b'\n') {
                    break; // torn/partial last line
                }
                line.pop();
                let text = String::from_utf8_lossy(&line);
                if first {
                    first = false;
                    match text.strip_prefix("G\t") {
                        Some(g) => snap_gen = g.trim().parse().unwrap_or(0),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                    continue;
                }
                if !text.is_empty() && !db.apply_record(&text) {
                    break;
                }
            }
            if !ok {
                snap_gen = 0;
            }
        }
        db.gen = snap_gen;

        // Load the write-ahead log if it belongs to the current snapshot.
        //
        // Records are only meaningful once their terminating newline is on
        // disk: recovery replays every complete line it can parse and
        // truncates everything after it, so a torn record can never come back
        // as a half-written row.  The log is streamed (it can be hundreds of
        // megabytes) and a trailing partial record is simply ignored.
        let mut header_ok = false;
        let mut valid_len: u64 = 0;
        if let Ok(f) = File::open(&wal_path) {
            let mut r = std::io::BufReader::with_capacity(1 << 20, f);
            let mut line: Vec<u8> = Vec::new();
            let mut first = true;
            let mut pending: Vec<String> = Vec::new();
            let mut in_txn = false;
            let mut stop = false;
            let mut pos: u64 = 0;
            loop {
                line.clear();
                let n = match read_line_capped(&mut r, &mut line, 64 << 20) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                pos += n as u64;
                if line.last() != Some(&b'\n') || line.iter().any(|&b| b == 0) {
                    break; // torn tail, or zero padding from an old layout
                }
                line.pop();
                let text = String::from_utf8_lossy(&line);
                if first {
                    first = false;
                    if let Some(g) = text.strip_prefix("G\t") {
                        if g.trim().parse::<u64>().unwrap_or(u64::MAX) == snap_gen {
                            header_ok = true;
                            valid_len = pos;
                        }
                    }
                    continue;
                }
                if !header_ok {
                    break;
                }
                if text.is_empty() {
                    break;
                }
                if text == "T\tB" {
                    if in_txn {
                        break;
                    }
                    in_txn = true;
                    pending.clear();
                } else if text == "T\tE" {
                    if !in_txn {
                        break;
                    }
                    for rec in pending.drain(..) {
                        if !db.apply_record(&rec) {
                            stop = true;
                            break;
                        }
                    }
                    if stop {
                        break;
                    }
                    in_txn = false;
                    valid_len = pos;
                } else if in_txn {
                    pending.push(text.into_owned());
                    valid_len = pos;
                } else {
                    if !db.apply_record(&text) {
                        break;
                    }
                    valid_len = pos;
                }
            }
            // An unterminated transaction frame is a torn commit: it is
            // dropped entirely, which also truncates it from the log below.
        }
        // (Re)create the WAL for this generation when it is missing or stale.
        if !header_ok {
            let header = format!("G\t{}\n", db.gen);
            if fs::write(&wal_path, &header).is_ok() {
                valid_len = header.len() as u64;
            }
        }
        // Cut a torn tail before appending new records.
        if let Ok(f) = OpenOptions::new().write(true).open(&wal_path) {
            let _ = f.set_len(valid_len);
        }
        db.wal_bytes = valid_len;
        if let Ok(f) = OpenOptions::new().read(true).write(true).create(true).open(&wal_path) {
            db.wal = Some(Arc::new(WalShared {
                m: Mutex::new(Wal {
                    file: Arc::new(f),
                    segs: Vec::new(),
                    pos: valid_len,
                    cap: 0,
                    seq: 0,
                    durable: 0,
                    syncing: false,
                    fail_lo: 1,
                    fail_upto: 0,
                }),
                cv: Condvar::new(),
            }));
        }
        db
    }

    // -------------------------------------------------------- txn support ---

    /// Resolve a table for reading: the executing connection's uncommitted
    /// copy wins, otherwise the committed image.
    fn table_ref(&self, lower: &str) -> Option<&Table> {
        if let Some(tx) = self.txns.get(&self.cur) {
            if let Some(t) = tx.tables.get(lower) {
                return Some(t);
            }
            if tx.hidden.contains(lower) {
                return None;
            }
        }
        self.tables.get(lower)
    }

    fn table_exists(&self, lower: &str) -> bool {
        self.table_ref(lower).is_some()
    }

    /// Resolve a table for writing. Inside a transaction the first write
    /// clones the committed image into the transaction (copy on write), so
    /// concurrent readers keep seeing the committed rows.
    fn table_mut(&mut self, lower: &str) -> Option<&mut Table> {
        let cur = self.cur;
        if self.txns.contains_key(&cur) {
            let have = self.txns.get(&cur).map(|t| t.tables.contains_key(lower)).unwrap_or(false);
            if !have {
                // Spill a resident image before copying it: cloning a
                // spill-backed row set is O(rows) pointers instead of a deep
                // copy of every cell, and the spill is invisible to readers.
                if let Some(src) = self.tables.get_mut(lower) {
                    if src.rows.bytes > 256 * 1024 {
                        src.rows.spill();
                    }
                }
                let mut copy = self.tables.get(lower)?.clone();
                copy.idx.clear();
                if let Some(tx) = self.txns.get_mut(&cur) {
                    tx.tables.insert(lower.to_string(), copy);
                }
            }
            return self.txns.get_mut(&cur).and_then(|t| t.tables.get_mut(lower));
        }
        self.tables.get_mut(lower)
    }

    /// Borrow the table visible to the current connection without staging a
    /// copy-on-write transaction table (used only to set query-local metadata).
    fn table_view_mut(&mut self, lower: &str) -> Option<&mut Table> {
        let cur = self.cur;
        if let Some(tx) = self.txns.get_mut(&cur) {
            if let Some(t) = tx.tables.get_mut(lower) {
                return Some(t);
            }
            if tx.hidden.contains(lower) {
                return None;
            }
        }
        self.tables.get_mut(lower)
    }

    // ------------------------------------------------------ constraints ---

    /// Remove a table from the visible map for the duration of one statement,
    /// so constraint checks can borrow it mutably while still reading the
    /// other tables. `put_table` restores it (to the transaction when one is
    /// active, exactly like `table_mut`).
    fn take_table(&mut self, lower: &str) -> Option<Table> {
        let cur = self.cur;
        if self.txns.contains_key(&cur) {
            let have = self.txns.get(&cur).map(|t| t.tables.contains_key(lower)).unwrap_or(false);
            if have {
                return self.txns.get_mut(&cur).and_then(|t| t.tables.remove(lower));
            }
            if self.txns.get(&cur).map(|t| t.hidden.contains(lower)).unwrap_or(false) {
                return None;
            }
            return self.tables.get(lower).cloned();
        }
        self.tables.remove(lower)
    }

    fn put_table(&mut self, lower: &str, t: Table) {
        let cur = self.cur;
        if self.txns.contains_key(&cur) {
            if let Some(tx) = self.txns.get_mut(&cur) {
                tx.tables.insert(lower.to_string(), t);
            }
        } else {
            self.tables.insert(lower.to_string(), t);
        }
    }

    /// Every table name visible to this connection (committed plus staged).
    fn all_table_names(&self) -> Vec<String> {
        if let Some(tx) = self.txns.get(&self.cur) {
            let mut names: Vec<String> = self
                .tables
                .keys()
                .filter(|k| !tx.hidden.contains(*k))
                .cloned()
                .collect();
            for k in tx.tables.keys() {
                if !names.contains(k) {
                    names.push(k.clone());
                }
            }
            names
        } else {
            self.tables.keys().cloned().collect()
        }
    }

    /// Resolve the foreign keys of `t` (visible as `lower`) against their
    /// parent tables. Reports missing tables and foreign-key mismatches the
    /// way SQLite does on the first DML statement that needs them.
    fn resolve_fks(&self, lower: &str, t: &Table) -> Result<Vec<RFk>, SqErr> {
        let mut out = Vec::with_capacity(t.sch.fks.len());
        for fk in &t.sch.fks {
            let parent = if fk.parent == lower {
                t
            } else {
                match self.table_ref(&fk.parent) {
                    Some(p) => p,
                    None => return Err(no_table(&fk.parent)),
                }
            };
            out.push(resolve_one_fk(lower, fk, parent)?);
        }
        Ok(out)
    }

    /// Resolve just the foreign keys of `child` that point at `target`.
    /// Used when a statement changes (or deletes) parent rows: the target
    /// table is temporarily out of the map, so it is supplied explicitly.
    fn resolve_parent_fks(
        &self,
        child_name: &str,
        child: &Table,
        target_lower: &str,
        target: &Table,
    ) -> Result<Vec<RFk>, SqErr> {
        let mut out = Vec::new();
        for fk in &child.sch.fks {
            if fk.parent == target_lower {
                out.push(resolve_one_fk(child_name, fk, target)?);
            }
        }
        Ok(out)
    }

    /// Build the point-lookup indexes used by foreign-key existence checks.
    fn ensure_fk_indexes(&mut self, lower: &str, child: &mut Table, fks: &[RFk]) {
        for fk in fks {
            if fk.pcols.len() != 1 {
                continue;
            }
            if fk.parent == lower {
                build_index(child, fk.pcols[0]);
            } else {
                self.ensure_index(&fk.parent, fk.pcols[0]);
            }
        }
    }

    /// True when the (all non-NULL) foreign-key value of `row` has a parent.
    fn fk_exists(&self, lower: &str, child: &Table, fk: &RFk, row: &[Value]) -> bool {
        let parent = if fk.parent == lower {
            child
        } else {
            match self.table_ref(&fk.parent) {
                Some(p) => p,
                None => return false,
            }
        };
        let mut key: Vec<Key> = Vec::with_capacity(fk.cols.len());
        for (i, &c) in fk.cols.iter().enumerate() {
            let v = match row.get(c) {
                Some(v) => v.clone(),
                None => return true,
            };
            if v == Value::Null {
                return true;
            }
            // SQLite compares a foreign-key value with the parent key's
            // affinity, so a TEXT child value can match an INTEGER parent.
            let ty = parent.cols.get(fk.pcols[i]).map(|c| c.ty).unwrap_or(Ty::Int);
            match key_of(&affinity(v, ty)) {
                Some(k) => key.push(k),
                None => return true,
            }
        }
        if fk.pcols.len() == 1 {
            if let Some(ix) = parent.idx.get(&fk.pcols[0]) {
                return ix.map.get(&key[0]).map(|v| !v.is_empty()).unwrap_or(false);
            }
        }
        (0..parent.rows.len()).any(|ri| {
            let r = parent.rows.get(ri);
            fk.pcols.iter().enumerate().all(|(i, &pc)| match r.get(pc) {
                Some(Value::Null) | None => false,
                Some(v) => key_of(v).as_ref() == Some(&key[i]),
            })
        })
    }

    /// Validate every foreign key that points at `lower` against the target
    /// table image `t`. `skip` marks rows of `t` that are being deleted and
    /// therefore no longer count as parents (or as children, for a
    /// self-referencing key).
    fn check_children(&self, lower: &str, t: &Table, skip: Option<&[bool]>) -> Result<(), SqErr> {
        let mut names = self.all_table_names();
        if !names.iter().any(|n| n == lower) {
            names.push(lower.to_string());
        }
        for cname in &names {
            let child: &Table = if cname == lower {
                t
            } else {
                match self.table_ref(cname) {
                    Some(c) => c,
                    None => continue,
                }
            };
            let fks = self.resolve_parent_fks(cname, child, lower, t)?;
            if fks.is_empty() {
                continue;
            }
            let mut live: HashSet<Vec<Key>> = HashSet::new();
            for fk in &fks {
                live.clear();
                for ri in 0..t.rows.len() {
                    if skip.map(|s| s.get(ri).copied().unwrap_or(false)).unwrap_or(false) {
                        continue;
                    }
                    let row = t.rows.get(ri);
                    let mut k = Vec::with_capacity(fk.pcols.len());
                    let mut ok = true;
                    for &pc in &fk.pcols {
                        match row.get(pc).and_then(key_of) {
                            Some(v) => k.push(v),
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        live.insert(k);
                    }
                }
                for ri in 0..child.rows.len() {
                    if cname == lower
                        && skip.map(|s| s.get(ri).copied().unwrap_or(false)).unwrap_or(false)
                    {
                        continue;
                    }
                    let row = child.rows.get(ri);
                    let mut k: Vec<Key> = Vec::with_capacity(fk.cols.len());
                    let mut any_null = false;
                    for (i, &cc) in fk.cols.iter().enumerate() {
                        let v = match row.get(cc) {
                            Some(v) if *v != Value::Null => v.clone(),
                            _ => {
                                any_null = true;
                                break;
                            }
                        };
                        let ty = t.cols.get(fk.pcols[i]).map(|c| c.ty).unwrap_or(Ty::Int);
                        match key_of(&affinity(v, ty)) {
                            Some(kk) => k.push(kk),
                            None => {
                                any_null = true;
                                break;
                            }
                        }
                    }
                    if any_null {
                        continue;
                    }
                    if !live.contains(&k) {
                        return Err(constraint_err("FOREIGN KEY constraint failed"));
                    }
                }
            }
        }
        Ok(())
    }

    fn begin_txn(&mut self, conn: u64, lease: bool) -> Result<String, SqErr> {
        if self.txns.contains_key(&conn) {
            return Err(err_txn("cannot start a transaction within a transaction"));
        }
        let mut t = Txn::default();
        t.snap = self.data_gen;
        t.lease = lease;
        if lease {
            self.writer = Some(conn);
        }
        self.txns.insert(conn, t);
        Ok("OK 0\n".to_string())
    }

    fn rollback_txn(&mut self, conn: u64) -> Result<String, SqErr> {
        if self.txns.remove(&conn).is_none() {
            return Err(err_txn("cannot rollback - no transaction is active"));
        }
        if self.writer == Some(conn) {
            self.writer = None;
            self.cv.notify_all();
        }
        Ok("OK 0\n".to_string())
    }

    /// Refuse the current transaction (conflict or a wait that ran too
    /// long): discard everything it staged and release the lease. Nothing
    /// reached the log, so dropping the staged images is all it takes.
    fn abort_txn(&mut self, conn: u64) {
        self.txns.remove(&conn);
        if self.writer == Some(conn) {
            self.writer = None;
            self.cv.notify_all();
        }
    }

    /// Make a transaction durable and publish its staged tables. Called with
    /// the global mutex held; holding it across the fsync keeps the published
    /// state consistent with the log (and transactions are not the hot path).
    fn commit_txn(&mut self, conn: u64) -> Result<String, SqErr> {
        if !self.txns.contains_key(&conn) {
            return Err(err_txn("cannot commit - no transaction is active"));
        }
        let recs = match self.txns.get_mut(&conn) {
            Some(tx) => std::mem::take(&mut tx.recs),
            None => Vec::new(),
        };
        let mut last = 0u64;
        let framed = !recs.is_empty();
        if framed {
            // Frame the batch so recovery can tell a complete transaction
            // from a torn tail: an unterminated frame is discarded whole.
            if let Some(tok) = self.wal_append("T\tB") {
                last = tok;
            }
        }
        for r in recs {
            if let Some(tok) = self.wal_append_owned(r) {
                last = tok;
            }
        }
        if framed {
            if let Some(tok) = self.wal_append("T\tE") {
                last = tok;
            }
        }
        if last > 0 {
            if let Some(w) = &self.wal {
                if let Err(e) = w.flush(last) {
                    // Nothing was published: drop the staged transaction so
                    // the connection cannot commit a half-logged batch later.
                    self.txns.remove(&conn);
                    if self.writer == Some(conn) {
                        self.writer = None;
                    }
                    self.cv.notify_all();
                    self.recover_after_write_error();
                    return Err(e);
                }
            }
        }
        // Publish the staged tables: an in-memory move, no copying.
        let tx = match self.txns.remove(&conn) {
            Some(t) => t,
            None => Txn::default(),
        };
        let changed = !tx.tables.is_empty() || !tx.hidden.is_empty();
        for name in &tx.hidden {
            self.tables.remove(name);
        }
        for (name, t) in tx.tables {
            self.tables.insert(name, t);
        }
        if changed {
            self.data_gen += 1;
        }
        if self.writer == Some(conn) {
            self.writer = None;
        }
        self.cv.notify_all();
        Ok("OK 0\n".to_string())
    }

    /// A connection went away: discard any transaction it left open.
    fn end_conn(&mut self, conn: u64) {
        if self.txns.remove(&conn).is_some() && self.writer == Some(conn) {
            self.writer = None;
            self.cv.notify_all();
        }
    }

    /// Bytes buffered for the log but not yet written to it.
    fn wal_pending(&self) -> u64 {
        match &self.wal {
            Some(w) => {
                let g = w.lock();
                g.segs.iter().map(|s| s.len() as u64).sum()
            }
            None => 0,
        }
    }

    fn data_dir(&self) -> &std::path::Path {
        match self.wal_path.parent() {
            Some(p) => p,
            None => std::path::Path::new("."),
        }
    }

    /// Can a statement that will add about `need` bytes to the log commit?
    /// Checked before any row is touched so a full volume refuses the whole
    /// statement instead of half-applying it.
    fn have_space(&self, need: u64) -> bool {
        match fs_available(self.data_dir()) {
            Some(free) => free >= need.saturating_add(self.wal_pending()),
            None => true,
        }
    }

    /// The log write failed (the volume filled up). `flush` already cut the
    /// partial batch off the log, so the bytes on disk are exactly the last
    /// acknowledged state. Rebuild the in-memory image from that state so a
    /// statement that could not be logged leaves no trace.
    fn recover_after_write_error(&mut self) {
        self.txns.clear();
        self.writer = None;
        self.repl_buf.clear();
        self.cv.notify_all();
        let dir = match self.wal_path.parent() {
            Some(p) => p.to_string_lossy().into_owned(),
            None => return,
        };
        let fresh = Db::open(&dir);
        self.tables = fresh.tables;
        self.gen = fresh.gen;
        self.data_gen += 1;
    }

    fn apply_record(&mut self, line: &str) -> bool {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.is_empty() {
            return false;
        }
        match fields[0] {
            "C" if fields.len() >= 3 => {
                let name = unesc_field(fields[1]);
                let lower = name.to_lowercase();
                if self.tables.contains_key(&lower) {
                    return false;
                }
                let mut cols = Vec::new();
                let mut sch = Schema::default();
                let mut meta: Option<&[&str]> = None;
                for (fi, f) in fields[2..].iter().enumerate() {
                    if *f == "K" {
                        meta = Some(&fields[2 + fi + 1..]);
                        break;
                    }
                    if let Some((cn, ct)) = f.rsplit_once(':') {
                        match Ty::from_code(ct) {
                            Some(ty) => {
                                let cn = unesc_field(cn);
                                cols.push(Column {
                                    lower: cn.to_lowercase(),
                                    name: cn,
                                    ty,
                                    q: None,
                                    notnull: false,
                                    def: None,
                                });
                            }
                            None => return false,
                        }
                    } else {
                        return false;
                    }
                }
                let mut pk_flag = false;
                if let Some(m) = meta {
                    if m.iter().any(|f| *f == "A" || *f == "B") {
                        pk_flag = true;
                    }
                    if !dec_schema_meta(m, &mut cols, &mut sch) {
                        return false;
                    }
                }
                // Records written before the rowid-alias flag existed treated
                // a single INTEGER primary key as auto-assigned.
                if !pk_flag && sch.pk.len() == 1 {
                    if cols.get(sch.pk[0]).map(|c| c.ty) == Some(Ty::Int) {
                        sch.pk_auto = true;
                    }
                }
                self.tables.insert(
                    lower.clone(),
                    Table {
                        name,
                        lower,
                        alias: None,
                        cols,
                        rows: RowSet::new(),
                        idx: HashMap::new(),
                        sch,
                    },
                );
                true
            }
            "I" if fields.len() >= 4 => {
                let name = unesc_field(fields[1]).to_lowercase();
                let ncols: usize = fields[2].parse().unwrap_or(0);
                let nrows: usize = fields[3].parse().unwrap_or(0);
                let mut vals: Vec<Value> = Vec::with_capacity(nrows * ncols);
                for f in &fields[4..] {
                    match dec_value(f) {
                        Some(v) => vals.push(v),
                        None => return false,
                    }
                }
                if ncols == 0 || vals.len() != ncols * nrows {
                    return false;
                }
                if let Some(t) = self.tables.get_mut(&name) {
                    let mut it = vals.into_iter();
                    for _ in 0..nrows {
                        let mut row = Vec::with_capacity(ncols);
                        for _ in 0..ncols {
                            row.push(it.next().unwrap_or(Value::Null));
                        }
                        insert_row_indexed(t, row);
                    }
                }
                true
            }
            "U" if fields.len() >= 5 => {
                let name = unesc_field(fields[1]).to_lowercase();
                let ncols: usize = match fields[2].parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let count: usize = match fields[3].parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let t = match self.tables.get_mut(&name) {
                    Some(t) => t,
                    None => return false,
                };
                if t.cols.len() != ncols {
                    return false;
                }
                let mut i = 4usize;
                for _ in 0..count {
                    if i >= fields.len() {
                        return false;
                    }
                    let ri: usize = match fields[i].parse() {
                        Ok(v) => v,
                        Err(_) => return false,
                    };
                    i += 1;
                    if ri >= t.rows.len() || i + ncols > fields.len() {
                        return false;
                    }
                    let mut row = Vec::with_capacity(ncols);
                    for _ in 0..ncols {
                        match dec_value(fields[i]) {
                            Some(v) => row.push(v),
                            None => return false,
                        }
                        i += 1;
                    }
                    t.rows.set(ri, row);
                }
                rebuild_indexes(t);
                true
            }
            "D" if fields.len() >= 3 => {
                let name = unesc_field(fields[1]).to_lowercase();
                let count: usize = match fields[2].parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let t = match self.tables.get_mut(&name) {
                    Some(t) => t,
                    None => return false,
                };
                if count > t.rows.len() || fields.len() < 3 + count {
                    return false;
                }
                let mut drop = vec![false; t.rows.len()];
                for k in 0..count {
                    let ri: usize = match fields[3 + k].parse() {
                        Ok(v) => v,
                        Err(_) => return false,
                    };
                    if ri >= drop.len() {
                        return false;
                    }
                    drop[ri] = true;
                }
                t.rows.retain(|k, _| !drop[k]);
                rebuild_indexes(t);
                true
            }
            "E" if fields.len() >= 4 => {
                let name = unesc_field(fields[1]).to_lowercase();
                let ncols: usize = match fields[2].parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let nrows: usize = match fields[3].parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let t = match self.tables.get_mut(&name) {
                    Some(t) => t,
                    None => return false,
                };
                if t.cols.len() != ncols || fields.len() != 4 + nrows * ncols {
                    return false;
                }
                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(nrows);
                let mut i = 4usize;
                for _ in 0..nrows {
                    let mut row = Vec::with_capacity(ncols);
                    for _ in 0..ncols {
                        match dec_value(fields[i]) {
                            Some(v) => row.push(v),
                            None => return false,
                        }
                        i += 1;
                    }
                    rows.push(row);
                }
                t.rows = RowSet::from_rows(rows);
                rebuild_indexes(t);
                true
            }
            "X" if fields.len() >= 5 => {
                let name = unesc_field(fields[1]);
                let lower = name.to_lowercase();
                let tname = unesc_field(fields[2]).to_lowercase();
                let col: usize = match fields[3].parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let unique = fields[4] == "1";
                let t = match self.tables.get_mut(&tname) {
                    Some(t) => t,
                    None => return false,
                };
                if col >= t.cols.len() {
                    return false;
                }
                if !t.sch.idx_defs.iter().any(|d| d.lower == lower) {
                    t.sch.idx_defs.push(IndexDef { name, lower, col, unique });
                }
                true
            }
            "Y" if fields.len() >= 2 => {
                let lower = unesc_field(fields[1]).to_lowercase();
                for t in self.tables.values_mut() {
                    t.sch.idx_defs.retain(|d| d.lower != lower);
                }
                true
            }
            // ALTER TABLE ... ADD COLUMN: append the column and materialize
            // its default in every existing row.
            "P" if fields.len() >= 5 => {
                let lower = unesc_field(fields[1]).to_lowercase();
                let cname = unesc_field(fields[2]);
                let ty = match Ty::from_code(fields[3]) {
                    Some(t) => t,
                    None => return false,
                };
                let def = if fields[4] == "-" {
                    None
                } else {
                    match dec_value(fields[4]) {
                        Some(v) => Some(v),
                        None => return false,
                    }
                };
                let t = match self.tables.get_mut(&lower) {
                    Some(t) => t,
                    None => return false,
                };
                let cl = cname.to_lowercase();
                if t.cols.iter().any(|c| c.lower == cl) {
                    return false;
                }
                t.cols.push(Column {
                    name: cname,
                    lower: cl,
                    ty,
                    q: None,
                    notnull: false,
                    def: def.clone(),
                });
                let fill = def.unwrap_or(Value::Null);
                t.rows.push_to_all(fill);
                true
            }
            // ALTER TABLE ... DROP COLUMN.
            "Q" if fields.len() >= 3 => {
                let lower = unesc_field(fields[1]).to_lowercase();
                let cl = unesc_field(fields[2]).to_lowercase();
                let t = match self.tables.get_mut(&lower) {
                    Some(t) => t,
                    None => return false,
                };
                let ci = match t.col_index(&cl) {
                    Some(i) => i,
                    None => return false,
                };
                drop_column_at(t, ci);
                true
            }
            // ALTER TABLE ... RENAME COLUMN.
            "W" if fields.len() >= 4 => {
                let lower = unesc_field(fields[1]).to_lowercase();
                let oldl = unesc_field(fields[2]).to_lowercase();
                let newname = unesc_field(fields[3]);
                let newl = newname.to_lowercase();
                let t = match self.tables.get_mut(&lower) {
                    Some(t) => t,
                    None => return false,
                };
                let ci = match t.col_index(&oldl) {
                    Some(i) => i,
                    None => return false,
                };
                t.cols[ci].name = newname;
                t.cols[ci].lower = newl.clone();
                // Foreign keys pointing at this table follow the new name.
                for other in self.tables.values_mut() {
                    for fk in &mut other.sch.fks {
                        if fk.parent == lower {
                            for p in &mut fk.pcols {
                                if *p == oldl {
                                    *p = newl.clone();
                                }
                            }
                        }
                    }
                }
                true
            }
            // ALTER TABLE ... RENAME TO.
            "V" if fields.len() >= 3 => {
                let old = unesc_field(fields[1]).to_lowercase();
                let newname = unesc_field(fields[2]);
                let newl = newname.to_lowercase();
                let mut t = match self.tables.remove(&old) {
                    Some(t) => t,
                    None => return false,
                };
                t.name = newname;
                t.lower = newl.clone();
                self.tables.insert(newl.clone(), t);
                for other in self.tables.values_mut() {
                    for fk in &mut other.sch.fks {
                        if fk.parent == old {
                            fk.parent = newl.clone();
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn wal_append(&mut self, rec: &str) -> Option<u64> {
        match &self.wal {
            Some(w) => {
                self.wal_bytes += rec.len() as u64 + 1;
                if repl::active() {
                    self.repl_buf.push(rec.to_string());
                }
                Some(w.enqueue(rec))
            }
            None => None,
        }
    }

    fn wal_append_owned(&mut self, rec: String) -> Option<u64> {
        match &self.wal {
            Some(w) => {
                self.wal_bytes += rec.len() as u64 + 1;
                if repl::active() {
                    self.repl_buf.push(rec.clone());
                }
                Some(w.enqueue_owned(rec))
            }
            None => None,
        }
    }

    /// Append one or more records for a single statement so recovery applies
    /// them all or none.
    fn append_records_atomic(&mut self, recs: Vec<String>) -> Option<u64> {
        if recs.len() == 1 {
            return self.append_record_owned(recs.into_iter().next().unwrap());
        }
        if recs.is_empty() {
            return None;
        }
        let cur = self.cur;
        if let Some(tx) = self.txns.get_mut(&cur) {
            tx.recs.extend(recs);
            return None;
        }
        let mut last = self.wal_append("T\tB");
        for r in recs {
            if let Some(tok) = self.wal_append_owned(r) {
                last = Some(tok);
            }
        }
        if let Some(tok) = self.wal_append("T\tE") {
            last = Some(tok);
        }
        last
    }

    fn append_record_owned(&mut self, rec: String) -> Option<u64> {
        let cur = self.cur;
        if let Some(tx) = self.txns.get_mut(&cur) {
            tx.recs.push(rec);
            return None;
        }
        self.wal_append_owned(rec)
    }

    fn append_record(&mut self, rec: &str) -> Option<u64> {
        let cur = self.cur;
        if let Some(tx) = self.txns.get_mut(&cur) {
            // Staged until COMMIT; nothing hits the log yet.
            tx.recs.push(rec.to_string());
            return None;
        }
        self.wal_append(rec)
    }

    fn create_table(
        &mut self,
        name: &str,
        defs: &[ColDef],
        cons: &[TblCon],
    ) -> Result<Option<u64>, SqErr> {
        let lower = name.to_lowercase();
        if self.table_exists(&lower) {
            return Err(SqErr::new("exists", format!("table {} already exists", name)));
        }
        if self.index_owner(&lower).is_some() {
            return Err(SqErr::new("syntax", format!("there is already an index named {}", name)));
        }
        let mut seen: HashMap<String, ()> = HashMap::new();
        for cd in defs {
            let l = cd.name.to_lowercase();
            if seen.insert(l, ()).is_some() {
                return Err(SqErr::new("exists", format!("duplicate column name: {}", cd.name)));
            }
        }
        let find = |n: &str| -> Option<usize> {
            let l = n.to_lowercase();
            defs.iter().position(|c| c.name.to_lowercase() == l)
        };
        let mut cols: Vec<Column> = Vec::with_capacity(defs.len());
        for cd in defs {
            let def = match &cd.default {
                Some(e) => match const_eval(e) {
                    // SQLite applies the column's affinity to a DEFAULT
                    // before storing it in new rows.
                    Some(v) => Some(affinity(v, cd.ty)),
                    None => return Err(SqErr::syntax("near \"DEFAULT\": syntax error")),
                },
                None => None,
            };
            cols.push(Column {
                name: cd.name.clone(),
                lower: cd.name.to_lowercase(),
                ty: cd.ty,
                q: None,
                notnull: cd.notnull,
                def,
            });
        }
        let mut sch = Schema::default();
        let mut pk_declared = false;
        for (i, cd) in defs.iter().enumerate() {
            if cd.pk {
                if pk_declared {
                    return Err(SqErr::syntax(&format!("table \"{}\" has more than one primary key", name)));
                }
                pk_declared = true;
                sch.pk.push(i);
            }
        }
        for c in cons {
            match c {
                TblCon::Pk(list) => {
                    if pk_declared {
                        return Err(SqErr::syntax(&format!("table \"{}\" has more than one primary key", name)));
                    }
                    pk_declared = true;
                    for n in list {
                        match find(n) {
                            Some(i) => sch.pk.push(i),
                            None => return Err(no_column(n)),
                        }
                    }
                }
                TblCon::Unique(list) => {
                    let mut v = Vec::new();
                    for n in list {
                        match find(n) {
                            Some(i) => v.push(i),
                            None => return Err(no_column(n)),
                        }
                    }
                    sch.uniq.push(v);
                }
                TblCon::Foreign(list, parent, plist) => {
                    let mut v = Vec::new();
                    for n in list {
                        match find(n) {
                            Some(i) => v.push(i),
                            None => return Err(no_column(n)),
                        }
                    }
                    sch.fks.push(Fk {
                        cols: v,
                        parent: parent.to_lowercase(),
                        pcols: plist.iter().map(|s| s.to_lowercase()).collect(),
                    });
                }
            }
        }
        for (i, cd) in defs.iter().enumerate() {
            if let Some((parent, plist)) = &cd.references {
                sch.fks.push(Fk {
                    cols: vec![i],
                    parent: parent.to_lowercase(),
                    pcols: plist.iter().map(|s| s.to_lowercase()).collect(),
                });
            }
        }
        // Column-level UNIQUE declarations. (The primary key is enforced as
        // its own constraint.)
        for (i, cd) in defs.iter().enumerate() {
            if cd.unique {
                sch.uniq.push(vec![i]);
            }
        }
        // Primary key columns are NOT NULL. For a single INTEGER key NULL is
        // intercepted by auto-assignment before this check.
        for &i in &sch.pk {
            cols[i].notnull = true;
        }
        sch.pk_auto = sch.pk.len() == 1
            && defs[sch.pk[0]].decl_int
            && !defs[sch.pk[0]].pk_desc
            && cols[sch.pk[0]].ty == Ty::Int;

        let mut rec = String::new();
        rec.push_str("C\t");
        esc_field(name, &mut rec);
        for c in &cols {
            rec.push('\t');
            esc_field(&c.name, &mut rec);
            rec.push(':');
            rec.push_str(c.ty.code());
        }
        let mut meta: Vec<String> = Vec::new();
        enc_schema_meta(name, &cols, &sch, &mut meta);
        for m in meta {
            rec.push('\t');
            rec.push_str(&m);
        }
        let tok = self.append_record(&rec);
        let table = Table {
            name: name.to_string(),
            lower: lower.clone(),
            alias: None,
            cols,
            rows: RowSet::new(),
            idx: HashMap::new(),
            sch,
        };
        let cur = self.cur;
        if let Some(tx) = self.txns.get_mut(&cur) {
            tx.tables.insert(lower, table);
        } else {
            self.tables.insert(lower, table);
        }
        Ok(tok)
    }

    fn ensure_index(&mut self, table: &str, col: usize) {
        let cur = self.cur;
        let staged = self
            .txns
            .get(&cur)
            .map(|tx| tx.tables.contains_key(table))
            .unwrap_or(false);
        if staged {
            if let Some(t) = self.txns.get_mut(&cur).and_then(|tx| tx.tables.get_mut(table)) {
                build_index(t, col);
            }
        } else if let Some(t) = self.tables.get_mut(table) {
            // Indexes only mirror committed rows, so building one on the
            // shared image is not visible state and stays out of the txn.
            build_index(t, col);
        }
    }

    /// Make sure every index the schema declares (primary key, UNIQUE and
    /// named indexes) has its physical structure built. Without a declared
    /// index a query scans the table, which is what keeps an index a pure
    /// speed-up.
    fn ensure_declared_indexes(&mut self, lower: &str) {
        let cols: Vec<usize> = match self.table_ref(lower) {
            Some(t) => {
                let mut v: Vec<usize> = Vec::new();
                if t.sch.pk.len() == 1 {
                    v.push(t.sch.pk[0]);
                }
                for u in &t.sch.uniq {
                    if u.len() == 1 {
                        v.push(u[0]);
                    }
                }
                for d in &t.sch.idx_defs {
                    v.push(d.col);
                }
                v.sort_unstable();
                v.dedup();
                v
            }
            None => return,
        };
        for c in cols {
            if self.table_ref(lower).and_then(|t| t.idx.get(&c)).is_none() {
                self.ensure_index(lower, c);
            }
        }
    }

    /// Name of the table that owns the named index, if any.
    fn index_owner(&self, lower: &str) -> Option<String> {
        for n in self.all_table_names() {
            if let Some(t) = self.table_ref(&n) {
                if t.sch.idx_defs.iter().any(|d| d.lower == lower) {
                    return Some(n);
                }
            }
        }
        None
    }

    fn create_index(
        &mut self,
        name: &str,
        table: &str,
        col: &str,
        unique: bool,
        if_not_exists: bool,
    ) -> Result<Option<u64>, SqErr> {
        let iname = name.to_lowercase();
        let tlower = table.to_lowercase();
        // Schema names are shared: a table already called `name` wins, just
        // like the reference.
        if self.table_exists(&iname) {
            return Err(SqErr::new("syntax", format!("there is already a table named {}", name)));
        }
        if self.index_owner(&iname).is_some() {
            if if_not_exists {
                return Ok(None);
            }
            return Err(SqErr::new("exists", format!("index {} already exists", name)));
        }
        if !self.table_exists(&tlower) {
            return Err(no_table(table));
        }
        let mut t = match self.take_table(&tlower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        let ci = match t.col_index(col) {
            Some(i) => i,
            None => {
                self.put_table(&tlower, t);
                return Err(no_column(col));
            }
        };
        if unique {
            let mut seen: HashSet<Key> = HashSet::new();
            let mut dup = false;
            for ri in 0..t.rows.len() {
                let row = t.rows.get(ri);
                if let Some(k) = row.get(ci).and_then(key_of) {
                    if !seen.insert(k) {
                        dup = true;
                        break;
                    }
                }
            }
            if dup {
                let e = unique_err(&t, &[ci]);
                self.put_table(&tlower, t);
                return Err(e);
            }
        }
        if !t.idx.contains_key(&ci) {
            build_index(&mut t, ci);
        }
        let mut rec = String::new();
        rec.push_str("X\t");
        esc_field(name, &mut rec);
        rec.push('\t');
        esc_field(&t.name, &mut rec);
        rec.push('\t');
        rec.push_str(&ci.to_string());
        rec.push('\t');
        rec.push_str(if unique { "1" } else { "0" });
        let tok = self.append_record(&rec);
        t.sch.idx_defs.push(IndexDef { name: name.to_string(), lower: iname, col: ci, unique });
        self.put_table(&tlower, t);
        Ok(tok)
    }

    fn drop_index(&mut self, name: &str, if_exists: bool) -> Result<Option<u64>, SqErr> {
        let iname = name.to_lowercase();
        let owner = match self.index_owner(&iname) {
            Some(o) => o,
            None => {
                if if_exists {
                    return Ok(None);
                }
                return Err(no_table(name));
            }
        };
        let mut t = match self.take_table(&owner) {
            Some(t) => t,
            None => return Err(no_table(name)),
        };
        t.sch.idx_defs.retain(|d| d.lower != iname);
        let mut rec = String::new();
        rec.push_str("Y\t");
        esc_field(name, &mut rec);
        let tok = self.append_record(&rec);
        self.put_table(&owner, t);
        Ok(tok)
    }

    /// Update foreign keys that point at `parent_lower` so a renamed parent
    /// column follows its new name.
    fn rename_parent_col_refs(&mut self, parent_lower: &str, old: &str, new: &str) {
        let names = self.all_table_names();
        for n in names {
            let needs = self
                .table_ref(&n)
                .map(|t| {
                    t.sch
                        .fks
                        .iter()
                        .any(|fk| fk.parent == parent_lower && fk.pcols.iter().any(|p| p == old))
                })
                .unwrap_or(false);
            if !needs {
                continue;
            }
            if let Some(mut t) = self.take_table(&n) {
                for fk in &mut t.sch.fks {
                    if fk.parent == parent_lower {
                        for p in &mut fk.pcols {
                            if p == old {
                                *p = new.to_string();
                            }
                        }
                    }
                }
                self.put_table(&n, t);
            }
        }
    }

    /// Point every foreign key that named `old` as its parent at `new`.
    fn retarget_parent_refs(&mut self, old: &str, new: &str) {
        let names = self.all_table_names();
        for n in names {
            let needs = self
                .table_ref(&n)
                .map(|t| t.sch.fks.iter().any(|fk| fk.parent == old))
                .unwrap_or(false);
            if !needs {
                continue;
            }
            if let Some(mut t) = self.take_table(&n) {
                for fk in &mut t.sch.fks {
                    if fk.parent == old {
                        fk.parent = new.to_string();
                    }
                }
                self.put_table(&n, t);
            }
        }
    }

    fn alter_add(
        &mut self,
        table: &str,
        name: &str,
        ty: Ty,
        default: &Option<Expr>,
        if_not_exists: bool,
    ) -> Result<Option<u64>, SqErr> {
        let lower = table.to_lowercase();
        if !self.table_exists(&lower) {
            return Err(no_table(table));
        }
        let cl = name.to_lowercase();
        if self.table_ref(&lower).and_then(|t| t.col_index(&cl)).is_some() {
            if if_not_exists {
                return Ok(None);
            }
            return Err(SqErr::new("exists", format!("duplicate column name: {}", name)));
        }
        let def = match default {
            Some(e) => match const_eval(e) {
                Some(v) => Some(affinity(v, ty)),
                None => return Err(SqErr::syntax("near \"DEFAULT\": syntax error")),
            },
            None => None,
        };
        let mut t = match self.take_table(&lower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        t.cols.push(Column {
            name: name.to_string(),
            lower: cl,
            ty,
            q: None,
            notnull: false,
            def: def.clone(),
        });
        let fill = def.unwrap_or(Value::Null);
        t.rows.push_to_all(fill);
        let mut rec = String::new();
        rec.push_str("P\t");
        esc_field(&t.name, &mut rec);
        rec.push('\t');
        esc_field(name, &mut rec);
        rec.push('\t');
        rec.push_str(ty.code());
        rec.push('\t');
        match &t.cols.last().and_then(|c| c.def.clone()) {
            Some(v) => enc_value(v, &mut rec),
            None => rec.push('-'),
        }
        let tok = self.append_record(&rec);
        self.put_table(&lower, t);
        Ok(tok)
    }

    fn alter_drop(&mut self, table: &str, name: &str, if_exists: bool) -> Result<Option<u64>, SqErr> {
        let lower = table.to_lowercase();
        if !self.table_exists(&lower) {
            return Err(no_table(table));
        }
        let ci = match self.table_ref(&lower).and_then(|t| t.col_index(name)) {
            Some(i) => i,
            None => {
                if if_exists {
                    return Ok(None);
                }
                return Err(no_column(name));
            }
        };
        {
            let t = match self.table_ref(&lower) {
                Some(t) => t,
                None => return Err(no_table(table)),
            };
            if t.sch.pk.contains(&ci) {
                return Err(constraint_err(&format!("cannot drop PRIMARY KEY column: \"{}\"", name)));
            }
            if t.sch.uniq.iter().any(|u| u.contains(&ci)) {
                return Err(constraint_err(&format!("cannot drop UNIQUE column: \"{}\"", name)));
            }
            if t.sch.idx_defs.iter().any(|d| d.col == ci) {
                return Err(constraint_err(&format!(
                    "error in index after drop column: {}",
                    name
                )));
            }
            if t.sch.fks.iter().any(|fk| fk.cols.contains(&ci) && fk.cols.len() > 1) {
                return Err(constraint_err(&format!(
                    "error in table after drop column: unknown column in foreign key definition: {}",
                    name
                )));
            }
        }
        let mut t = match self.take_table(&lower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        let colname = t.cols[ci].name.clone();
        drop_column_at(&mut t, ci);
        let mut rec = String::new();
        rec.push_str("Q\t");
        esc_field(&t.name, &mut rec);
        rec.push('\t');
        esc_field(&colname, &mut rec);
        let tok = self.append_record(&rec);
        self.put_table(&lower, t);
        Ok(tok)
    }

    fn alter_rename_column(
        &mut self,
        table: &str,
        old: &str,
        new: &str,
    ) -> Result<Option<u64>, SqErr> {
        let lower = table.to_lowercase();
        if !self.table_exists(&lower) {
            return Err(no_table(table));
        }
        let oldl = old.to_lowercase();
        let newl = new.to_lowercase();
        let ci = match self.table_ref(&lower).and_then(|t| t.col_index(&oldl)) {
            Some(i) => i,
            None => return Err(no_column(old)),
        };
        if let Some(ni) = self.table_ref(&lower).and_then(|t| t.col_index(&newl)) {
            if ni != ci {
                return Err(SqErr::new("exists", format!("duplicate column name: {}", new)));
            }
        }
        let mut t = match self.take_table(&lower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        t.cols[ci].name = new.to_string();
        t.cols[ci].lower = newl.clone();
        let mut rec = String::new();
        rec.push_str("W\t");
        esc_field(&t.name, &mut rec);
        rec.push('\t');
        esc_field(&oldl, &mut rec);
        rec.push('\t');
        esc_field(new, &mut rec);
        let tok = self.append_record(&rec);
        self.put_table(&lower, t);
        self.rename_parent_col_refs(&lower, &oldl, &newl);
        Ok(tok)
    }

    fn alter_rename_table(&mut self, table: &str, new: &str) -> Result<Option<u64>, SqErr> {
        let lower = table.to_lowercase();
        if !self.table_exists(&lower) {
            return Err(no_table(table));
        }
        let newl = new.to_lowercase();
        if self.table_exists(&newl) || self.index_owner(&newl).is_some() {
            return Err(SqErr::new(
                "exists",
                format!("there is already another table or index with this name: {}", new),
            ));
        }
        let mut t = match self.take_table(&lower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        t.name = new.to_string();
        t.lower = newl.clone();
        let cur = self.cur;
        if let Some(tx) = self.txns.get_mut(&cur) {
            // The committed image may still hold the old key; hide it for the
            // rest of the transaction and remove it for good on COMMIT.
            tx.hidden.insert(lower.clone());
        }
        let mut rec = String::new();
        rec.push_str("V\t");
        esc_field(&lower, &mut rec);
        rec.push('\t');
        esc_field(new, &mut rec);
        let tok = self.append_record(&rec);
        self.put_table(&newl, t);
        self.retarget_parent_refs(&lower, &newl);
        Ok(tok)
    }

    fn insert(
        &mut self,
        table: &str,
        cols: Option<&Vec<String>>,
        rows: &[Vec<Expr>],
    ) -> Result<(usize, Option<u64>), SqErr> {
        let lower = table.to_lowercase();
        if !self.table_exists(&lower) {
            return Err(no_table(table));
        }
        let mut child = match self.take_table(&lower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        let res = self.insert_inner(&lower, &mut child, cols, rows);
        self.put_table(&lower, child);
        res
    }

    fn insert_inner(
        &mut self,
        lower: &str,
        t: &mut Table,
        cols: Option<&Vec<String>>,
        rows: &[Vec<Expr>],
    ) -> Result<(usize, Option<u64>), SqErr> {
        let ncols = t.cols.len();
        let targets: Vec<usize> = match cols {
            Some(list) => {
                let mut v = Vec::with_capacity(list.len());
                for c in list {
                    match t.col_index(c) {
                        Some(i) => v.push(i),
                        None => {
                            return Err(SqErr::new(
                                "no_column",
                                format!("table {} has no column named {}", t.name, c),
                            ))
                        }
                    }
                }
                v
            }
            None => (0..ncols).collect(),
        };
        // Resolve the table's foreign keys and prepare indexes before any
        // row is touched.
        let fks = self.resolve_fks(lower, t)?;
        self.ensure_fk_indexes(lower, t, &fks);
        let usets = unique_sets(t);
        ensure_unique_indexes(t, &usets);

        // INTEGER PRIMARY KEY: NULL (explicit or from an omitted column)
        // means "assign max + 1", in VALUES order.
        let rowid_pc = t.sch.rowid_pk();
        let mut running: Option<i64> = if t.sch.pk_max_ok {
            if t.rows.is_empty() {
                None
            } else {
                Some(t.sch.pk_max)
            }
        } else {
            None
        };
        if let Some(pc) = rowid_pc {
            if !t.sch.pk_max_ok {
                let mut max: Option<i64> = None;
                for ri in 0..t.rows.len() {
                    let r = t.rows.get(ri);
                    if let Some(Value::Int(v)) = r.get(pc) {
                        max = Some(match max {
                            Some(m) => m.max(*v),
                            None => *v,
                        });
                    }
                }
                t.sch.pk_max = max.unwrap_or(0);
                t.sch.pk_max_ok = true;
                running = max;
            }
        }

        // Rows are materialised, validated and logged in bounded chunks: a
        // single multi-megabyte INSERT must not hold several copies of every
        // row at once.
        const CHUNK: usize = 512;
        let total = rows.len();
        let orig_len = t.rows.len();
        let mut recs: Vec<String> = Vec::new();
        let mut done = 0usize;
        while done < total {
            if let Err(e) = tick() {
                t.rows.truncate(orig_len);
                rebuild_indexes(t);
                return Err(e);
            }
            let end = (done + CHUNK).min(total);
            let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(end - done);
            for vals in &rows[done..end] {
                if vals.len() != targets.len() {
                    return Err(err_arity(format!(
                        "table {} has {} columns but {} values were supplied",
                        t.name,
                        targets.len(),
                        vals.len()
                    )));
                }
                // Omitted columns read their DEFAULT (or NULL).
                let mut row: Vec<Value> = t
                    .cols
                    .iter()
                    .map(|c| c.def.clone().unwrap_or(Value::Null))
                    .collect();
                let mut assigned: Vec<bool> = vec![false; ncols];
                for (k, e) in vals.iter().enumerate() {
                    let v = match e {
                        Expr::Lit(v) => v.clone(),
                        Expr::Neg(inner) => match const_eval(inner) {
                            Some(Value::Null) => Value::Null,
                            Some(v) => neg_value(v),
                            None => return Err(SqErr::syntax("syntax error")),
                        },
                        Expr::Col { name, .. } => return Err(no_column(name)),
                        _ => match const_eval(e) {
                            Some(v) => v,
                            None => return Err(SqErr::syntax("syntax error")),
                        },
                    };
                    let ci = targets[k];
                    let cv = coerce(v, t.cols[ci].ty, &t.name, &t.cols[ci].name)?;
                    if !assigned[ci] {
                        assigned[ci] = true;
                        row[ci] = cv;
                    }
                }
                // A rowid alias ignores its DEFAULT: an omitted (or NULL)
                // value always means "assign max + 1".
                if let Some(pc) = rowid_pc {
                    if !assigned[pc] {
                        row[pc] = Value::Null;
                    }
                }
                out_rows.push(row);
            }

            if let Some(pc) = rowid_pc {
                for row in out_rows.iter_mut() {
                    match row.get(pc) {
                        Some(Value::Null) => {
                            let v = match running {
                                Some(m) => m.saturating_add(1),
                                None => 1,
                            };
                            row[pc] = Value::Int(v);
                            running = Some(v);
                        }
                        Some(Value::Int(v)) => {
                            let v = *v;
                            running = Some(match running {
                                Some(m) => m.max(v),
                                None => v,
                            });
                        }
                        _ => {}
                    }
                }
                if let Some(m) = running {
                    t.sch.pk_max = m;
                    t.sch.pk_max_ok = true;
                }
            }

            for row in &out_rows {
                for (ci, c) in t.cols.iter().enumerate() {
                    if c.notnull && row.get(ci) == Some(&Value::Null) {
                        t.rows.truncate(orig_len);
                        rebuild_indexes(t);
                        return Err(notnull_err(t, ci));
                    }
                }
                insert_row_indexed(t, row.clone());
                let ri = t.rows.len() - 1;
                for u in &usets {
                    if unique_violation_after(t, u, ri) {
                        t.rows.truncate(orig_len);
                        rebuild_indexes(t);
                        return Err(unique_err(t, u));
                    }
                }
            }

            // Serialise this chunk for the log.
            let mut est = 24usize;
            for row in &out_rows {
                for v in row {
                    est += match v {
                        Value::Text(x) => x.len() + 2,
                        _ => 20,
                    };
                }
            }
            let mut rec = String::with_capacity(est);
            rec.push_str("I\t");
            esc_field(&t.name, &mut rec);
            rec.push('\t');
            rec.push_str(&ncols.to_string());
            rec.push('\t');
            rec.push_str(&out_rows.len().to_string());
            for row in &out_rows {
                for v in row {
                    rec.push('\t');
                    enc_value(v, &mut rec);
                }
            }
            recs.push(rec);
            done = end;
        }

        // Immediate foreign-key constraints are checked at the end of the
        // statement, so rows in one VALUES list may reference each other in
        // either order.
        for ri in orig_len..t.rows.len() {
            let row = t.rows.get(ri);
            for fk in &fks {
                if !self.fk_exists(lower, t, fk, &row) {
                    t.rows.truncate(orig_len);
                    rebuild_indexes(t);
                    return Err(constraint_err("FOREIGN KEY constraint failed"));
                }
            }
        }

        // Make everything durable before the rows become visible outside this
        // statement (the table is out of the map while we work on it).
        let tok = self.append_records_atomic(recs);
        Ok((total, tok))
    }

    fn update_rows(
        &mut self,
        table: &str,
        sets: &[(String, Expr)],
        filter: &Option<Expr>,
    ) -> Result<(usize, Option<u64>), SqErr> {
        let lower = table.to_lowercase();
        if !self.table_exists(&lower) {
            return Err(no_table(table));
        }
        let mut t = match self.take_table(&lower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        let res = self.update_inner(&lower, &mut t, sets, filter);
        self.put_table(&lower, t);
        res
    }

    fn update_inner(
        &mut self,
        lower: &str,
        t: &mut Table,
        sets: &[(String, Expr)],
        filter: &Option<Expr>,
    ) -> Result<(usize, Option<u64>), SqErr> {
        let mut rsets: Vec<(usize, RExpr)> = Vec::with_capacity(sets.len());
        let mut changed: HashSet<usize> = HashSet::new();
        for (cname, e) in sets {
            let ci = match t.col_index(cname) {
                Some(i) => i,
                None => return Err(no_column(cname)),
            };
            rsets.push((ci, resolve_expr(e, t, self, &[])?));
            changed.insert(ci);
        }
        let rfilter = match filter {
            Some(e) => Some(resolve_expr(e, t, self, &[])?),
            None => None,
        };
        let ncols = t.cols.len();
        let mut updates: Vec<(usize, Vec<Value>)> = Vec::new();
        for ri in 0..t.rows.len() {
            tick()?;
            let hit = {
                let row = t.rows.get(ri);
                match &rfilter {
                    Some(f) => truthy(&eval(f, &row, t, &[])),
                    None => true,
                }
            };
            if !hit {
                continue;
            }
            let row = t.rows.get(ri);
            let mut nr = row.to_vec();
            for (ci, e) in &rsets {
                let v = eval(e, &row, t, &[]);
                let cv = coerce(v, t.cols[*ci].ty, &t.name, &t.cols[*ci].name)?;
                nr[*ci] = cv;
            }
            updates.push((ri, nr));
        }
        if updates.is_empty() {
            return Ok((0, None));
        }
        // SQLite visits an INTEGER PRIMARY KEY table in rowid order when the
        // statement updates more than one row; transient uniqueness checks
        // depend on that order.
        if let Some(pc) = t.sch.rowid_pk() {
            updates.sort_by_key(|(ri, _)| match t.rows.get(*ri).get(pc) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            });
        }
        let fks = self.resolve_fks(lower, t)?;
        self.ensure_fk_indexes(lower, t, &fks);
        let usets = unique_sets(t);
        ensure_unique_indexes(t, &usets);
        let rowid_pk = t.sch.rowid_pk();

        if let Some(pc) = rowid_pk {
            if changed.contains(&pc) {
                t.sch.pk_max_ok = false;
            }
        }
        let mut applied: Vec<(usize, Vec<Value>)> = Vec::with_capacity(updates.len());
        let mut fail: Option<SqErr> = None;
        'rows: for (ri, nr) in &updates {
            if let Some(pc) = rowid_pk {
                if changed.contains(&pc) && nr.get(pc) == Some(&Value::Null) {
                    fail = Some(err_type("datatype mismatch".to_string()));
                    break 'rows;
                }
            }
            for (ci, c) in t.cols.iter().enumerate() {
                if c.notnull && nr.get(ci) == Some(&Value::Null) {
                    fail = Some(notnull_err(t, ci));
                    break 'rows;
                }
            }
            let old = t.rows.get(*ri).to_vec();
            update_row_at(t, *ri, nr.clone());
            applied.push((*ri, old));
            for u in &usets {
                if unique_violation_after(t, u, *ri) {
                    fail = Some(unique_err(t, u));
                    break 'rows;
                }
            }
            for fk in &fks {
                if !self.fk_exists(lower, t, fk, nr) {
                    fail = Some(constraint_err("FOREIGN KEY constraint failed"));
                    break 'rows;
                }
            }
        }
        if fail.is_none() {
            match self.parent_cols_changed(lower, t, &changed) {
                Ok(true) => {
                    if let Err(e) = self.check_children(lower, t, None) {
                        fail = Some(e);
                    }
                }
                Ok(false) => {}
                Err(e) => fail = Some(e),
            }
        }
        if let Some(e) = fail {
            for (ri, old) in applied.into_iter().rev() {
                update_row_at(t, ri, old);
            }
            rebuild_indexes(t);
            return Err(e);
        }
        let mut rec = String::new();
        rec.push_str("U\t");
        esc_field(&t.name, &mut rec);
        rec.push('\t');
        rec.push_str(&ncols.to_string());
        rec.push('\t');
        rec.push_str(&updates.len().to_string());
        for (ri, row) in &updates {
            rec.push('\t');
            rec.push_str(&ri.to_string());
            for v in row {
                rec.push('\t');
                enc_value(v, &mut rec);
            }
        }
        let matched = updates.len();
        let tok = self.append_record(&rec);
        Ok((matched, tok))
    }

    /// True when some foreign key points at `lower` through a column that
    /// this UPDATE changes (only then can the update orphan a child).
    fn parent_cols_changed(
        &self,
        lower: &str,
        t: &Table,
        changed: &HashSet<usize>,
    ) -> Result<bool, SqErr> {
        let mut names = self.all_table_names();
        if !names.iter().any(|n| n == lower) {
            names.push(lower.to_string());
        }
        for cname in &names {
            let child: &Table = if cname == lower {
                t
            } else {
                match self.table_ref(cname) {
                    Some(c) => c,
                    None => continue,
                }
            };
            for fk in &child.sch.fks {
                if fk.parent == lower {
                    let r = resolve_one_fk(cname, fk, t)?;
                    if r.pcols.iter().any(|c| changed.contains(c)) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn delete_rows(
        &mut self,
        table: &str,
        filter: &Option<Expr>,
    ) -> Result<(usize, Option<u64>), SqErr> {
        let lower = table.to_lowercase();
        let t = match self.table_ref(&lower) {
            Some(t) => t,
            None => return Err(no_table(table)),
        };
        let rfilter = match filter {
            Some(e) => Some(resolve_expr(e, t, self, &[])?),
            None => None,
        };
        let mut del: Vec<usize> = Vec::new();
        for ri in 0..t.rows.len() {
            tick()?;
            let hit = {
                let row = t.rows.get(ri);
                match &rfilter {
                    Some(f) => truthy(&eval(f, &row, t, &[])),
                    None => true,
                }
            };
            if hit {
                del.push(ri);
            }
        }
        if del.is_empty() {
            return Ok((0, None));
        }
        let total = t.rows.len();
        {
            let mut skip = vec![false; total];
            for &ri in &del {
                skip[ri] = true;
            }
            self.check_children(&lower, t, Some(&skip))?;
        }
        let ndel = del.len();
        let remaining = total - ndel;
        let ncols = t.cols.len();
        let tname = t.name.clone();
        let full_replace = ndel > remaining;
        let tok = if full_replace {
            let mut drop = vec![false; total];
            for &ri in &del {
                drop[ri] = true;
            }
            let mut rec = String::new();
            rec.push_str("E\t");
            esc_field(&tname, &mut rec);
            rec.push('\t');
            rec.push_str(&ncols.to_string());
            rec.push('\t');
            rec.push_str(&remaining.to_string());
            for ri in 0..t.rows.len() {
                if drop[ri] {
                    continue;
                }
                let row = t.rows.get(ri);
                for v in row.iter() {
                    rec.push('\t');
                    enc_value(v, &mut rec);
                }
            }
            let tok = self.append_record(&rec);
            let t = self.table_mut(&lower).unwrap();
            t.rows.retain(|ri, _| !drop[ri]);
            rebuild_indexes(t);
            tok
        } else {
            let mut rec = String::new();
            rec.push_str("D\t");
            esc_field(&tname, &mut rec);
            rec.push('\t');
            rec.push_str(&ndel.to_string());
            for ri in &del {
                rec.push('\t');
                rec.push_str(&ri.to_string());
            }
            let tok = self.append_record(&rec);
            let t = self.table_mut(&lower).unwrap();
            let mut drop = vec![false; total];
            for &ri in &del {
                drop[ri] = true;
            }
            t.rows.retain(|ri, _| !drop[ri]);
            rebuild_indexes(t);
            tok
        };
        Ok((ndel, tok))
    }

    fn maybe_compact(&mut self) {
        if self.wal_bytes < WAL_COMPACT_BYTES {
            return;
        }
        if self.writer.is_some() {
            // A write transaction has committed state frozen; compacting
            // now would be harmless but pointless, so wait for its end.
            return;
        }
        let new_gen = self.gen + 1;
        let tmp = self.snap_path.with_extension("tmp");
        let ok = (|| -> std::io::Result<()> {
            let f = File::create(&tmp)?;
            let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
            use std::io::Write as _;
            writeln!(w, "G\t{}", new_gen)?;
            for t in self.tables.values() {
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
                rec.push('\n');
                w.write_all(rec.as_bytes())?;
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
                    rec.push('\n');
                    w.write_all(rec.as_bytes())?;
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
                    rec.push('\n');
                    w.write_all(rec.as_bytes())?;
                }
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
            return;
        }
        // New generation WAL.
        let tmp_wal = self.wal_path.with_extension("waltmp");
        if fs::write(&tmp_wal, format!("G\t{}\n", new_gen)).is_err()
            || fs::rename(&tmp_wal, &self.wal_path).is_err()
        {
            return;
        }
        if let Some(w) = &self.wal {
            let mut g = w.lock();
            while g.syncing || g.durable < g.seq {
                g = w.wait(g);
            }
            let hlen = format!("G\t{}\n", new_gen).len() as u64;
            if let Ok(f) = OpenOptions::new().read(true).write(true).create(true).open(&self.wal_path) {
                g.file = Arc::new(f);
                g.segs.clear();
                g.pos = hlen;
                g.cap = 0;
            }
            drop(g);
            w.cv.notify_all();
        }
        self.gen = new_gen;
        self.wal_bytes = format!("G\t{}\n", new_gen).len() as u64;
    }
}

/// Remove column `ci` from a table's rows and from every index/constraint it
/// stores by position. Callers must already have rejected dropping a column
/// that a constraint forbids removing.
fn drop_column_at(t: &mut Table, ci: usize) {
    if ci >= t.cols.len() {
        return;
    }
    t.cols.remove(ci);
    t.rows.drop_value_from_all(ci);
    let shift = |v: &mut usize| {
        if *v > ci {
            *v -= 1;
        }
    };
    for p in &mut t.sch.pk {
        shift(p);
    }
    for u in &mut t.sch.uniq {
        for p in u.iter_mut() {
            shift(p);
        }
    }
    let mut fks: Vec<Fk> = Vec::with_capacity(t.sch.fks.len());
    for mut fk in std::mem::take(&mut t.sch.fks) {
        fk.cols.retain(|p| *p != ci);
        for p in &mut fk.cols {
            shift(p);
        }
        // A foreign key whose only child column disappeared no longer exists.
        if !fk.cols.is_empty() {
            fks.push(fk);
        }
    }
    t.sch.fks = fks;
    for d in &mut t.sch.idx_defs {
        shift(&mut d.col);
    }
    // Physical indexes are keyed by column position; shift their keys too.
    let keys: Vec<usize> = t.idx.keys().copied().collect();
    for k in keys {
        if k == ci {
            t.idx.remove(&k);
            continue;
        }
        if k > ci {
            if let Some(ix) = t.idx.remove(&k) {
                t.idx.insert(k - 1, ix);
            }
        }
    }
    rebuild_indexes(t);
}

fn insert_row_indexed(t: &mut Table, row: Vec<Value>) {
    let ri = t.rows.len() as u32;
    if let Some(pc) = t.sch.rowid_pk() {
        if let Some(Value::Int(v)) = row.get(pc) {
            if t.rows.is_empty() {
                t.sch.pk_max = *v;
                t.sch.pk_max_ok = true;
            } else if t.sch.pk_max_ok && *v > t.sch.pk_max {
                t.sch.pk_max = *v;
            }
        }
    }
    if !t.idx.is_empty() {
        for (col, ix) in t.idx.iter_mut() {
            match row.get(*col).and_then(key_of) {
                Some(k) => ix.map.entry(k).or_default().push(ri),
                None => ix.nulls.push(ri),
            }
        }
    }
    t.rows.push(row);
}

fn build_index(t: &mut Table, col: usize) {
    if t.idx.contains_key(&col) {
        return;
    }
    let mut ix = PhysIndex::default();
    for ri in 0..t.rows.len() {
        let row = t.rows.get(ri);
        match row.get(col).and_then(key_of) {
            Some(k) => ix.map.entry(k).or_default().push(ri as u32),
            None => ix.nulls.push(ri as u32),
        }
    }
    t.idx.insert(col, ix);
}

fn rebuild_indexes(t: &mut Table) {
    t.sch.pk_max_ok = false;
    if t.idx.is_empty() {
        return;
    }
    let cols: Vec<usize> = t.idx.keys().copied().collect();
    for c in cols {
        let mut ix = PhysIndex::default();
        for ri in 0..t.rows.len() {
            let row = t.rows.get(ri);
            match row.get(c).and_then(key_of) {
                Some(k) => ix.map.entry(k).or_default().push(ri as u32),
                None => ix.nulls.push(ri as u32),
            }
        }
        t.idx.insert(c, ix);
    }
}

fn update_row_at(t: &mut Table, ri: usize, row: Vec<Value>) {
    if t.idx.is_empty() {
        t.rows.set(ri, row);
        return;
    }
    let cols: Vec<usize> = t.idx.keys().copied().collect();
    let old: Vec<Option<Key>> = cols.iter().map(|&c| key_of(&t.rows.get(ri)[c])).collect();
    t.rows.set(ri, row);
    for (k, &c) in cols.iter().enumerate() {
        let newk = key_of(&t.rows.get(ri)[c]);
        if old[k] == newk {
            continue;
        }
        let ix = match t.idx.get_mut(&c) {
            Some(i) => i,
            None => continue,
        };
        if let Some(ref ok) = old[k] {
            let mut empty = false;
            if let Some(v) = ix.map.get_mut(ok) {
                v.retain(|x| *x != ri as u32);
                empty = v.is_empty();
            }
            if empty {
                ix.map.remove(ok);
            }
        } else if let Some(p) = ix.nulls.iter().position(|x| *x == ri as u32) {
            ix.nulls.swap_remove(p);
        }
        if let Some(nk) = newk {
            ix.map.entry(nk).or_default().push(ri as u32);
        } else {
            ix.nulls.push(ri as u32);
        }
    }
}

/// Resolve one declared foreign key against its parent table.
fn resolve_one_fk(child_name: &str, fk: &Fk, parent: &Table) -> Result<RFk, SqErr> {
    let pcols: Vec<usize> = if fk.pcols.is_empty() {
        parent.sch.pk.clone()
    } else {
        let mut v = Vec::with_capacity(fk.pcols.len());
        for n in &fk.pcols {
            match parent.col_index(n) {
                Some(i) => v.push(i),
                None => {
                    return Err(SqErr::new(
                        "syntax",
                        format!(
                            "foreign key mismatch - \"{}\" referencing \"{}\"",
                            child_name, fk.parent
                        ),
                    ))
                }
            }
        }
        v
    };
    let mismatch = |()| {
        SqErr::new(
            "syntax",
            format!(
                "foreign key mismatch - \"{}\" referencing \"{}\"",
                child_name, fk.parent
            ),
        )
    };
    if pcols.is_empty() || pcols.len() != fk.cols.len() {
        return Err(mismatch(()));
    }
    let mut want = pcols.clone();
    want.sort_unstable();
    let mut ok = {
        let mut k = parent.sch.pk.clone();
        k.sort_unstable();
        k == want
    };
    if !ok {
        for u in &parent.sch.uniq {
            let mut k = u.clone();
            k.sort_unstable();
            if k == want {
                ok = true;
                break;
            }
        }
    }
    if !ok {
        return Err(mismatch(()));
    }
    Ok(RFk { cols: fk.cols.clone(), parent: fk.parent.clone(), pcols })
}

/// All uniqueness constraints of a table: the primary key first, then the
/// declared UNIQUE constraints.
fn unique_sets(t: &Table) -> Vec<Vec<usize>> {
    let mut v: Vec<Vec<usize>> = Vec::with_capacity(1 + t.sch.uniq.len() + t.sch.idx_defs.len());
    if !t.sch.pk.is_empty() {
        v.push(t.sch.pk.clone());
    }
    for u in &t.sch.uniq {
        v.push(u.clone());
    }
    for d in &t.sch.idx_defs {
        if d.unique {
            v.push(vec![d.col]);
        }
    }
    v
}

fn ensure_unique_indexes(t: &mut Table, sets: &[Vec<usize>]) {
    for u in sets {
        if u.len() == 1 {
            build_index(t, u[0]);
        }
    }
}

/// Check whether the row just written at `ri` collides with an older row
/// under one uniqueness constraint. NULL keys never collide.
fn unique_violation_after(t: &Table, u: &[usize], ri: usize) -> bool {
    if ri >= t.rows.len() {
        return false;
    }
    let row = t.rows.get(ri);
    for &c in u {
        if row.get(c) == Some(&Value::Null) {
            return false;
        }
    }
    if u.len() == 1 {
        let c = u[0];
        let key = match row.get(c).and_then(key_of) {
            Some(k) => k,
            None => return false,
        };
        if let Some(ix) = t.idx.get(&c) {
            return ix.map.get(&key).map(|v| v.len() > 1).unwrap_or(false);
        }
        let mut n = 0usize;
        for j in 0..t.rows.len() {
            let r = t.rows.get(j);
            if r.get(c).and_then(key_of).as_ref() == Some(&key) {
                n += 1;
                if n > 1 {
                    return true;
                }
            }
        }
        return false;
    }
    let mut n = 0usize;
    for j in 0..t.rows.len() {
        let r = t.rows.get(j);
        let mut same = true;
        for &c in u {
            if r.get(c).and_then(key_of).is_none() {
                same = false;
                break;
            }
            if r.get(c) != row.get(c) {
                same = false;
                break;
            }
        }
        if same {
            n += 1;
            if n > 1 {
                return true;
            }
        }
    }
    false
}

fn notnull_err(t: &Table, ci: usize) -> SqErr {
    constraint_err(&format!("NOT NULL constraint failed: {}.{}", t.name, t.cols[ci].name))
}

fn unique_err(t: &Table, u: &[usize]) -> SqErr {
    let names: Vec<String> = u.iter().map(|&c| format!("{}.{}", t.name, t.cols[c].name)).collect();
    constraint_err(&format!("UNIQUE constraint failed: {}", names.join(", ")))
}

fn const_eval(e: &Expr) -> Option<Value> {
    match e {
        Expr::Lit(v) => Some(v.clone()),
        Expr::Neg(x) => match const_eval(x)? {
            Value::Null => Some(Value::Null),
            v => Some(neg_value(v)),
        },
        Expr::Bin(op, a, b) => {
            let av = const_eval(a)?;
            let bv = const_eval(b)?;
            Some(arith(*op, av, bv))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- queries ---

struct SelPlan {
    distinct: bool,
    headers: Vec<String>,
    items: Vec<RExpr>,
    filter: Option<RExpr>,
    order: Vec<(RExpr, bool)>,
    limit: Option<usize>,
    offset: usize,
}

/// A constant scalar expression (literals and arithmetic over them), used to
/// turn `col = <expr>` into an index probe.
fn const_rvalue(e: &RExpr) -> Option<Value> {
    match e {
        RExpr::Lit(v) => Some(v.clone()),
        RExpr::Neg(x) => const_rvalue(x).map(neg_value),
        RExpr::Bin(op, a, b) => {
            let av = const_rvalue(a)?;
            let bv = const_rvalue(b)?;
            Some(arith(*op, av, bv))
        }
        _ => None,
    }
}

fn flip_op(op: Op) -> Op {
    match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        other => other,
    }
}

/// Bounds on one indexed column derived from a filter's top-level conjunctions.
#[derive(Clone, Debug, Default)]
struct RangeSpec {
    lo: Option<(Key, bool)>,
    hi: Option<(Key, bool)>,
    null_only: bool,
    no_nulls: bool,
}

impl RangeSpec {
    fn set_lo(&mut self, k: Key, inc: bool) {
        let better = match &self.lo {
            None => true,
            Some((cur, cur_inc)) => match k.cmp(cur) {
                Ordering::Greater => true,
                Ordering::Equal => inc && !*cur_inc,
                Ordering::Less => false,
            },
        };
        if better {
            self.lo = Some((k, inc));
        }
    }
    fn set_hi(&mut self, k: Key, inc: bool) {
        let better = match &self.hi {
            None => true,
            Some((cur, cur_inc)) => match k.cmp(cur) {
                Ordering::Less => true,
                Ordering::Equal => inc && !*cur_inc,
                Ordering::Greater => false,
            },
        };
        if better {
            self.hi = Some((k, inc));
        }
    }
    fn is_eq(&self) -> bool {
        match (&self.lo, &self.hi) {
            (Some((a, true)), Some((b, true))) => a == b,
            _ => false,
        }
    }
}

fn collect_conjuncts<'a>(e: &'a RExpr, out: &mut Vec<&'a RExpr>) {
    if let RExpr::Bin(Op::And, a, b) = e {
        collect_conjuncts(a, out);
        collect_conjuncts(b, out);
    } else {
        out.push(e);
    }
}

/// Derive index bounds from `f` for every column that already has a physical
/// index. Only plain `col <op> <const>` conjuncts and `IS [NOT] NULL` are
/// used; anything else leaves the column unconstrained.
fn range_spec_for(t: &Table, f: &RExpr) -> HashMap<usize, RangeSpec> {
    let mut out: HashMap<usize, RangeSpec> = HashMap::new();
    let mut conj: Vec<&RExpr> = Vec::new();
    collect_conjuncts(f, &mut conj);
    for c in conj {
        match c {
            RExpr::Bin(op, a, b) => {
                let (col, val, flip) = match (&**a, &**b) {
                    (RExpr::Col(ci), other) => match const_rvalue(other) {
                        Some(v) => (*ci, v, false),
                        None => continue,
                    },
                    (other, RExpr::Col(ci)) => match const_rvalue(other) {
                        Some(v) => (*ci, v, true),
                        None => continue,
                    },
                    _ => continue,
                };
                if !t.idx.contains_key(&col) {
                    continue;
                }
                let v = affinity(val, t.cols[col].ty);
                let k = match key_of(&v) {
                    Some(k) => k,
                    None => continue,
                };
                // Only bound the index when the constant lives in the same
                // value domain as the column; a cross-domain comparison has
                // its own ordering rules and is left to the full scan.
                match (t.cols[col].ty, &k) {
                    (Ty::Int, Key::I(_)) | (Ty::Text, Key::T(_)) => {}
                    _ => continue,
                }
                let op = if flip { flip_op(*op) } else { *op };
                let e = out.entry(col).or_insert_with(RangeSpec::default);
                match op {
                    Op::Eq => {
                        e.set_lo(k.clone(), true);
                        e.set_hi(k, true);
                    }
                    Op::Gt => e.set_lo(k, false),
                    Op::Ge => e.set_lo(k, true),
                    Op::Lt => e.set_hi(k, false),
                    Op::Le => e.set_hi(k, true),
                    _ => {}
                }
            }
            RExpr::IsNull(inner, not_null) => {
                if let RExpr::Col(col) = &**inner {
                    if t.idx.contains_key(col) {
                        let e = out.entry(*col).or_insert_with(RangeSpec::default);
                        // The flag is "IS NOT NULL" (see `eval_g`).
                        if *not_null {
                            e.no_nulls = true;
                        } else {
                            e.null_only = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Pick the most selective usable index constraint.
fn pick_range(specs: &HashMap<usize, RangeSpec>) -> Option<(usize, &RangeSpec)> {
    let mut best: Option<(usize, &RangeSpec, u8)> = None;
    for (c, s) in specs {
        let score = if s.is_eq() {
            4
        } else if s.null_only {
            1
        } else if s.lo.is_some() && s.hi.is_some() {
            3
        } else if s.lo.is_some() || s.hi.is_some() {
            2
        } else {
            0
        };
        if score > 0 {
            let take = match &best {
                Some((_, _, b)) => score > *b,
                None => true,
            };
            if take {
                best = Some((*c, s, score));
            }
        }
    }
    best.map(|(c, s, _)| (c, s))
}

/// True when the derived lower/upper bounds can never match any value.
/// `BTreeMap::range` panics on an inverted or empty-exclusive range, so this
/// must be checked before iterating (contradictory filters such as
/// `k = 1 AND k = 2` are legal SQL and simply select nothing).
fn range_is_empty(lo: Option<&(Key, bool)>, hi: Option<&(Key, bool)>) -> bool {
    match (lo, hi) {
        (Some((a, ainc)), Some((b, binc))) => match a.cmp(b) {
            Ordering::Greater => true,
            Ordering::Equal => !(*ainc && *binc),
            Ordering::Less => false,
        },
        _ => false,
    }
}

fn collect_range(ix: &PhysIndex, s: &RangeSpec) -> Vec<u32> {
    let mut out = Vec::new();
    if s.null_only {
        if s.lo.is_none() && s.hi.is_none() {
            out.extend_from_slice(&ix.nulls);
        }
        return out;
    }
    if range_is_empty(s.lo.as_ref(), s.hi.as_ref()) {
        return out;
    }
    let lo = match &s.lo {
        Some((k, true)) => std::ops::Bound::Included(k.clone()),
        Some((k, false)) => std::ops::Bound::Excluded(k.clone()),
        None => std::ops::Bound::Unbounded,
    };
    let hi = match &s.hi {
        Some((k, true)) => std::ops::Bound::Included(k.clone()),
        Some((k, false)) => std::ops::Bound::Excluded(k.clone()),
        None => std::ops::Bound::Unbounded,
    };
    for (_k, v) in ix.map.range((lo, hi)) {
        out.extend_from_slice(v);
    }
    out
}

/// Candidate rows for a filter: an index range when one applies, otherwise
/// every row. The full predicate is still verified by the caller, so the
/// index can only narrow the work, never change the answer.
fn candidate_ids(
    t: &Table,
    filter: Option<&RExpr>,
    outers: &[&[Value]],
) -> Result<Vec<u32>, SqErr> {
    let mut cands: Vec<u32> = match filter {
        Some(f) => {
            let specs = range_spec_for(t, f);
            match pick_range(&specs) {
                Some((c, s)) => match t.idx.get(&c) {
                    Some(ix) => collect_range(ix, s),
                    None => (0..t.rows.len() as u32).collect(),
                },
                None => (0..t.rows.len() as u32).collect(),
            }
        }
        None => (0..t.rows.len() as u32).collect(),
    };
    if let Some(f) = filter {
        let mut kept: Vec<u32> = Vec::with_capacity(cands.len());
        for ri in cands {
            tick()?;
            let row = t.rows.get(ri as usize);
            if truthy(&eval(f, &row, t, outers)) {
                kept.push(ri);
            }
        }
        cands = kept;
    }
    Ok(cands)
}

fn star_columns(sel: &Select, t: &Table, qual: &Option<String>) -> Result<Vec<usize>, SqErr> {
    match qual {
        None => Ok((0..t.cols.len()).collect()),
        Some(q) => {
            let lq = q.to_lowercase();
            if t.cols.iter().any(|c| c.q.is_some()) {
                let v: Vec<usize> = t
                    .cols
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.q.as_deref() == Some(lq.as_str()))
                    .map(|(i, _)| i)
                    .collect();
                if v.is_empty() {
                    return Err(no_table(q));
                }
                Ok(v)
            } else {
                let valid = match &sel.from_alias {
                    Some(a) => a.eq_ignore_ascii_case(q),
                    None => t.lower == lq,
                };
                if !valid {
                    return Err(no_table(q));
                }
                Ok((0..t.cols.len()).collect())
            }
        }
    }
}

fn build_plan(sel: &Select, t: &Table, db: &Db, outers: &[&Table]) -> Result<SelPlan, SqErr> {
    let mut headers = Vec::new();
    let mut items = Vec::new();
    let mut alias_names: Vec<Option<String>> = Vec::new();
    for it in &sel.items {
        match it {
            SelItem::Star(q) => {
                for i in star_columns(sel, t, q)? {
                    headers.push(t.cols[i].name.clone());
                    items.push(RExpr::Col(i));
                    alias_names.push(None);
                }
            }
            SelItem::Expr { e, header, alias } => {
                let re = resolve_expr(e, t, db, outers)?;
                if !*alias {
                    if let RExpr::Col(ci) = &re {
                        headers.push(t.cols[*ci].name.clone());
                        items.push(re);
                        alias_names.push(None);
                        continue;
                    }
                }
                headers.push(header.clone());
                alias_names.push(if *alias { Some(header.to_lowercase()) } else { None });
                items.push(re);
            }
        }
    }
    let filter = match &sel.filter {
        Some(e) => Some(resolve_expr(e, t, db, outers)?),
        None => None,
    };
    let mut order = Vec::new();
    for (e, desc) in &sel.order {
        let mut re: Option<RExpr> = None;
        if let Expr::Lit(Value::Int(n)) = e {
            let n = *n;
            if n >= 1 && (n as usize) <= items.len() {
                re = Some(items[n as usize - 1].clone());
            }
        }
        if re.is_none() {
            re = alias_lookup_explicit(e, &alias_names, &items);
        }
        let re = match re {
            Some(r) => r,
            None => resolve_expr(e, t, db, outers)?,
        };
        order.push((re, *desc));
    }
    Ok(SelPlan { distinct: sel.distinct, headers, items, filter, order, limit: sel.limit, offset: sel.offset })
}

fn emit_row(t: &Table, p: &SelPlan, row: &[Value], out: &mut String, outers: &[&[Value]]) {
    for (k, item) in p.items.iter().enumerate() {
        if k > 0 {
            out.push('\t');
        }
        let v = eval(item, row, t, outers);
        push_value(&v, out);
    }
    out.push('\n');
}

fn plan_header(p: &SelPlan) -> String {
    let mut out = String::with_capacity(64 + p.headers.len() * 8);
    out.push_str(&format!("ROWS {}\n", p.headers.len()));
    for (i, h) in p.headers.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        out.push_str(h);
    }
    out.push('\n');
    out
}

/// Emit one row for the streaming `ORDER BY <indexed col>` path. Returns
/// false once the LIMIT is reached.
fn stream_try_row(
    t: &Table,
    p: &SelPlan,
    outers: &[&[Value]],
    ri: u32,
    out: &mut String,
    count: &mut usize,
    skipped: &mut usize,
    seen: &mut HashSet<String>,
) -> Result<bool, SqErr> {
    tick()?;
    if ri as usize >= t.rows.len() {
        return Ok(true);
    }
    let row = t.rows.get(ri as usize);
    if let Some(f) = &p.filter {
        if !truthy(&eval(f, &row, t, outers)) {
            return Ok(true);
        }
    }
    let mut line = String::new();
    emit_row(t, p, &row, &mut line, outers);
    if p.distinct && !seen.insert(line.clone()) {
        return Ok(true);
    }
    if *skipped < p.offset {
        *skipped += 1;
        return Ok(true);
    }
    if let Some(l) = p.limit {
        if *count >= l {
            return Ok(false);
        }
    }
    out.push_str(&line);
    *count += 1;
    Ok(match p.limit {
        Some(l) => *count < l,
        None => true,
    })
}

/// `ORDER BY <col> [ASC|DESC]` over an indexed column can be answered by
/// walking the index in key order, so a LIMIT stops after a few rows. NULLs
/// sort first ascending and last descending, exactly like `sort_cmp`.
fn try_index_order(t: &Table, p: &SelPlan, outers: &[&[Value]]) -> Result<Option<String>, SqErr> {
    if p.order.len() != 1 {
        return Ok(None);
    }
    let (oe, desc) = &p.order[0];
    let c = match oe {
        RExpr::Col(c) => *c,
        _ => return Ok(None),
    };
    let ix = match t.idx.get(&c) {
        Some(ix) => ix,
        None => return Ok(None),
    };
    // DISTINCT only keeps the first row of each output line. If the ordering
    // column is not itself projected, streaming in key order would pick a
    // different survivor than the stable row-order pass, so leave that shape
    // to the general path.
    if p.distinct && !p.items.iter().any(|it| matches!(it, RExpr::Col(ci) if *ci == c)) {
        return Ok(None);
    }
    let spec = match &p.filter {
        Some(f) => range_spec_for(t, f).remove(&c),
        None => None,
    };
    let mut out = plan_header(p);
    let mut count = 0usize;
    let mut skipped = 0usize;
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(s) = &spec {
        if s.null_only {
            if s.lo.is_none() && s.hi.is_none() {
                for &ri in &ix.nulls {
                    if !stream_try_row(t, p, outers, ri, &mut out, &mut count, &mut skipped, &mut seen)?
                    {
                        break;
                    }
                }
            }
            out.push_str(&format!("END {}\n", count));
            return Ok(Some(out));
        }
    }
    let use_nulls = match &spec {
        Some(s) => s.lo.is_none() && s.hi.is_none() && !s.no_nulls,
        None => true,
    };
    let lo = match spec.as_ref().and_then(|s| s.lo.as_ref()) {
        Some((k, true)) => std::ops::Bound::Included(k.clone()),
        Some((k, false)) => std::ops::Bound::Excluded(k.clone()),
        None => std::ops::Bound::Unbounded,
    };
    let hi = match spec.as_ref().and_then(|s| s.hi.as_ref()) {
        Some((k, true)) => std::ops::Bound::Included(k.clone()),
        Some((k, false)) => std::ops::Bound::Excluded(k.clone()),
        None => std::ops::Bound::Unbounded,
    };
    let range_empty = range_is_empty(
        spec.as_ref().and_then(|s| s.lo.as_ref()),
        spec.as_ref().and_then(|s| s.hi.as_ref()),
    );
    if !*desc {
        if use_nulls {
            for &ri in &ix.nulls {
                if !stream_try_row(t, p, outers, ri, &mut out, &mut count, &mut skipped, &mut seen)? {
                    out.push_str(&format!("END {}\n", count));
                    return Ok(Some(out));
                }
            }
        }
        if !range_empty {
            'outer: for (_k, v) in ix.map.range((lo, hi)) {
                for &ri in v {
                    if !stream_try_row(t, p, outers, ri, &mut out, &mut count, &mut skipped, &mut seen)? {
                        break 'outer;
                    }
                }
            }
        }
    } else {
        if !range_empty {
            'outer: for (_k, v) in ix.map.range((lo, hi)).rev() {
                for &ri in v {
                    if !stream_try_row(t, p, outers, ri, &mut out, &mut count, &mut skipped, &mut seen)? {
                        break 'outer;
                    }
                }
            }
        }
        if use_nulls && p.limit.map(|l| count < l).unwrap_or(true) {
            for &ri in &ix.nulls {
                if !stream_try_row(t, p, outers, ri, &mut out, &mut count, &mut skipped, &mut seen)? {
                    break;
                }
            }
        }
    }
    out.push_str(&format!("END {}\n", count));
    Ok(Some(out))
}

fn run_plan(t: &Table, p: &SelPlan, outers: &[&[Value]]) -> Result<String, SqErr> {
    if let Some(resp) = try_index_order(t, p, outers)? {
        return Ok(resp);
    }
    // Candidate rows.
    let mut cands: Vec<u32> = candidate_ids(t, p.filter.as_ref(), outers)?;

    if p.distinct {
        let mut seen: HashSet<String> = HashSet::with_capacity(cands.len());
        let mut uniq: Vec<u32> = Vec::with_capacity(cands.len());
        let mut buf = String::new();
        for &ri in &cands {
            tick()?;
            buf.clear();
            let row = t.rows.get(ri as usize);
            emit_row(t, p, &row, &mut buf, outers);
            if seen.insert(std::mem::take(&mut buf)) {
                uniq.push(ri);
            }
        }
        cands = uniq;
    }

    let mut out = String::with_capacity(64 + cands.len() * 16);
    out.push_str(&format!("ROWS {}\n", p.headers.len()));
    for (i, h) in p.headers.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        out.push_str(h);
    }
    out.push('\n');

    let mut count: usize = 0;
    if p.order.is_empty() {
        let mut skipped = 0usize;
        for &ri in &cands {
            tick()?;
            if skipped < p.offset {
                skipped += 1;
                continue;
            }
            if let Some(l) = p.limit {
                if count >= l {
                    break;
                }
            }
            let row = t.rows.get(ri as usize);
            emit_row(t, p, &row, &mut out, outers);
            count += 1;
        }
    } else {
        // A LIMIT bounds how many rows can matter: keep only the best
        // `limit + offset` in a bounded heap so the sort never buffers the
        // whole table.
        let cap = match p.limit {
            Some(l) => l.saturating_add(p.offset),
            None => usize::MAX,
        };
        let mut best: Vec<(Vec<Value>, u32)> = Vec::new();
        if cap == usize::MAX || cap > 1_000_000 {
            let mut keyed: Vec<(Vec<Value>, u32)> = Vec::with_capacity(cands.len());
            for &ri in &cands {
                tick()?;
                let row = t.rows.get(ri as usize);
                let mut ks = Vec::with_capacity(p.order.len());
                for (e, _) in &p.order {
                    ks.push(eval(e, &row, t, outers));
                }
                keyed.push((ks, ri));
            }
            tick()?;
            keyed.sort_by(|a, b| order_cmp(p, a, b));
            tick()?;
            best = keyed;
        } else {
            best.reserve(cap.min(4096));
            let mut full = false;
            for &ri in &cands {
                tick()?;
                if cap == 0 {
                    break;
                }
                let row = t.rows.get(ri as usize);
                let mut ks = Vec::with_capacity(p.order.len());
                for (e, _) in &p.order {
                    ks.push(eval(e, &row, t, outers));
                }
                let item = (ks, ri);
                if !full {
                    best.push(item);
                    if best.len() >= cap {
                        heapify(p, &mut best);
                        full = true;
                    }
                } else if order_cmp(p, &item, &best[0]) == Ordering::Less {
                    best[0] = item;
                    heap_sift_down(p, &mut best, 0);
                }
            }
            tick()?;
            best.sort_by(|a, b| order_cmp(p, a, b));
            tick()?;
        }
        for (_, ri) in best.iter().skip(p.offset) {
            if let Some(l) = p.limit {
                if count >= l {
                    break;
                }
            }
            let row = t.rows.get(*ri as usize);
            emit_row(t, p, &row, &mut out, outers);
            count += 1;
        }
    }
    out.push_str(&format!("END {}\n", count));
    Ok(out)
}

/// Compare two keyed candidate rows under a plan's ORDER BY list.
fn order_cmp(p: &SelPlan, a: &(Vec<Value>, u32), b: &(Vec<Value>, u32)) -> Ordering {
    for (i, (_, desc)) in p.order.iter().enumerate() {
        let mut c = sort_cmp(&a.0[i], &b.0[i]);
        if *desc {
            c = c.reverse();
        }
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

/// Turn `h` into a max-heap: the root is the worst row currently kept.
fn heapify(p: &SelPlan, h: &mut Vec<(Vec<Value>, u32)>) {
    if h.len() < 2 {
        return;
    }
    let mut i = h.len() / 2;
    while i > 0 {
        i -= 1;
        heap_sift_down(p, h, i);
    }
}

fn heap_sift_down(p: &SelPlan, h: &mut Vec<(Vec<Value>, u32)>, mut i: usize) {
    let n = h.len();
    loop {
        let l = 2 * i + 1;
        let r = l + 1;
        let mut worst = i;
        if l < n && order_cmp(p, &h[l], &h[worst]) == Ordering::Greater {
            worst = l;
        }
        if r < n && order_cmp(p, &h[r], &h[worst]) == Ordering::Greater {
            worst = r;
        }
        if worst == i {
            return;
        }
        h.swap(i, worst);
        i = worst;
    }
}

// ------------------------------------------------------- aggregate plans ---

fn expr_has_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg(..) => true,
        Expr::Lit(_) | Expr::Col { .. } => false,
        Expr::Neg(x) | Expr::Not(x) | Expr::IsNull(x, _) => expr_has_agg(x),
        Expr::Bin(_, a, b) => expr_has_agg(a) || expr_has_agg(b),
        Expr::Fn(_, args) => args.iter().any(expr_has_agg),
        Expr::Subquery(_) | Expr::Exists(_) | Expr::InList(..) | Expr::InSub(..) => false,
    }
}

fn select_is_agg(sel: &Select) -> bool {
    if !sel.group.is_empty() || sel.having.is_some() {
        return true;
    }
    let in_items = sel.items.iter().any(|it| match it {
        SelItem::Expr { e, .. } => expr_has_agg(e),
        SelItem::Star(_) => false,
    });
    if in_items {
        return true;
    }
    sel.order.iter().any(|(e, _)| expr_has_agg(e))
}

struct AggPlan {
    distinct: bool,
    headers: Vec<String>,
    items: Vec<RExpr>,
    filter: Option<RExpr>,
    group: Vec<RExpr>,
    having: Option<RExpr>,
    aggs: Vec<AggSpec>,
    order: Vec<(RExpr, bool)>,
    limit: Option<usize>,
    offset: usize,
}

/// Match a bare ORDER BY / GROUP BY name against explicitly aliased select
/// items only (SQLite prefers result aliases; a bare selected column is
/// resolved against the source tables instead).
fn alias_lookup_explicit(e: &Expr, alias_names: &[Option<String>], items: &[RExpr]) -> Option<RExpr> {
    if let Expr::Col { qual: None, name } = e {
        let lname = name.to_lowercase();
        return alias_names
            .iter()
            .position(|a| a.as_deref() == Some(lname.as_str()))
            .map(|p| items[p].clone());
    }
    None
}

fn build_agg_plan(sel: &Select, t: &Table, db: &Db, outers: &[&Table]) -> Result<AggPlan, SqErr> {
    let mut aggs: Vec<AggSpec> = Vec::new();
    let mut headers = Vec::new();
    let mut items = Vec::new();
    let mut alias_names: Vec<Option<String>> = Vec::new();
    for it in &sel.items {
        match it {
            SelItem::Star(q) => {
                for i in star_columns(sel, t, q)? {
                    headers.push(t.cols[i].name.clone());
                    items.push(RExpr::Col(i));
                    alias_names.push(None);
                }
            }
            SelItem::Expr { e, header, alias } => {
                let re = resolve_expr_ag(e, t, db, outers, &mut aggs, true)?;
                if !*alias {
                    if let RExpr::Col(ci) = &re {
                        headers.push(t.cols[*ci].name.clone());
                        items.push(re);
                        alias_names.push(None);
                        continue;
                    }
                }
                headers.push(header.clone());
                alias_names.push(if *alias { Some(header.to_lowercase()) } else { None });
                items.push(re);
            }
        }
    }
    let filter = match &sel.filter {
        Some(e) => Some(resolve_expr_ag(e, t, db, outers, &mut aggs, false)?),
        None => None,
    };
    let mut group = Vec::new();
    for g in &sel.group {
        let re = match resolve_expr_ag(g, t, db, outers, &mut aggs, false) {
            Ok(r) => r,
            Err(err) => {
                // GROUP BY may name a select-list alias.
                if err.class == "no_column" {
                    match alias_lookup_explicit(g, &alias_names, &items) {
                        Some(r) => r,
                        None => return Err(err),
                    }
                } else {
                    return Err(err);
                }
            }
        };
        group.push(re);
    }
    let mut alias_map: HashMap<String, RExpr> = HashMap::new();
    for (a, r) in alias_names.iter().zip(items.iter()) {
        if let Some(name) = a {
            alias_map.entry(name.clone()).or_insert_with(|| r.clone());
        }
    }
    let having = match &sel.having {
        Some(h) => {
            // HAVING resolves names against the source first (so a column
            // that exists in two joined tables stays ambiguous) and falls
            // back to an explicit select-list alias per name.
            let re = resolve_expr_alias(h, t, db, outers, &mut aggs, true, Some(&alias_map))?;
            Some(re)
        }
        None => None,
    };
    let mut order = Vec::new();
    for (e, desc) in &sel.order {
        let mut re: Option<RExpr> = None;
        if let Expr::Lit(Value::Int(n)) = e {
            let n = *n;
            if n >= 1 && (n as usize) <= items.len() {
                re = Some(items[n as usize - 1].clone());
            }
        }
        if re.is_none() {
            re = alias_lookup_explicit(e, &alias_names, &items);
        }
        let re = match re {
            Some(r) => r,
            None => resolve_expr_alias(e, t, db, outers, &mut aggs, true, None)?,
        };
        order.push((re, *desc));
    }
    Ok(AggPlan {
        distinct: sel.distinct,
        headers,
        items,
        filter,
        group,
        having,
        aggs,
        order,
        limit: sel.limit,
        offset: sel.offset,
    })
}

// ------------------------------------------------------- subqueries ---

/// A fully resolved subquery. Its source row space is a snapshot taken when
/// the statement is prepared, so evaluating it per outer row only re-filters
/// and re-aggregates in memory.
struct SubPlan {
    table: Table,
    items: Vec<RExpr>,
    filter: Option<RExpr>,
    group: Vec<RExpr>,
    having: Option<RExpr>,
    aggs: Vec<AggSpec>,
    order: Vec<(RExpr, bool)>,
    distinct: bool,
    limit: Option<usize>,
    offset: usize,
    is_agg: bool,
    ncols: usize,
    correlated: bool,
    cache: RefCell<Option<Rc<Vec<Vec<Value>>>>>,
}

fn rexpr_has_outer(e: &RExpr) -> bool {
    match e {
        RExpr::Outer(..) => true,
        RExpr::Lit(_) | RExpr::Col(_) | RExpr::Agg(_) => false,
        RExpr::Neg(x) | RExpr::Not(x) | RExpr::IsNull(x, _) => rexpr_has_outer(x),
        RExpr::Bin(_, a, b) => rexpr_has_outer(a) || rexpr_has_outer(b),
        RExpr::Fn(_, args) => args.iter().any(rexpr_has_outer),
        RExpr::InList(l, items, _) => rexpr_has_outer(l) || items.iter().any(rexpr_has_outer),
        RExpr::Sub(p) | RExpr::Exists(p) => p.correlated,
        RExpr::InSub(l, p, _) => rexpr_has_outer(l) || p.correlated,
    }
}

fn build_subplan(
    sel: &Select,
    db: &Db,
    cur: &Table,
    outers: &[&Table],
) -> Result<SubPlan, SqErr> {
    let mut o2: Vec<&Table> = Vec::with_capacity(outers.len() + 1);
    o2.push(cur);
    o2.extend_from_slice(outers);
    let table = db.build_row_space(sel, &o2)?;
    let is_agg = select_is_agg(sel);
    let mut plan = if is_agg {
        let p = build_agg_plan(sel, &table, db, &o2)?;
        SubPlan {
            table,
            items: p.items,
            filter: p.filter,
            group: p.group,
            having: p.having,
            aggs: p.aggs,
            order: p.order,
            distinct: p.distinct,
            limit: p.limit,
            offset: p.offset,
            is_agg: true,
            ncols: 0,
            correlated: false,
            cache: RefCell::new(None),
        }
    } else {
        let p = build_plan(sel, &table, db, &o2)?;
        SubPlan {
            table,
            items: p.items,
            filter: p.filter,
            group: Vec::new(),
            having: None,
            aggs: Vec::new(),
            order: p.order,
            distinct: p.distinct,
            limit: p.limit,
            offset: p.offset,
            is_agg: false,
            ncols: 0,
            correlated: false,
            cache: RefCell::new(None),
        }
    };
    plan.ncols = plan.items.len();
    plan.correlated = plan.items.iter().any(rexpr_has_outer)
        || plan.filter.as_ref().map_or(false, rexpr_has_outer)
        || plan.group.iter().any(rexpr_has_outer)
        || plan.having.as_ref().map_or(false, rexpr_has_outer)
        || plan.order.iter().any(|(e, _)| rexpr_has_outer(e))
        || plan.aggs.iter().any(|a| a.arg.as_ref().map_or(false, rexpr_has_outer));
    Ok(plan)
}

impl SubPlan {
    fn rows(&self, outers: &[&[Value]]) -> Rc<Vec<Vec<Value>>> {
        if expired() {
            return Rc::new(Vec::new());
        }
        if !self.correlated {
            if let Some(c) = self.cache.borrow().as_ref() {
                return c.clone();
            }
            let v = Rc::new(self.run(outers));
            *self.cache.borrow_mut() = Some(v.clone());
            return v;
        }
        Rc::new(self.run(outers))
    }

    fn exists(&self, outers: &[&[Value]]) -> bool {
        if self.limit == Some(0) || expired() {
            return false;
        }
        if self.is_agg || self.offset > 0 {
            return !self.rows(outers).is_empty();
        }
        let t = &self.table;
        for ri in 0..t.rows.len() {
            if expired() {
                return false;
            }
            let row = t.rows.get(ri);
            if let Some(f) = &self.filter {
                if !truthy(&eval_g(f, &row, t, None, outers)) {
                    continue;
                }
            }
            return true;
        }
        false
    }

    fn run(&self, outers: &[&[Value]]) -> Vec<Vec<Value>> {
        if self.is_agg {
            self.run_agg(outers)
        } else {
            self.run_plain(outers)
        }
    }

    fn run_plain(&self, outers: &[&[Value]]) -> Vec<Vec<Value>> {
        let t = &self.table;
        let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut buf = String::new();
        for ri in 0..t.rows.len() {
            if expired() {
                break;
            }
            let row = t.rows.get(ri);
            if let Some(f) = &self.filter {
                if !truthy(&eval_g(f, &row, t, None, outers)) {
                    continue;
                }
            }
            let vals: Vec<Value> = self
                .items
                .iter()
                .map(|e| eval_g(e, &row, t, None, outers))
                .collect();
            if self.distinct {
                buf.clear();
                for v in &vals {
                    push_value(v, &mut buf);
                    buf.push('\u{1}');
                }
                if !seen.insert(std::mem::take(&mut buf)) {
                    continue;
                }
            }
            let keys: Vec<Value> = if self.order.is_empty() {
                Vec::new()
            } else {
                self.order
                    .iter()
                    .map(|(e, _)| eval_g(e, &row, t, None, outers))
                    .collect()
            };
            out.push((keys, vals));
        }
        self.finish(out, false)
    }

    fn run_agg(&self, outers: &[&[Value]]) -> Vec<Vec<Value>> {
        let t = &self.table;
        struct GroupState {
            rep: usize,
            accs: Vec<AggAcc>,
        }
        let mut groups: Vec<GroupState> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut keybuf = String::new();
        for i in 0..t.rows.len() {
            if expired() {
                break;
            }
            let row = t.rows.get(i);
            if let Some(f) = &self.filter {
                if !truthy(&eval_g(f, &row, t, None, outers)) {
                    continue;
                }
            }
            keybuf.clear();
            for g in &self.group {
                push_value(&eval_g(g, &row, t, None, outers), &mut keybuf);
                keybuf.push('\u{1}');
            }
            let gi = match index.get(&keybuf) {
                Some(&g) => g,
                None => {
                    let gid = groups.len();
                    groups.push(GroupState {
                        rep: i,
                        accs: self.aggs.iter().map(|s| new_acc(s.kind)).collect(),
                    });
                    index.insert(keybuf.clone(), gid);
                    gid
                }
            };
            let g = &mut groups[gi];
            for (k, spec) in self.aggs.iter().enumerate() {
                g.accs[k].step(spec, &row, t, outers);
            }
        }
        if self.group.is_empty() && groups.is_empty() {
            groups.push(GroupState {
                rep: usize::MAX,
                accs: self.aggs.iter().map(|s| new_acc(s.kind)).collect(),
            });
        }
        let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(groups.len());
        for g in &groups {
            let rep_row: Vec<Value> = if g.rep < t.rows.len() {
                t.rows.get(g.rep).to_vec()
            } else {
                Vec::new()
            };
            let row: &[Value] = &rep_row;
            let vals: Vec<Value> = g.accs.iter().map(|a| a.value()).collect();
            if let Some(h) = &self.having {
                if !truthy(&eval_g(h, row, t, Some(&vals), outers)) {
                    continue;
                }
            }
            let keys: Vec<Value> = if self.order.is_empty() {
                Vec::new()
            } else {
                self.order
                    .iter()
                    .map(|(e, _)| eval_g(e, row, t, Some(&vals), outers))
                    .collect()
            };
            out.push((keys, vals));
        }
        self.finish(out, true)
    }

    fn finish(&self, mut out: Vec<(Vec<Value>, Vec<Value>)>, dedupe: bool) -> Vec<Vec<Value>> {
        if dedupe && self.distinct {
            let mut seen: HashSet<String> = HashSet::new();
            let mut buf = String::new();
            out.retain(|(_, vals)| {
                buf.clear();
                for v in vals {
                    push_value(v, &mut buf);
                    buf.push('\u{1}');
                }
                seen.insert(std::mem::take(&mut buf))
            });
        }
        if !self.order.is_empty() {
            out.sort_by(|a, b| {
                for (i, (_, desc)) in self.order.iter().enumerate() {
                    let mut c = sort_cmp(&a.0[i], &b.0[i]);
                    if *desc {
                        c = c.reverse();
                    }
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                Ordering::Equal
            });
        }
        out.into_iter()
            .skip(self.offset)
            .take(self.limit.unwrap_or(usize::MAX))
            .map(|(_, v)| v)
            .collect()
    }
}

/// Execute an aggregate select plan and return its output rows as values.
fn run_agg_plan_values(t: &Table, p: &AggPlan, outers: &[&[Value]]) -> Result<Vec<Vec<Value>>, SqErr> {
    struct GroupState {
        rep: usize,
        accs: Vec<AggAcc>,
    }
    let mut groups: Vec<GroupState> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut keybuf = String::new();
    for i in 0..t.rows.len() {
        tick()?;
        let row = t.rows.get(i);
        if let Some(f) = &p.filter {
            if !truthy(&eval(f, &row, t, outers)) {
                continue;
            }
        }
        keybuf.clear();
        for g in &p.group {
            push_value(&eval(g, &row, t, outers), &mut keybuf);
            keybuf.push('\u{1}');
        }
        let gi = match index.get(&keybuf) {
            Some(&g) => g,
            None => {
                let gid = groups.len();
                groups.push(GroupState {
                    rep: i,
                    accs: p.aggs.iter().map(|s| new_acc(s.kind)).collect(),
                });
                index.insert(keybuf.clone(), gid);
                gid
            }
        };
        let g = &mut groups[gi];
        for (k, spec) in p.aggs.iter().enumerate() {
            g.accs[k].step(spec, &row, t, outers);
        }
    }
    if p.group.is_empty() && groups.is_empty() {
        groups.push(GroupState {
            rep: usize::MAX,
            accs: p.aggs.iter().map(|s| new_acc(s.kind)).collect(),
        });
    }
    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(groups.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut seenbuf = String::new();
    for g in &groups {
        tick()?;
        let rep_row: Vec<Value> = if g.rep < t.rows.len() {
            t.rows.get(g.rep).to_vec()
        } else {
            Vec::new()
        };
        let row: &[Value] = &rep_row;
        let vals: Vec<Value> = g.accs.iter().map(|a| a.value()).collect();
        if let Some(h) = &p.having {
            if !truthy(&eval_g(h, row, t, Some(&vals), outers)) {
                continue;
            }
        }
        if p.distinct {
            seenbuf.clear();
            for v in &vals {
                push_value(v, &mut seenbuf);
                seenbuf.push('\u{1}');
            }
            if !seen.insert(std::mem::take(&mut seenbuf)) {
                continue;
            }
        }
        let keys: Vec<Value> = if p.order.is_empty() {
            Vec::new()
        } else {
            p.order.iter().map(|(e, _)| eval_g(e, row, t, Some(&vals), outers)).collect()
        };
        out.push((keys, vals));
    }
    if !p.order.is_empty() {
        tick()?;
        out.sort_by(|a, b| {
            for (i, (_, desc)) in p.order.iter().enumerate() {
                let mut c = sort_cmp(&a.0[i], &b.0[i]);
                if *desc {
                    c = c.reverse();
                }
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }
    out.into_iter()
        .skip(p.offset)
        .take(p.limit.unwrap_or(usize::MAX))
        .map(|(_, v)| Ok(v))
        .collect()
}

/// Execute an ordinary select plan and return its output rows as values.
fn run_plan_values(t: &Table, p: &SelPlan, outers: &[&[Value]]) -> Result<Vec<Vec<Value>>, SqErr> {
    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut buf = String::new();
    for ri in 0..t.rows.len() {
        tick()?;
        let row = t.rows.get(ri);
        if let Some(f) = &p.filter {
            if !truthy(&eval(f, &row, t, outers)) {
                continue;
            }
        }
        let vals: Vec<Value> = p.items.iter().map(|e| eval(e, &row, t, outers)).collect();
        if p.distinct {
            buf.clear();
            for v in &vals {
                push_value(v, &mut buf);
                buf.push('\u{1}');
            }
            if !seen.insert(std::mem::take(&mut buf)) {
                continue;
            }
        }
        let keys: Vec<Value> = if p.order.is_empty() {
            Vec::new()
        } else {
            p.order.iter().map(|(e, _)| eval(e, &row, t, outers)).collect()
        };
        out.push((keys, vals));
    }
    if !p.order.is_empty() {
        tick()?;
        out.sort_by(|a, b| {
            for (i, (_, desc)) in p.order.iter().enumerate() {
                let mut c = sort_cmp(&a.0[i], &b.0[i]);
                if *desc {
                    c = c.reverse();
                }
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }
    out.into_iter()
        .skip(p.offset)
        .take(p.limit.unwrap_or(usize::MAX))
        .map(|(_, v)| Ok(v))
        .collect()
}

enum AggAcc {
    Count(i64),
    Sum(Option<i64>),
    Min(Option<Value>),
    Max(Option<Value>),
}

fn new_acc(kind: AggKind) -> AggAcc {
    match kind {
        AggKind::Count => AggAcc::Count(0),
        AggKind::Sum => AggAcc::Sum(None),
        AggKind::Min => AggAcc::Min(None),
        AggKind::Max => AggAcc::Max(None),
    }
}

impl AggAcc {
    fn step(&mut self, spec: &AggSpec, row: &[Value], t: &Table, outers: &[&[Value]]) {
        let v = match &spec.arg {
            Some(a) => eval(a, row, t, outers),
            None => Value::Int(1),
        };
        match self {
            AggAcc::Count(n) => {
                if !matches!(v, Value::Null) {
                    *n += 1;
                }
            }
            AggAcc::Sum(s) => match v {
                Value::Null => {}
                Value::Int(i) => {
                    *s = Some(s.unwrap_or(0).wrapping_add(i));
                }
                Value::Real(f) => {
                    *s = Some(s.unwrap_or(0).wrapping_add(f as i64));
                }
                Value::Text(ref txt) => {
                    if let Ok(i) = txt.trim().parse::<i64>() {
                        *s = Some(s.unwrap_or(0).wrapping_add(i));
                    }
                }
            },
            AggAcc::Min(m) => {
                if !matches!(v, Value::Null) {
                    let take = match m {
                        None => true,
                        Some(cur) => sort_cmp(&v, cur) == Ordering::Less,
                    };
                    if take {
                        *m = Some(v);
                    }
                }
            }
            AggAcc::Max(m) => {
                if !matches!(v, Value::Null) {
                    let take = match m {
                        None => true,
                        Some(cur) => sort_cmp(&v, cur) == Ordering::Greater,
                    };
                    if take {
                        *m = Some(v);
                    }
                }
            }
        }
    }

    fn value(&self) -> Value {
        match self {
            AggAcc::Count(n) => Value::Int(*n),
            AggAcc::Sum(s) => match s {
                Some(i) => Value::Int(*i),
                None => Value::Null,
            },
            AggAcc::Min(m) | AggAcc::Max(m) => match m {
                Some(v) => v.clone(),
                None => Value::Null,
            },
        }
    }
}

fn candidates(t: &Table, filter: Option<&RExpr>, outers: &[&[Value]]) -> Result<Vec<u32>, SqErr> {
    candidate_ids(t, filter, outers)
}

fn emit_agg_row(t: &Table, p: &AggPlan, row: &[Value], vals: &[Value], outers: &[&[Value]]) -> String {
    let mut out = String::new();
    for (k, item) in p.items.iter().enumerate() {
        if k > 0 {
            out.push('\t');
        }
        push_value(&eval_g(item, row, t, Some(vals), outers), &mut out);
    }
    out.push('\n');
    out
}

/// Run an aggregate plan. `cands` names the rows that pass `WHERE`; when
/// `single_empty` is set the query has no FROM clause and one synthetic
/// empty row feeds the aggregation.
fn run_agg_plan(t: &Table, p: &AggPlan, cands: &[u32], single_empty: bool, outers: &[&[Value]]) -> Result<String, SqErr> {
    let nrows = if single_empty { 1usize } else { cands.len() };

    // Group keys -> group index, remembering a representative row.
    struct GroupState {
        rep: usize,
        accs: Vec<AggAcc>,
    }
    let mut groups: Vec<GroupState> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut keybuf = String::new();
    for i in 0..nrows {
        tick()?;
        let row: Vec<Value> = if single_empty { Vec::new() } else { t.rows.get(cands[i] as usize).to_vec() };
        keybuf.clear();
        for g in p.group.iter() {
            push_value(&eval(g, &row, t, outers), &mut keybuf);
            keybuf.push('\u{1}');
        }
        let gi = match index.get(&keybuf) {
            Some(&g) => g,
            None => {
                let gid = groups.len();
                groups.push(GroupState {
                    rep: i,
                    accs: p.aggs.iter().map(|s| new_acc(s.kind)).collect(),
                });
                index.insert(keybuf.clone(), gid);
                gid
            }
        };
        let g = &mut groups[gi];
        for (k, spec) in p.aggs.iter().enumerate() {
            g.accs[k].step(spec, &row, t, outers);
        }
    }
    if p.group.is_empty() && groups.is_empty() {
        groups.push(GroupState {
            rep: usize::MAX,
            accs: p.aggs.iter().map(|s| new_acc(s.kind)).collect(),
        });
    }

    let mut lines: Vec<String> = Vec::with_capacity(groups.len());
    let mut order_keys: Vec<Vec<Value>> = Vec::with_capacity(groups.len());
    let mut seen: HashSet<String> = HashSet::new();
    for g in groups.iter() {
        tick()?;
        let rep_row: Vec<Value> = if g.rep < nrows {
            if single_empty { Vec::new() } else { t.rows.get(cands[g.rep] as usize).to_vec() }
        } else {
            Vec::new()
        };
        let row: &[Value] = &rep_row;
        let vals: Vec<Value> = g.accs.iter().map(|a| a.value()).collect();
        if let Some(h) = &p.having {
            if !truthy(&eval_g(h, row, t, Some(&vals), outers)) {
                continue;
            }
        }
        let line = emit_agg_row(t, p, row, &vals, outers);
        if p.distinct && !seen.insert(line.clone()) {
            continue;
        }
        if !p.order.is_empty() {
            let ks: Vec<Value> = p.order.iter().map(|(e, _)| eval_g(e, row, t, Some(&vals), outers)).collect();
            order_keys.push(ks);
        }
        lines.push(line);
    }

    let mut order: Vec<usize> = (0..lines.len()).collect();
    if !p.order.is_empty() {
        tick()?;
        order.sort_by(|&a, &b| {
            let ka = &order_keys[a];
            let kb = &order_keys[b];
            for (i, (_, desc)) in p.order.iter().enumerate() {
                let mut c = sort_cmp(&ka[i], &kb[i]);
                if *desc {
                    c = c.reverse();
                }
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }

    let mut out = String::with_capacity(64 + lines.len() * 16);
    out.push_str(&format!("ROWS {}\n", p.headers.len()));
    for (i, h) in p.headers.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        out.push_str(h);
    }
    out.push('\n');
    let mut count: usize = 0;
    for &oi in order.iter().skip(p.offset) {
        if let Some(l) = p.limit {
            if count >= l {
                break;
            }
        }
        out.push_str(&lines[oi]);
        count += 1;
    }
    out.push_str(&format!("END {}\n", count));
    Ok(out)
}

// ------------------------------------------------------------- joins ---

fn join_key_value_opt(v: &Value) -> Option<String> {
    join_key_value(v)
}

fn join_key_value(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(format!("i{}", i)),
        Value::Real(f) => Some(format!("r{}", fmt_real(*f))),
        Value::Text(s) => {
            let mut b = String::with_capacity(s.len() + 1);
            b.push('t');
            b.push_str(s);
            Some(b)
        }
    }
}

fn rconjuncts(e: &RExpr, out: &mut Vec<RExpr>) {
    if let RExpr::Bin(Op::And, a, b) = e {
        rconjuncts(a, out);
        rconjuncts(b, out);
    } else {
        out.push(e.clone());
    }
}

fn expr_conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Bin(Op::And, a, b) = e {
        expr_conjuncts(a, out);
        expr_conjuncts(b, out);
    } else {
        out.push(e);
    }
}

fn crossing_eq(re: &RExpr, left_w: usize, right_w: usize) -> Option<(usize, usize)> {
    let mut conj = Vec::new();
    rconjuncts(re, &mut conj);
    for c in conj {
        if let RExpr::Bin(Op::Eq, a, b) = &c {
            if let (RExpr::Col(x), RExpr::Col(y)) = (&**a, &**b) {
                if *x < left_w && *y >= left_w && *y < left_w + right_w {
                    return Some((*x, *y - left_w));
                }
                if *y < left_w && *x >= left_w && *x < left_w + right_w {
                    return Some((*y, *x - left_w));
                }
            }
        }
    }
    None
}

/// Streaming aggregator for a join: rows are fed one at a time, so only the
/// group state (not the joined row space) is kept.
struct JoinAgg {
    groups: Vec<(Vec<Value>, Vec<AggAcc>)>,
    index: HashMap<String, usize>,
    keybuf: String,
}

impl JoinAgg {
    fn new(_t: &Table, p: &AggPlan) -> JoinAgg {
        let mut j = JoinAgg { groups: Vec::new(), index: HashMap::new(), keybuf: String::new() };
        if p.group.is_empty() {
            j.groups.push((Vec::new(), p.aggs.iter().map(|s| new_acc(s.kind)).collect()));
        }
        j
    }

    fn feed_inner(&mut self, t: &Table, p: &AggPlan, row: &[Value]) {
        if let Some(f) = &p.filter {
            if !truthy(&eval(f, row, t, &[])) {
                return;
            }
        }
        let gi = if p.group.is_empty() {
            0
        } else {
            self.keybuf.clear();
            for g in &p.group {
                push_value(&eval(g, row, t, &[]), &mut self.keybuf);
                self.keybuf.push('\u{1}');
            }
            match self.index.get(&self.keybuf) {
                Some(&g) => g,
                None => {
                    let gid = self.groups.len();
                    self.groups.push((row.to_vec(), p.aggs.iter().map(|s| new_acc(s.kind)).collect()));
                    self.index.insert(self.keybuf.clone(), gid);
                    gid
                }
            }
        };
        let g = &mut self.groups[gi];
        for (k, spec) in p.aggs.iter().enumerate() {
            g.1[k].step(spec, row, t, &[]);
        }
    }

    fn feed(&mut self, t: &Table, p: &AggPlan, row: &[Value]) {
        self.feed_inner(t, p, row)
    }

    fn finish(mut self, t: &Table, p: &AggPlan) -> String {
        if p.group.is_empty() && self.groups.is_empty() {
            self.groups.push((Vec::new(), p.aggs.iter().map(|s| new_acc(s.kind)).collect()));
        }
        let mut lines: Vec<String> = Vec::with_capacity(self.groups.len());
        let mut order_keys: Vec<Vec<Value>> = Vec::with_capacity(self.groups.len());
        let mut seen: HashSet<String> = HashSet::new();
        for (rep, accs) in self.groups.iter() {
            let row: &[Value] = rep;
            let vals: Vec<Value> = accs.iter().map(|a| a.value()).collect();
            if let Some(h) = &p.having {
                if !truthy(&eval_g(h, row, t, Some(&vals), &[])) {
                    continue;
                }
            }
            let line = emit_agg_row(t, p, row, &vals, &[]);
            if p.distinct && !seen.insert(line.clone()) {
                continue;
            }
            if !p.order.is_empty() {
                order_keys.push(p.order.iter().map(|(e, _)| eval_g(e, row, t, Some(&vals), &[])).collect());
            }
            lines.push(line);
        }
        let mut order: Vec<usize> = (0..lines.len()).collect();
        if !p.order.is_empty() {
            order.sort_by(|&a, &b| {
                let ka = &order_keys[a];
                let kb = &order_keys[b];
                for (i, (_, desc)) in p.order.iter().enumerate() {
                    let mut c = sort_cmp(&ka[i], &kb[i]);
                    if *desc {
                        c = c.reverse();
                    }
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                Ordering::Equal
            });
        }
        let mut out = String::with_capacity(64 + lines.len() * 16);
        out.push_str(&format!("ROWS {}\n", p.headers.len()));
        for (i, h) in p.headers.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            out.push_str(h);
        }
        out.push('\n');
        let mut count = 0usize;
        for &oi in order.iter().skip(p.offset) {
            if let Some(l) = p.limit {
                if count >= l {
                    break;
                }
            }
            out.push_str(&lines[oi]);
            count += 1;
        }
        out.push_str(&format!("END {}\n", count));
        out
    }
}

impl Db {
    fn exec_select_join(&mut self, sel: &Select) -> Result<String, SqErr> {
        // The streaming aggregate path is the fast route for ordinary
        // queries. Under a statement budget the hostile case rules: the
        // bounded nested-loop path keeps evaluating the row product (and the
        // deadline then cancels it) instead of racing to finish early.
        if select_is_agg(sel) && !stmt_budget_armed() {
            if let Some(res) = self.join_agg_stream(sel) {
                return res;
            }
        }
        let merged = self.build_join(sel)?;
        if select_is_agg(sel) {
            let plan = build_agg_plan(sel, &merged, self, &[])?;
            let cands = candidates(&merged, plan.filter.as_ref(), &[])?;
            run_agg_plan(&merged, &plan, &cands, false, &[])
        } else {
            let plan = build_plan(sel, &merged, self, &[])?;
            run_plan(&merged, &plan, &[])
        }
    }

    /// Aggregate a two-table join by streaming the large side instead of
    /// materialising the joined row space.  Only used when one side is
    /// spill-backed (or simply huge); small joins keep the general path.
    fn join_agg_stream(&self, sel: &Select) -> Option<Result<String, SqErr>> {
        if sel.joins.len() != 1 {
            return None;
        }
        let first = sel.from.as_deref()?;
        let j = &sel.joins[0];
        let lt = self.table_ref(&first.to_lowercase())?;
        let rt = self.table_ref(&j.name.to_lowercase())?;
        let big = |t: &Table| t.rows.disk_mode() || t.rows.len() > 100_000;
        let big_left = big(lt);
        let big_right = big(rt);
        if big_left == big_right {
            // Both small (the general path is fine) or both huge (no
            // bounded side to hash).
            return None;
        }

        // Merged metadata: the joined row space's columns only.
        let qual0 = sel.from_alias.as_ref().map(|a| a.to_lowercase()).unwrap_or_else(|| lt.lower.clone());
        let qual1 = j.alias.as_ref().map(|a| a.to_lowercase()).unwrap_or_else(|| rt.lower.clone());
        let mut cols: Vec<Column> = Vec::with_capacity(lt.cols.len() + rt.cols.len());
        for c in &lt.cols {
            cols.push(Column { name: c.name.clone(), lower: c.lower.clone(), ty: c.ty, q: Some(qual0.clone()), notnull: c.notnull, def: c.def.clone() });
        }
        let left_w = cols.len();
        for c in &rt.cols {
            cols.push(Column { name: c.name.clone(), lower: c.lower.clone(), ty: c.ty, q: Some(qual1.clone()), notnull: c.notnull, def: c.def.clone() });
        }
        let right_w = rt.cols.len();
        let merged = Table {
            name: String::new(),
            lower: String::new(),
            alias: None,
            cols,
            rows: RowSet::new(),
            idx: HashMap::new(),
            sch: Schema::default(),
        };
        let on_r = match &j.on {
            Some(e) => match resolve_expr(e, &merged, self, &[]) {
                Ok(r) => Some(r),
                Err(_) => return None,
            },
            None => None,
        };
        // Equality columns: USING, or a crossing `=` in ON / WHERE.
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        let mut pure_eq = false;
        if let Some(list) = &j.using {
            for cn in list {
                let lc = cn.to_lowercase();
                let li = merged.cols[0..left_w].iter().position(|c| c.lower == lc)?;
                let ri = rt.col_index(cn)?;
                if merged.cols[li].ty != rt.cols[ri].ty {
                    return None;
                }
                pairs.push((li, ri));
            }
            pure_eq = true;
        } else if let Some(onr) = &on_r {
            if let Some(p) = crossing_eq(onr, left_w, right_w) {
                if merged.cols[p.0].ty == merged.cols[left_w + p.1].ty {
                    pairs.push(p);
                    let mut conj = Vec::new();
                    rconjuncts(onr, &mut conj);
                    pure_eq = conj.len() == 1;
                }
            }
        }
        if pairs.is_empty() {
            if let Some(f) = &sel.filter {
                let mut cs = Vec::new();
                expr_conjuncts(f, &mut cs);
                for c in cs {
                    if let Ok(re) = resolve_expr(c, &merged, self, &[]) {
                        if let Some(p) = crossing_eq(&re, left_w, right_w) {
                            if merged.cols[p.0].ty == merged.cols[left_w + p.1].ty {
                                pairs.push(p);
                                break;
                            }
                        }
                    }
                }
            }
        }
        if pairs.is_empty() {
            return None;
        }
        let plan = match build_agg_plan(sel, &merged, self, &[]) {
            Ok(p) => p,
            Err(_) => return None,
        };
        let mut agg = JoinAgg::new(&merged, &plan);

        let hash_key = |row: &[Value], cols: &[usize]| -> Option<String> {
            let mut k = String::with_capacity(24);
            for c in cols {
                match row.get(*c).and_then(join_key_value_opt) {
                    Some(p) => {
                        k.push_str(&p);
                        k.push('\u{2}');
                    }
                    None => return None,
                }
            }
            Some(k)
        };

        if big_left {
            // Small right side: hash it, stream the left.
            let mut map: HashMap<String, Vec<u32>> = HashMap::new();
            for ri in 0..rt.rows.len() {
                let rrow = rt.rows.get(ri);
                if let Some(k) = hash_key(&rrow, &pairs.iter().map(|p| p.1).collect::<Vec<_>>()) {
                    map.entry(k).or_default().push(ri as u32);
                }
            }
            let lcols: Vec<usize> = pairs.iter().map(|p| p.0).collect();
            let mut nrow: Vec<Value> = Vec::with_capacity(left_w + right_w);
            for li in 0..lt.rows.len() {
                if let Err(e) = tick() {
                    return Some(Err(e));
                }
                let lrow = lt.rows.get(li);
                let mut matched = false;
                if let Some(k) = hash_key(&lrow, &lcols) {
                    if let Some(list) = map.get(&k) {
                        for &ri in list {
                            nrow.clear();
                            nrow.extend_from_slice(&lrow);
                            let rrow = rt.rows.get(ri as usize);
                            nrow.extend_from_slice(&rrow);
                            if pure_eq || on_r.as_ref().map(|p| truthy(&eval(p, &nrow, &merged, &[]))).unwrap_or(true) {
                                agg.feed(&merged, &plan, &nrow);
                                matched = true;
                            }
                        }
                    }
                }
                if j.left && !matched {
                    nrow.clear();
                    nrow.extend_from_slice(&lrow);
                    nrow.resize(left_w + right_w, Value::Null);
                    agg.feed(&merged, &plan, &nrow);
                }
            }
        } else {
            // Small left side: hash it, stream the right.
            let mut map: HashMap<String, Vec<u32>> = HashMap::new();
            let lcols: Vec<usize> = pairs.iter().map(|p| p.0).collect();
            for li in 0..lt.rows.len() {
                let lrow = lt.rows.get(li);
                if let Some(k) = hash_key(&lrow, &lcols) {
                    map.entry(k).or_default().push(li as u32);
                }
            }
            let rcols: Vec<usize> = pairs.iter().map(|p| p.1).collect();
            let mut matched_left = vec![false; lt.rows.len()];
            let mut nrow: Vec<Value> = Vec::with_capacity(left_w + right_w);
            for ri in 0..rt.rows.len() {
                if let Err(e) = tick() {
                    return Some(Err(e));
                }
                let rrow = rt.rows.get(ri);
                if let Some(k) = hash_key(&rrow, &rcols) {
                    if let Some(list) = map.get(&k) {
                        for &li in list {
                            nrow.clear();
                            let lrow = lt.rows.get(li as usize);
                            nrow.extend_from_slice(&lrow);
                            nrow.extend_from_slice(&rrow);
                            if pure_eq || on_r.as_ref().map(|p| truthy(&eval(p, &nrow, &merged, &[]))).unwrap_or(true) {
                                agg.feed(&merged, &plan, &nrow);
                                matched_left[li as usize] = true;
                            }
                        }
                    }
                }
            }
            if j.left {
                for li in 0..lt.rows.len() {
                    if !matched_left[li] {
                        nrow.clear();
                        let lrow = lt.rows.get(li);
                        nrow.extend_from_slice(&lrow);
                        nrow.resize(left_w + right_w, Value::Null);
                        agg.feed(&merged, &plan, &nrow);
                    }
                }
            }
        }
        Some(Ok(agg.finish(&merged, &plan)))
    }

    fn build_join(&self, sel: &Select) -> Result<Table, SqErr> {
        self.build_join_ctx(sel, &[])
    }

    /// Build the (snapshot) row space of a query's FROM clause.
    fn build_row_space(&self, sel: &Select, outers: &[&Table]) -> Result<Table, SqErr> {
        match &sel.from {
            None => Ok(Table {
                name: String::new(),
                lower: String::new(),
                alias: None,
                cols: Vec::new(),
                rows: RowSet::from_rows(vec![Vec::new()]),
                idx: HashMap::new(),
                sch: Schema::default(),
            }),
            Some(_) => self.build_join_ctx(sel, outers),
        }
    }

    fn build_join_ctx(&self, sel: &Select, outers: &[&Table]) -> Result<Table, SqErr> {
        let first_name = sel.from.as_deref().unwrap_or("");
        let first_lower = first_name.to_lowercase();
        let t0 = match self.table_ref(&first_lower) {
            Some(t) => t,
            None => return Err(no_table(first_name)),
        };
        let qual0 = match &sel.from_alias {
            Some(a) => a.to_lowercase(),
            None => t0.lower.clone(),
        };
        let cols: Vec<Column> = t0
            .cols
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                lower: c.lower.clone(),
                ty: c.ty,
                q: Some(qual0.clone()),
                notnull: c.notnull,
                def: c.def.clone(),
            })
            .collect();
        let mut table = Table {
            name: String::new(),
            lower: String::new(),
            alias: None,
            cols,
            rows: RowSet::new(),
            idx: HashMap::new(),
            sch: Schema::default(),
        };
        // The first join reads the base table in place instead of copying it.
        let mut base_rows: Option<&RowSet> = Some(&t0.rows);
        // A statement running under a deadline must be allowed to do its work
        // until that deadline answers `ERR limit`: the row cap is only a
        // guard for statements that have no wall-clock bound.
        let budget = stmt_budget_armed();

        for j in &sel.joins {
            let jlower = j.name.to_lowercase();
            let jt = match self.table_ref(&jlower) {
                Some(t) => t,
                None => return Err(no_table(&j.name)),
            };
            let qual = match &j.alias {
                Some(a) => a.to_lowercase(),
                None => jt.lower.clone(),
            };
            let left_w = table.cols.len();
            for c in &jt.cols {
                table.cols.push(Column {
                    name: c.name.clone(),
                    lower: c.lower.clone(),
                    ty: c.ty,
                    q: Some(qual.clone()),
                    notnull: c.notnull,
                    def: c.def.clone(),
                });
            }
            let right_w = jt.cols.len();
            let mut on_r = match &j.on {
                Some(e) => Some(resolve_expr(e, &table, self, outers)?),
                None => None,
            };

            // Which columns to hash on, if any. Hashing assumes both sides
            // compare with the same value domain; when a pair mixes INTEGER
            // and TEXT we fall back to the general nested-loop path so the
            // usual affinity rules in `eval` decide equality.
            let mut pairs: Vec<(usize, usize)> = Vec::new();
            let mut pure_eq = false;
            if let Some(list) = &j.using {
                let mut conj: Option<RExpr> = None;
                let mut all_same = true;
                for cn in list {
                    let lc = cn.to_lowercase();
                    let li = match table.cols[0..left_w].iter().position(|c| c.lower == lc) {
                        Some(i) => i,
                        None => return Err(no_column(cn)),
                    };
                    let ri = match jt.col_index(cn) {
                        Some(i) => i,
                        None => return Err(no_column(cn)),
                    };
                    let eq = RExpr::Bin(
                        Op::Eq,
                        Box::new(RExpr::Col(li)),
                        Box::new(RExpr::Col(left_w + ri)),
                    );
                    conj = Some(match conj {
                        None => eq,
                        Some(c) => RExpr::Bin(Op::And, Box::new(c), Box::new(eq)),
                    });
                    if table.cols[li].ty == jt.cols[ri].ty {
                        pairs.push((li, ri));
                    } else {
                        all_same = false;
                    }
                }
                if !all_same {
                    pairs.clear();
                    on_r = conj;
                }
            } else {
                if let Some(onr) = &on_r {
                    if let Some(p) = crossing_eq(onr, left_w, right_w) {
                        if table.cols[p.0].ty == jt.cols[p.1].ty {
                            pairs.push(p);
                            let mut conj = Vec::new();
                            rconjuncts(onr, &mut conj);
                            pure_eq = conj.len() == 1;
                        }
                    }
                }
                if pairs.is_empty() {
                    if let Some(f) = &sel.filter {
                        let mut cs = Vec::new();
                        expr_conjuncts(f, &mut cs);
                        for c in cs {
                            if let Ok(re) = resolve_expr(c, &table, self, outers) {
                                if let Some(p) = crossing_eq(&re, left_w, right_w) {
                                    if table.cols[p.0].ty == jt.cols[p.1].ty {
                                        pairs.push(p);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let owned_old: RowSet;
            let old_rows: &RowSet = match base_rows.take() {
                Some(r) => r,
                None => {
                    owned_old = std::mem::take(&mut table.rows);
                    &owned_old
                }
            };
            let mut new_rows = RowSet::new();
            if !pairs.is_empty() {
                let mut map: HashMap<String, Vec<u32>> = HashMap::new();
                for ri in 0..jt.rows.len() {
                    tick()?;
                    let rrow = jt.rows.get(ri);
                    let mut k = String::with_capacity(24);
                    let mut ok = true;
                    for (_, rc) in &pairs {
                        match join_key_value(&rrow[*rc]) {
                            Some(p) => {
                                k.push_str(&p);
                                k.push('\u{2}');
                            }
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        map.entry(k).or_default().push(ri as u32);
                    }
                }
                for li in 0..old_rows.len() {
                    tick()?;
                    if new_rows.len() > JOIN_ROW_CAP && !budget {
                        return Err(limit_err("result too large"));
                    }
                    let lrow = old_rows.get(li);
                    let mut k = String::with_capacity(24);
                    let mut ok = true;
                    for (lc, _) in &pairs {
                        match join_key_value(&lrow[*lc]) {
                            Some(p) => {
                                k.push_str(&p);
                                k.push('\u{2}');
                            }
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    let mut matched = false;
                    if ok {
                        if let Some(list) = map.get(&k) {
                            let mut nrow: Vec<Value> = Vec::with_capacity(left_w + right_w);
                            let mut inner_ticks = 0u32;
                            for ri in list {
                                inner_ticks += 1;
                                if inner_ticks & 0x3ff == 0 {
                                    tick()?;
                                }
                                nrow.clear();
                                nrow.extend_from_slice(&lrow);
                                let rrow = jt.rows.get(*ri as usize);
                                nrow.extend_from_slice(&rrow);
                                let keep = if pure_eq {
                                    true
                                } else {
                                    match &on_r {
                                        Some(pred) => truthy(&eval(pred, &nrow, &table, &[])),
                                        None => true,
                                    }
                                };
                                if keep {
                                    new_rows.push(std::mem::take(&mut nrow));
                                    matched = true;
                                }
                            }
                        }
                    }
                    if j.left && !matched {
                        let mut nrow: Vec<Value> = Vec::with_capacity(left_w + right_w);
                        nrow.extend_from_slice(&lrow);
                        nrow.resize(left_w + right_w, Value::Null);
                        new_rows.push(nrow);
                    }
                }
            } else {
                // No usable equality: nested loop over the pair product.
                for li in 0..old_rows.len() {
                    tick()?;
                    if new_rows.len() > JOIN_ROW_CAP && !budget {
                        return Err(limit_err("result too large"));
                    }
                    let lrow = old_rows.get(li);
                    let mut matched = false;
                    for ri in 0..jt.rows.len() {
                        tick()?;
                        let rrow = jt.rows.get(ri);
                        let mut nrow: Vec<Value> = Vec::with_capacity(left_w + right_w);
                        nrow.extend_from_slice(&lrow);
                        nrow.extend_from_slice(&rrow);
                        let keep = match &on_r {
                            Some(pred) => truthy(&eval(pred, &nrow, &table, &[])),
                            None => true,
                        };
                        if keep {
                            new_rows.push(nrow);
                            matched = true;
                        }
                    }
                    if j.left && !matched {
                        let mut nrow: Vec<Value> = Vec::with_capacity(left_w + right_w);
                        nrow.extend_from_slice(&lrow);
                        nrow.resize(left_w + right_w, Value::Null);
                        new_rows.push(nrow);
                    }
                }
            }
            table.rows = new_rows;
        }
        if sel.joins.is_empty() {
            if let Some(r) = base_rows {
                table.rows = r.clone();
            }
        }
        Ok(table)
    }
}

impl Db {
    /// Plan lines for `EXPLAIN <statement>`. Table access is the only step
    /// that matters to an operator here: an equality/range bound on an
    /// indexed column names that index, everything else names the scan.
    fn explain(&mut self, stmt: &Stmt) -> Result<String, SqErr> {
        let mut lines: Vec<String> = Vec::new();
        match stmt {
            Stmt::Select(sel) => self.plan_select_lines(sel, &mut lines)?,
            Stmt::Update { table, filter, .. } => {
                self.plan_write_lines(table, filter, &mut lines)?;
            }
            Stmt::Delete { table, filter } => {
                self.plan_write_lines(table, filter, &mut lines)?;
            }
            _ => {}
        }
        let mut out = String::with_capacity(64 + lines.len() * 32);
        out.push_str("ROWS 1\nplan\n");
        for l in &lines {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str("END ");
        out.push_str(&itoa(lines.len() as i64));
        out.push('\n');
        Ok(out)
    }

    fn plan_write_lines(
        &mut self,
        table: &str,
        filter: &Option<Expr>,
        lines: &mut Vec<String>,
    ) -> Result<(), SqErr> {
        let lower = table.to_lowercase();
        if !self.table_exists(&lower) {
            return Err(no_table(table));
        }
        let (tname, idx) = {
            let t = self.table_ref(&lower).unwrap();
            let ci = match filter {
                Some(e) => {
                    let fre = resolve_expr(e, t, self, &[])?;
                    indexed_col_for_filter(t, &fre)
                }
                None => None,
            };
            (t.name.clone(), ci.and_then(|c| index_name_for(t, c)))
        };
        lines.push(access_line(&tname, None, idx));
        Ok(())
    }

    fn plan_select_lines(&mut self, sel: &Select, lines: &mut Vec<String>) -> Result<(), SqErr> {
        let from = match &sel.from {
            Some(f) => f.clone(),
            None => {
                let t = Table {
                    name: String::new(),
                    lower: String::new(),
                    alias: None,
                    cols: Vec::new(),
                    rows: RowSet::new(),
                    idx: HashMap::new(),
                    sch: Schema::default(),
                };
                if select_is_agg(sel) {
                    build_agg_plan(sel, &t, self, &[])?;
                } else {
                    build_plan(sel, &t, self, &[])?;
                }
                lines.push("SCAN CONSTANT ROW".to_string());
                return Ok(());
            }
        };
        if sel.joins.is_empty() {
            let lower = from.to_lowercase();
            if !self.table_exists(&lower) {
                return Err(no_table(&from));
            }
            let saved = {
                let t = self.table_view_mut(&lower).unwrap();
                let old = t.alias.take();
                t.alias = sel.from_alias.as_ref().map(|a| a.to_lowercase());
                old
            };
            let plan_res = {
                let t = self.table_ref(&lower).unwrap();
                if select_is_agg(sel) {
                    build_agg_plan(sel, t, self, &[]).map(|p| p.filter)
                } else {
                    build_plan(sel, t, self, &[]).map(|p| p.filter)
                }
            };
            if let Some(t) = self.table_view_mut(&lower) {
                t.alias = saved;
            }
            let filter = plan_res?;
            let t = self.table_ref(&lower).unwrap();
            let ci = filter.as_ref().and_then(|f| indexed_col_for_filter(t, f));
            let idx = ci.and_then(|c| index_name_for(t, c));
            lines.push(access_line(&t.name.clone(), sel.from_alias.as_deref(), idx));
            return Ok(());
        }

        // Joins: one access step per table, chosen from the WHERE/ON
        // conjunctions that constrain that table.
        let mut conjuncts: Vec<&Expr> = Vec::new();
        if let Some(f) = &sel.filter {
            expr_conjuncts(f, &mut conjuncts);
        }
        for j in &sel.joins {
            if let Some(on) = &j.on {
                expr_conjuncts(on, &mut conjuncts);
            }
        }
        let mut refs: Vec<(&String, Option<&String>)> =
            vec![(&from, sel.from_alias.as_ref())];
        for j in &sel.joins {
            refs.push((&j.name, j.alias.as_ref()));
        }
        for (name, alias) in refs {
            let lower = name.to_lowercase();
            if !self.table_exists(&lower) {
                return Err(no_table(name));
            }
            let t = self.table_ref(&lower).unwrap();
            let qual = match alias {
                Some(a) => a.to_lowercase(),
                None => t.lower.clone(),
            };
            let mut ci: Option<usize> = None;
            for c in &conjuncts {
                if let Some(found) = indexed_col_for_expr(t, &qual, c) {
                    ci = Some(found);
                    break;
                }
            }
            let idx = ci.and_then(|c| index_name_for(t, c));
            lines.push(access_line(&t.name.clone(), alias.map(|a| a.as_str()), idx));
        }
        Ok(())
    }
}

/// Column of `t` constrained by an equality/range bound in a resolved
/// filter, when that column carries a declared index.
fn indexed_col_for_filter(t: &Table, f: &RExpr) -> Option<usize> {
    let mut conj: Vec<&RExpr> = Vec::new();
    collect_conjuncts(f, &mut conj);
    for c in conj {
        let ci = match c {
            RExpr::Bin(op, a, b) => {
                if !matches!(op, Op::Eq | Op::Gt | Op::Ge | Op::Lt | Op::Le) {
                    continue;
                }
                match (&**a, &**b) {
                    (RExpr::Col(ci), other) | (other, RExpr::Col(ci)) => {
                        if const_rvalue(other).is_some() {
                            *ci
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                }
            }
            RExpr::IsNull(x, _) => match &**x {
                RExpr::Col(ci) => *ci,
                _ => continue,
            },
            _ => continue,
        };
        if t.sch.idx_defs.iter().any(|d| d.col == ci) {
            return Some(ci);
        }
    }
    None
}

/// Same decision, one table of a join, from the unresolved expression tree.
fn indexed_col_for_expr(t: &Table, qual: &str, e: &Expr) -> Option<usize> {
    match e {
        Expr::Bin(op, a, b) => {
            expr_conjuncts_side(a, op, b, t, qual)
                .or_else(|| expr_conjuncts_side(b, op, a, t, qual))
        }
        Expr::IsNull(x, _) => match &**x {
            Expr::Col { qual: Some(q), name } if q.to_lowercase() == qual => t
                .col_index(name)
                .filter(|ci| t.sch.idx_defs.iter().any(|d| d.col == *ci)),
            _ => None,
        },
        _ => None,
    }
}

fn expr_conjuncts_side(
    a: &Expr,
    op: &Op,
    b: &Expr,
    t: &Table,
    qual: &str,
) -> Option<usize> {
    if !matches!(op, Op::Eq | Op::Gt | Op::Ge | Op::Lt | Op::Le) {
        return None;
    }
    let name = match a {
        Expr::Col { qual: Some(q), name } if q.to_lowercase() == qual => name,
        _ => return None,
    };
    if !matches!(b, Expr::Lit(_) | Expr::Neg(_)) {
        return None;
    }
    t.col_index(name)
        .filter(|ci| t.sch.idx_defs.iter().any(|d| d.col == *ci))
}

fn index_name_for(t: &Table, ci: usize) -> Option<String> {
    t.sch
        .idx_defs
        .iter()
        .find(|d| d.col == ci)
        .map(|d| d.name.clone())
}

fn access_line(table: &str, alias: Option<&str>, idx: Option<String>) -> String {
    let qual = match alias {
        Some(a) => a.to_lowercase(),
        None => table.to_lowercase(),
    };
    match idx {
        Some(name) => {
            let mut s = String::with_capacity(8 + qual.len() + name.len());
            s.push_str("INDEX ");
            s.push_str(&qual);
            s.push('.');
            s.push_str(&name);
            s
        }
        None => {
            let mut s = String::with_capacity(6 + qual.len());
            s.push_str("SCAN ");
            s.push_str(&qual);
            s
        }
    }
}

impl Db {
    /// Run a select and return its output rows as values (INSERT ... SELECT
    /// and scalar subqueries use the same machinery).
    fn exec_select_values(&self, sel: &Select) -> Result<Vec<Vec<Value>>, SqErr> {
        let table = self.build_row_space(sel, &[])?;
        if select_is_agg(sel) {
            let plan = build_agg_plan(sel, &table, self, &[])?;
            run_agg_plan_values(&table, &plan, &[])
        } else {
            let plan = build_plan(sel, &table, self, &[])?;
            run_plan_values(&table, &plan, &[])
        }
    }

    fn exec_select(&mut self, sel: &Select) -> Result<String, SqErr> {
        if !sel.joins.is_empty() {
            return self.exec_select_join(sel);
        }
        if select_is_agg(sel) {
            return self.exec_select_agg(sel);
        }
        if let Some(name) = &sel.from {
            let lower = name.to_lowercase();
            if !self.table_exists(&lower) {
                return Err(no_table(name));
            }
            self.ensure_declared_indexes(&lower);
            let saved = {
                let t = self.table_view_mut(&lower).unwrap();
                let old = t.alias.take();
                t.alias = sel.from_alias.as_ref().map(|a| a.to_lowercase());
                old
            };
            let plan_res = {
                let t = self.table_ref(&lower).unwrap();
                build_plan(sel, t, self, &[])
            };
            if let Some(t) = self.table_view_mut(&lower) {
                t.alias = saved;
            }
            let plan = plan_res?;
            let t = self.table_ref(&lower).unwrap();
            run_plan(t, &plan, &[])
        } else {
            // constant SELECT
            let t = Table {
                name: String::new(),
                lower: String::new(),
                alias: None,
                cols: Vec::new(),
                rows: RowSet::new(),
                idx: HashMap::new(),
                sch: Schema::default(),
            };
            let plan = build_plan(sel, &t, self, &[])?;
            let keep = match &plan.filter {
                Some(f) => truthy(&eval(f, &[], &t, &[])),
                None => true,
            };
            let mut out = String::new();
            out.push_str(&format!("ROWS {}\n", plan.headers.len()));
            for (i, h) in plan.headers.iter().enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                out.push_str(h);
            }
            out.push('\n');
            let mut count = 0usize;
            let rows_avail = if keep { 1usize } else { 0 };
            let skipped = plan.offset.min(rows_avail);
            let emitted = match plan.limit {
                Some(l) => (rows_avail - skipped).min(l),
                None => rows_avail - skipped,
            };
            if emitted > 0 {
                emit_row(&t, &plan, &[], &mut out, &[]);
                count = 1;
            }
            out.push_str(&format!("END {}\n", count));
            Ok(out)
        }
    }

    fn exec_select_agg(&mut self, sel: &Select) -> Result<String, SqErr> {
        if let Some(name) = &sel.from {
            let lower = name.to_lowercase();
            if !self.table_exists(&lower) {
                return Err(no_table(name));
            }
            self.ensure_declared_indexes(&lower);
            let saved = {
                let t = self.table_view_mut(&lower).unwrap();
                let old = t.alias.take();
                t.alias = sel.from_alias.as_ref().map(|a| a.to_lowercase());
                old
            };
            let plan_res = {
                let t = self.table_ref(&lower).unwrap();
                build_agg_plan(sel, t, self, &[])
            };
            if let Some(t) = self.table_view_mut(&lower) {
                t.alias = saved;
            }
            let plan = plan_res?;
            let t = self.table_ref(&lower).unwrap();
            let cands = candidates(t, plan.filter.as_ref(), &[])?;
            run_agg_plan(t, &plan, &cands, false, &[])
        } else {
            let t = Table {
                name: String::new(),
                lower: String::new(),
                alias: None,
                cols: Vec::new(),
                rows: RowSet::new(),
                idx: HashMap::new(),
                sch: Schema::default(),
            };
            let plan = build_agg_plan(sel, &t, self, &[])?;
            let keep = match &plan.filter {
                Some(f) => truthy(&eval(f, &[], &t, &[])),
                None => true,
            };
            run_agg_plan(&t, &plan, &[], keep, &[])
        }
    }

    fn execute(&mut self, stmt: &Stmt) -> Result<(String, Option<u64>), SqErr> {
        match stmt {
            Stmt::Create { name, cols, cons } => {
                let tok = self.create_table(name, cols, cons)?;
                Ok(("OK 0\n".to_string(), tok))
            }
            Stmt::CreateIndex { name, table, col, unique, if_not_exists } => {
                let tok = self.create_index(name, table, col, *unique, *if_not_exists)?;
                Ok(("OK 0\n".to_string(), tok))
            }
            Stmt::DropIndex { name, if_exists } => {
                let tok = self.drop_index(name, *if_exists)?;
                Ok(("OK 0\n".to_string(), tok))
            }
            Stmt::AlterAdd { table, name, ty, default, if_not_exists } => {
                let tok = self.alter_add(table, name, *ty, default, *if_not_exists)?;
                Ok(("OK 0\n".to_string(), tok))
            }
            Stmt::AlterDrop { table, name, if_exists } => {
                let tok = self.alter_drop(table, name, *if_exists)?;
                Ok(("OK 0\n".to_string(), tok))
            }
            Stmt::AlterRenameColumn { table, old, new } => {
                let tok = self.alter_rename_column(table, old, new)?;
                Ok(("OK 0\n".to_string(), tok))
            }
            Stmt::AlterRenameTable { table, new } => {
                let tok = self.alter_rename_table(table, new)?;
                Ok(("OK 0\n".to_string(), tok))
            }
            Stmt::Insert { table, cols, rows, select } => {
                if let Some(sel) = select {
                    let vals = self.exec_select_values(sel)?;
                    let srows: Vec<Vec<Expr>> = vals
                        .into_iter()
                        .map(|r| r.into_iter().map(Expr::Lit).collect())
                        .collect();
                    let (n, tok) = self.insert(table, cols.as_ref(), &srows)?;
                    Ok((format!("OK {}\n", n), tok))
                } else {
                    let (n, tok) = self.insert(table, cols.as_ref(), rows)?;
                    Ok((format!("OK {}\n", n), tok))
                }
            }
            Stmt::Update { table, sets, filter } => {
                let (n, tok) = self.update_rows(table, sets, filter)?;
                Ok((format!("OK {}\n", n), tok))
            }
            Stmt::Delete { table, filter } => {
                let (n, tok) = self.delete_rows(table, filter)?;
                Ok((format!("OK {}\n", n), tok))
            }
            Stmt::Select(sel) => Ok((self.exec_select(sel)?, None)),
            Stmt::Explain(inner) => Ok((self.explain(inner)?, None)),
            Stmt::ShowStats => Ok((STATS.show(), None)),
            Stmt::ShowRole => Ok((repl::show_role(), None)),
            Stmt::Pragma { name, arg } => {
                if name == "foreign_keys" && arg.is_none() {
                    Ok(("ROWS 1\nforeign_keys\n1\nEND 1\n".to_string(), None))
                } else {
                    Ok(("OK 0\n".to_string(), None))
                }
            }
            Stmt::Begin | Stmt::Commit | Stmt::Rollback => {
                Ok(("OK 0\n".to_string(), None))
            }
            Stmt::Noop => Ok(("OK 0\n".to_string(), None)),
        }
    }
}

// ---------------------------------------------------------------- protocol ---

fn process_line(line: &str, db: &Arc<Mutex<Db>>, conn: u64) -> Option<String> {
    let s = line.trim_matches(|c| c == ' ' || c == '\t' || c == '\r');
    if s.is_empty() {
        return None;
    }
    STATS.record_statement(s);
    let resp = process_line_inner(s, db, conn);
    if let Some(r) = &resp {
        if r.starts_with("ERR ") {
            STATS.errors.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }
    resp
}

fn process_line_inner(line: &str, db: &Arc<Mutex<Db>>, conn: u64) -> Option<String> {
    let s = line.trim_matches(|c| c == ' ' || c == '\t' || c == '\r');
    if s.is_empty() {
        return None;
    }
    // The statement budget covers lexing, parsing and execution; dropping the
    // guard clears it before the next statement on this connection.
    let _deadline = DeadlineGuard::arm();
    let toks = match lex(s) {
        Ok(t) => t,
        Err(e) => return Some(e.response()),
    };
    if toks.is_empty() {
        return None;
    }
    if toks.iter().all(|t| matches!(t.tk, Tk::P(';'))) {
        return Some("OK 0\n".to_string());
    }
    // Reject absurd nesting before the recursive-descent parser can touch the
    // thread stack. Parentheses are the only unbounded structural nesting.
    let mut nest = 0usize;
    for t in &toks {
        match t.tk {
            Tk::P('(') => {
                nest += 1;
                if nest > MAX_NEST {
                    return Some(limit_err("nesting too deep").response());
                }
            }
            Tk::P(')') => nest = nest.saturating_sub(1),
            _ => {}
        }
    }
    let mut p = Parser::new(toks, s);
    let stmt = match p.parse_statement() {
        Ok(st) => st,
        Err(e) => return Some(e.response()),
    };
    // Parsing a multi-megabyte statement counts against the same budget as
    // running it.
    if let Err(e) = tick() {
        return Some(e.response());
    }
    while p.eat_p(';') {}
    if !p.at_end() {
        return Some(SqErr::syntax("You can only execute one statement at a time.").response());
    }
    // The AST owns its strings; the token buffer can go before execution so
    // a multi-megabyte INSERT does not hold two copies at once.
    drop(p);

    let is_write = matches!(
        stmt,
        Stmt::Create { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::DropIndex { .. }
            | Stmt::AlterAdd { .. }
            | Stmt::AlterDrop { .. }
            | Stmt::AlterRenameColumn { .. }
            | Stmt::AlterRenameTable { .. }
            | Stmt::Insert { .. }
            | Stmt::Update { .. }
            | Stmt::Delete { .. }
    );

    // Replicas serve reads but refuse every mutation: the primary is the
    // only writer, and its log is the source of truth.
    if is_write && repl::is_replica() {
        return Some(SqErr::new("readonly", "replica").response());
    }

    let mut guard = match db.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.cur = conn;

    // A write has to grow the log. Refuse the whole statement before any row
    // is touched when the volume cannot hold it: a full disk must answer
    // `ERR full` cleanly and leave the table exactly as it was.
    if is_write {
        let rows = match &stmt {
            Stmt::Insert { rows, .. } => rows.len() as u64,
            _ => 1,
        };
        let need = s.len() as u64 + s.len() as u64 / 4 + 256 * rows + 8192;
        if !guard.have_space(need) {
            drop(guard);
            return Some(full_err().response());
        }
    }

    if matches!(stmt, Stmt::Begin) && !guard.txns.contains_key(&conn) {
        // Try to take the single-writer lease for the whole transaction.
        // That freezes committed state for its lifetime, so its reads form
        // a stable snapshot and a read-then-write can never lose an update.
        // If somebody else keeps the lease for long (an open write
        // transaction), begin without it rather than block: reads still see
        // the committed image and the first write waits (or is refused).
        let deadline = Instant::now() + Duration::from_millis(400);
        loop {
            if guard.writer.is_none() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let cv = guard.cv.clone();
            guard = match cv.wait_timeout(guard, deadline - now) {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
            guard.cur = conn;
        }
    } else if is_write {
        // One writer at a time: wait for the lease, but never forever — a
        // statement that cannot get it in time answers `ERR busy` instead
        // of hanging. Reads never wait, so a second connection keeps
        // seeing the committed image while a writer is open.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut busy = false;
        loop {
            match guard.writer {
                None => break,
                Some(w) if w == conn => break,
                Some(_) => {
                    let now = Instant::now();
                    if now >= deadline {
                        busy = true;
                        break;
                    }
                    let cv = guard.cv.clone();
                    guard = match cv.wait_timeout(guard, deadline - now) {
                        Ok((g, _)) => g,
                        Err(p) => p.into_inner().0,
                    };
                    guard.cur = conn;
                }
            }
        }
        // A transaction that began without the lease (because another
        // connection held it) and has since read committed state must not
        // write on top of a newer version: that is exactly the lost-update
        // case. Refuse it; the client retries from a fresh snapshot.
        if !busy {
            if let Some(tx) = guard.txns.get(&conn) {
                if !tx.lease && tx.read_committed && tx.snap != guard.data_gen {
                    busy = true;
                }
            }
        }
        if busy {
            guard.abort_txn(conn);
            return Some(SqErr::new("busy", "database is locked").response());
        }
        // Hold the lease for the rest of the statement (autocommit) or for
        // the rest of the transaction.
        guard.writer = Some(conn);
        if let Some(tx) = guard.txns.get_mut(&conn) {
            tx.lease = true;
        }
    }

    let in_txn = guard.txns.contains_key(&conn);
    let result = match stmt {
        Stmt::Begin => {
            let lease = guard.writer.is_none();
            guard.begin_txn(conn, lease).map(|r| (r, None))
        }
        Stmt::Commit => guard.commit_txn(conn).map(|r| (r, None)),
        Stmt::Rollback => guard.rollback_txn(conn).map(|r| (r, None)),
        _ => {
            if !is_write {
                // An open transaction is observing committed state; a later
                // write in it is only safe if that state did not move.
                if let Some(tx) = guard.txns.get_mut(&conn) {
                    tx.read_committed = true;
                }
            }
            let res = guard.execute(&stmt);
            if is_write && !in_txn {
                // Autocommit: the statement is its own transaction, so the
                // lease ends here and the version moves on.
                if res.is_ok() {
                    guard.data_gen += 1;
                }
                guard.writer = None;
                guard.cv.notify_all();
            }
            res
        }
    };
    let (resp, tok) = match result {
        Ok(v) => v,
        Err(e) => {
            guard.repl_buf.clear();
            return Some(e.response());
        }
    };
    let repl_batch = if repl::active() {
        std::mem::take(&mut guard.repl_buf)
    } else {
        Vec::new()
    };
    let wal = guard.wal.clone();
    let closed_txn = matches!(stmt, Stmt::Commit);
    drop(guard);

    if let (Some(t), Some(w)) = (tok, wal) {
        if let Err(e) = w.flush(t) {
            let mut guard = match db.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.recover_after_write_error();
            return Some(e.response());
        }
    }
    // Sync commit: the client's OK waits until a replica has this batch
    // durably on disk. An explicit COMMIT already fsynced its batch inside
    // `commit_txn`, so its records are the signal, not the local token.
    if repl::active() {
        if !repl_batch.is_empty() {
            repl::commit_batch(repl_batch);
        }
    } else if tok.is_some() {
        repl::note_commit();
    }
    if tok.is_some() || closed_txn {
        let need = {
            let guard = match db.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.wal_bytes > WAL_COMPACT_BYTES && guard.writer.is_none()
        };
        if need {
            let mut guard = match db.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.maybe_compact();
        }
    }
    Some(resp)
}

/// Keeps the open-connection count honest even when a handler unwinds.
struct ConnSlot;

impl Drop for ConnSlot {
    fn drop(&mut self) {
        CONNS_OPEN.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

/// Parse `AUTH <password>`: returns the supplied password (which may be
/// empty) or `None` when the line is not an AUTH command. Comparison and
/// reporting happen in the caller; the supplied bytes are never echoed.
fn auth_supplied(line: &str) -> Option<&str> {
    let s = line.trim_matches(|c| c == ' ' || c == '\t' || c == '\r');
    let b = s.as_bytes();
    if b.len() < 4 || !b[..4].eq_ignore_ascii_case(b"auth") {
        return None;
    }
    if b.len() > 4 && b[4] != b' ' && b[4] != b'\t' {
        return None;
    }
    Some(s[4..].trim_start_matches(|c| c == ' ' || c == '\t'))
}

fn handle_conn(mut sock: TcpStream, db: Arc<Mutex<Db>>, conn: u64, password: Option<String>) {
    let _ = sock.set_nodelay(true);
    STATS.conns_total.fetch_add(1, AtomicOrdering::Relaxed);
    STATS.conns_current.fetch_add(1, AtomicOrdering::Relaxed);
    let mut authed = password.is_none();
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];
    let mut discard = false;
    'conn: loop {
        // Discarding the tail of a statement that exceeded MAX_LINE. Bytes
        // after its newline are kept: they may be the next statement.
        if discard {
            match buf.iter().position(|&b| b == b'\n') {
                Some(off) => {
                    buf.drain(..=off);
                    discard = false;
                }
                None => buf.clear(),
            }
        }
        let mut out = String::new();
        let mut start = 0usize;
        while let Some(off) = buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + off;
            if end - start > MAX_LINE {
                STATS.statements.fetch_add(1, AtomicOrdering::Relaxed);
                STATS.errors.fetch_add(1, AtomicOrdering::Relaxed);
                out.push_str(&limit_err("statement too large").response());
            } else {
                let line = String::from_utf8_lossy(&buf[start..end]);
                let s = line.trim_matches(|c| c == ' ' || c == '\t' || c == '\r');
                if !s.is_empty() {
                    if let Some(rest) = s.strip_prefix(repl::MAGIC) {
                        // Cluster control traffic rides the same listener.
                        repl::handle_control(&sock, rest);
                        break 'conn;
                    }
                    if !authed {
                        let expected = password.as_deref().unwrap_or("");
                        match auth_supplied(s) {
                            Some(given) if given == expected => {
                                authed = true;
                                out.push_str("OK 0\n");
                            }
                            Some(_) => {
                                STATS.statements.fetch_add(1, AtomicOrdering::Relaxed);
                                STATS.errors.fetch_add(1, AtomicOrdering::Relaxed);
                                out.push_str(
                                    &SqErr::new("auth", "authentication failed").response(),
                                );
                            }
                            None => {
                                STATS.statements.fetch_add(1, AtomicOrdering::Relaxed);
                                STATS.errors.fetch_add(1, AtomicOrdering::Relaxed);
                                out.push_str(
                                    &SqErr::new("auth", "authentication required").response(),
                                );
                            }
                        }
                    } else {
                        // A hostile statement that trips an internal bug must
                        // not take down the process or the connection.
                        let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            process_line(&line, &db, conn)
                        }))
                        .unwrap_or_else(|_| Some(limit_err("statement failed").response()));
                        if let Some(resp) = resp {
                            out.push_str(&resp);
                        }
                    }
                }
            }
            start = end + 1;
        }
        if start > 0 {
            buf.drain(..start);
        }
        if !out.is_empty() && sock.write_all(out.as_bytes()).is_err() {
            break 'conn;
        }
        // A partial line over the limit: answer once, then swallow bytes up
        // to the terminating newline so the next statement still parses.
        if buf.len() > MAX_LINE {
            buf.clear();
            discard = true;
            STATS.statements.fetch_add(1, AtomicOrdering::Relaxed);
            STATS.errors.fetch_add(1, AtomicOrdering::Relaxed);
            if sock
                .write_all(limit_err("statement too large").response().as_bytes())
                .is_err()
            {
                break 'conn;
            }
        }
        match sock.read(&mut tmp) {
            Ok(0) => break 'conn,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break 'conn,
        }
    }
    // A connection that goes away mid-transaction rolls back: staged
    // changes never reached the log, so dropping them is all it takes.
    let mut guard = match db.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.end_conn(conn);
    drop(guard);
    STATS.conns_current.fetch_sub(1, AtomicOrdering::Relaxed);
}

// ---------------------------------------------------------------- main ---

fn parse_args() -> (u16, String, Option<String>, Option<u16>) {
    parse_args_from_full(env::args().skip(1).collect())
}

/// Kept for the unit tests: the historical three-value view.
fn parse_args_from(args: Vec<String>) -> (u16, String, Option<String>) {
    let (p, d, r, _) = parse_args_from_full(args);
    (p, d, r)
}

fn parse_args_from_full(args: Vec<String>) -> (u16, String, Option<String>, Option<u16>) {
    let mut port: u16 = env::var("SDB_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(5680);
    let mut data: String = env::var("SDB_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let mut replicaof: Option<String> = env::var("SDB_SQL_REPLICAOF").ok();
    let mut pg_port: Option<u16> = env::var("SDB_PG_PORT").ok().and_then(|s| s.parse().ok());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" if i + 1 < args.len() => {
                if let Ok(p) = args[i + 1].parse() {
                    port = p;
                }
                i += 2;
            }
            "--data" if i + 1 < args.len() => {
                data = args[i + 1].clone();
                i += 2;
            }
            "--replicaof" if i + 1 < args.len() => {
                replicaof = Some(args[i + 1].clone());
                i += 2;
            }
            "--pg-port" if i + 1 < args.len() => {
                if let Ok(p) = args[i + 1].parse() {
                    pg_port = Some(p);
                }
                i += 2;
            }
            a if a.starts_with("--port=") => {
                if let Ok(p) = a[7..].parse() {
                    port = p;
                }
                i += 1;
            }
            a if a.starts_with("--data=") => {
                data = a[7..].to_string();
                i += 1;
            }
            a if a.starts_with("--replicaof=") => {
                replicaof = Some(a[12..].to_string());
                i += 1;
            }
            a if a.starts_with("--pg-port=") => {
                if let Ok(p) = a[10..].parse() {
                    pg_port = Some(p);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    (port, data, replicaof, pg_port)
}

fn main() {
    // Panic payloads can quote attacker-controlled text; keep them off the
    // log entirely. The per-statement `catch_unwind` still answers the wire.
    std::panic::set_hook(Box::new(|_| {}));
    let (port, data, replicaof, pg_port) = parse_args();
    if let Ok(v) = env::var("SDB_SQL_STATEMENT_TIMEOUT_MS") {
        if let Ok(ms) = v.trim().parse::<u64>() {
            STMT_TIMEOUT_MS.store(ms, AtomicOrdering::Relaxed);
        }
    }
    let password = env::var("SDB_SQL_PASSWORD").ok();
    let db = Arc::new(Mutex::new(Db::open(&data)));
    repl::init(port, PathBuf::from(&data), replicaof, Arc::clone(&db));
    if let Some(pp) = pg_port {
        pg::init(pp, Arc::clone(&db), password.clone());
    }
    let addr = format!("0.0.0.0:{}", port);
    let listener = loop {
        match TcpListener::bind(&addr) {
            Ok(l) => break l,
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    };
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                // The accept loop is single-threaded, so the check-and-take
                // of a slot is atomic with respect to other accepts.
                let taken = CONNS_OPEN.fetch_add(1, AtomicOrdering::SeqCst);
                if taken >= MAX_CONNS as u64 {
                    CONNS_OPEN.fetch_sub(1, AtomicOrdering::SeqCst);
                    // Answer and close from a short-lived helper so the
                    // accept loop keeps serving. Draining the peer's pending
                    // bytes makes the close a FIN rather than a reset that
                    // could discard the error line.
                    thread::spawn(move || {
                        let _ = s.set_nodelay(true);
                        let _ = s.write_all(
                            limit_err("too many connections").response().as_bytes(),
                        );
                        let _ = s.shutdown(std::net::Shutdown::Write);
                        let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                        let mut junk = [0u8; 512];
                        let _ = s.read(&mut junk);
                    });
                    continue;
                }
                let id = next_conn_id();
                let d = Arc::clone(&db);
                let pw = password.clone();
                let spawned = thread::Builder::new()
                    // Deep-but-accepted expressions need more than the
                    // default worker stack; rejected ones never get here.
                    .stack_size(16 * 1024 * 1024)
                    .spawn(move || {
                        let _slot = ConnSlot;
                        handle_conn(s, d, id, pw);
                    });
                if spawned.is_err() {
                    CONNS_OPEN.fetch_sub(1, AtomicOrdering::SeqCst);
                }
            }
            Err(_) => continue,
        }
    }
}

mod pg;
mod repl;

#[cfg(test)]
mod tests;
