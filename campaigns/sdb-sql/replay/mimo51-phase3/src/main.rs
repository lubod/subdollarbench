//! sdb — a tiny SQL database server spoken over raw TCP.
//!
//! One statement per line, one response per statement, in order.
//! Standard library only.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::FileExt;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};

const MAX_LINE: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Values and errors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Value {
    Int(i64),
    Text(String),
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ColType {
    Int,
    Text,
}

struct SqErr {
    class: &'static str,
    msg: String,
}

impl SqErr {
    fn new(class: &'static str, msg: String) -> SqErr {
        SqErr { class, msg }
    }
    fn syn(msg: String) -> SqErr {
        SqErr::new("syntax", msg)
    }
    fn no_table(name: &str) -> SqErr {
        SqErr::new("no_table", format!("no such table: {}", name))
    }
    fn no_column(name: &str) -> SqErr {
        SqErr::new("no_column", format!("no such column: {}", name))
    }
    fn exists(name: &str) -> SqErr {
        SqErr::new("exists", format!("table {} already exists", name))
    }
    fn arity(msg: String) -> SqErr {
        SqErr::new("arity", msg)
    }
    fn typ(msg: String) -> SqErr {
        SqErr::new("type", msg)
    }
}

type R<T> = Result<T, SqErr>;

fn line_err(e: &SqErr) -> String {
    format!("ERR {} {}\n", e.class, e.msg)
}

// ---------------------------------------------------------------------------
// Reserved word sets (probed against the reference dialect)
// ---------------------------------------------------------------------------

const R_NAME: [&str; 58] = [
    "add", "all", "alter", "and", "as", "autoincrement", "between", "case", "check",
    "collate", "commit", "constraint", "create", "default", "deferrable", "delete",
    "distinct", "drop", "else", "escape", "except", "exists", "foreign", "from",
    "group", "having", "in", "index", "insert", "intersect", "into", "is", "isnull",
    "join", "limit", "not", "notnull", "nothing", "null", "on", "or", "order",
    "primary", "references", "returning", "select", "set", "table", "then", "to",
    "transaction", "union", "unique", "update", "using", "values", "when", "where",
];

fn res_name(w: &str) -> bool {
    R_NAME.contains(&w)
}

fn res_create(w: &str) -> bool {
    w == "if" || res_name(w)
}

fn res_expr(w: &str) -> bool {
    w != "null" && (res_name(w) || w == "cast" || w == "raise")
}

/// Words after a select item that continue the statement instead of being
/// an alias.
const CONTINUE: [&str; 10] = [
    "from", "where", "group", "having", "order", "limit", "offset", "asc", "desc", "nulls",
];

/// Words after a table name that continue the statement instead of being
/// a table alias (join operators, ON/USING, and the clause words).
const TABLE_CONT: [&str; 16] = [
    "inner", "left", "cross", "join", "on", "using", "from", "where", "group",
    "having", "order", "limit", "offset", "asc", "desc", "nulls",
];

/// Aggregate function names (ticket 7). Not reserved words: a column may
/// still be named `count`.
fn is_agg_name(w: &str) -> bool {
    matches!(w, "count" | "sum" | "min" | "max" | "avg")
}

// ---------------------------------------------------------------------------
// Tokens / lexer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Kind {
    Word(String),
    Num(String),
    Str(String),
    Punct(u8),
    /// Two-character operator: `<>`, `!=`, `<=`, `>=`, `||`.
    Op2([u8; 2]),
}

#[derive(Clone, Debug)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

fn lex(src: &str) -> R<Vec<Token>> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let mut out: Vec<Token> = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let st = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Token {
                kind: Kind::Word(src[st..i].to_string()),
                start: st,
                end: i,
            });
            continue;
        }
        if c.is_ascii_digit() {
            let st = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            // The dialect has no REAL literals: decimal points and exponents
            // are rejected the way the reference rejects REAL tokens.
            if i < b.len() && b[i] == b'.' {
                return Err(SqErr::syn("REAL/BLOB outside the dialect".to_string()));
            }
            if i < b.len()
                && (b[i] == b'e' || b[i] == b'E')
                && i + 1 < b.len()
                && (b[i + 1].is_ascii_digit() || b[i + 1] == b'+' || b[i + 1] == b'-')
            {
                return Err(SqErr::syn("REAL/BLOB outside the dialect".to_string()));
            }
            out.push(Token {
                kind: Kind::Num(src[st..i].to_string()),
                start: st,
                end: i,
            });
            continue;
        }
        if c == b'\'' {
            let st = i;
            i += 1;
            let mut val: Vec<u8> = Vec::new();
            let mut closed = false;
            while i < b.len() {
                if b[i] == b'\'' {
                    if i + 1 < b.len() && b[i + 1] == b'\'' {
                        val.push(b'\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                val.push(b[i]);
                i += 1;
            }
            if !closed {
                return Err(SqErr::syn("incomplete input".to_string()));
            }
            let text = String::from_utf8(val).unwrap_or_default();
            out.push(Token {
                kind: Kind::Str(text),
                start: st,
                end: i,
            });
            continue;
        }
        // `--` line comment and `/* ... */` block comment (reference dialect).
        if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            let mut closed = false;
            while i + 1 < b.len() {
                if b[i] == b'*' && b[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(SqErr::syn("unterminated comment".to_string()));
            }
            continue;
        }
        if c == b'(' || c == b')' || c == b',' || c == b'*' || c == b'-' || c == b'+' || c == b'.' {
            out.push(Token {
                kind: Kind::Punct(c),
                start: i,
                end: i + 1,
            });
            i += 1;
            continue;
        }
        // Arithmetic / comparison operators, including the two-char forms.
        if c == b'/' || c == b'%' || c == b'<' || c == b'>' || c == b'=' {
            if i + 1 < b.len() {
                let n = b[i + 1];
                let two = (c == b'<' && (n == b'>' || n == b'=')) || (c == b'>' && n == b'=');
                if two {
                    out.push(Token {
                        kind: Kind::Op2([c, n]),
                        start: i,
                        end: i + 2,
                    });
                    i += 2;
                    continue;
                }
            }
            out.push(Token {
                kind: Kind::Punct(c),
                start: i,
                end: i + 1,
            });
            i += 1;
            continue;
        }
        // `!=` and `||`; a lone `!` or `|` falls through to the error below.
        if (c == b'!' || c == b'|') && i + 1 < b.len() {
            let n = b[i + 1];
            if (c == b'!' && n == b'=') || (c == b'|' && n == b'|') {
                out.push(Token {
                    kind: Kind::Op2([c, n]),
                    start: i,
                    end: i + 2,
                });
                i += 2;
                continue;
            }
        }
        // Anything else is a syntax error.
        let mut end = i + 1;
        while end < b.len() && !b[end].is_ascii() {
            end += 1;
        }
        return Err(SqErr::syn(format!(
            "near \"{}\": syntax error",
            &src[i..end.min(b.len())]
        )));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Literals & statements
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Lit {
    Int(i64),
    /// Decimal digits outside i64 range (flows through as a REAL-ish value).
    Big { raw: String, neg: bool },
    Str(String),
    Null,
}

/// Per-column constraint metadata (ticket 9): `NOT NULL`, `UNIQUE` and
/// `DEFAULT <literal>`. Columns are stored in parallel with `Table::cols`.
#[derive(Clone, Debug)]
struct ColMeta {
    not_null: bool,
    unique: bool,
    default: Option<Lit>,
}

impl Default for ColMeta {
    fn default() -> ColMeta {
        ColMeta {
            not_null: false,
            unique: false,
            default: None,
        }
    }
}

/// A `REFERENCES` clause resolved to child column indexes (table-level and
/// column-level both live here). `pcols` empty means "the parent's primary
/// key".
#[derive(Clone, Debug)]
struct FkDef {
    cols: Vec<usize>,
    ptab: String,
    pcols: Vec<String>,
}

/// A fully parsed `CREATE TABLE` body: columns plus the constraints the
/// reference validated at create time (column references, one primary key).
#[derive(Clone, Debug)]
struct TableDef {
    cols: Vec<(String, ColType)>,
    meta: Vec<ColMeta>,
    pk: Option<Vec<usize>>,
    autoinc: bool,
    /// Declared composite `UNIQUE (...)` constraints (single-column ones are
    /// folded into the column metadata).
    multi_uq: Vec<Vec<usize>>,
    fks: Vec<FkDef>,
}

/// Binary operators. Precedence is encoded in the recursive-descent chain;
/// these only label the node.
#[derive(Clone, Debug)]
enum BinOp {
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
    Is,
}

#[derive(Clone, Debug)]
enum Expr {
    Lit(Lit),
    Col(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Func(String, Vec<Expr>),
    /// An aggregate call: `COUNT(*)`, `COUNT(x)`, `SUM(x)`, `MIN(x)`,
    /// `MAX(x)`, `AVG(x)` (plus the `DISTINCT` modifier forms). `star` is
    /// the bare `*` argument (row count); `COUNT()` with no argument also
    /// counts rows, like the reference.
    Agg {
        name: String,
        star: bool,
        distinct: bool,
        args: Vec<Expr>,
    },
    /// Qualified column `t.c` (table name or alias), from any join form.
    /// Rewritten against the FROM list before resolution/evaluation.
    QCol(String, String),
    /// `t.*` in a select item; expanded while building the projection.
    QStar(String),
    /// Scalar subquery `(SELECT ...)`: one output column. NULL over no rows,
    /// the first row otherwise; the column count is validated late (by
    /// `check_subs_width`), position-dependently like the reference.
    Sub(Box<Stmt>),
    /// `x [NOT] IN (SELECT ...)`.
    InSub {
        neg: bool,
        left: Box<Expr>,
        sub: Box<Stmt>,
    },
    /// `x [NOT] IN (v, v, ...)` — the list may be empty.
    InList {
        neg: bool,
        left: Box<Expr>,
        list: Vec<Expr>,
    },
    /// `EXISTS (SELECT ...)`; `NOT EXISTS` is `Not(Exists)`.
    Exists { sub: Box<Stmt> },
}

#[derive(Clone, Debug)]
enum SelItem {
    Star,
    E {
        expr: Expr,
        alias: Option<String>,
        raw: String,
    },
}

/// One table in a FROM list, with its optional alias. `derived` holds
/// `FROM (SELECT ...)`: the subquery runs and materializes as a temporary
/// table (its headers become the column names).
#[derive(Clone, Debug)]
struct TableRef {
    name: String,
    alias: Option<String>,
    derived: Option<Box<Stmt>>,
}

impl TableRef {
    /// Can a qualifier (`t.c`) refer to this table? Once aliased, only the
    /// alias can (the reference reports `no_column` for the original name).
    fn matches(&self, qualifier: &str) -> bool {
        match &self.alias {
            Some(a) => a == qualifier,
            None => self.name == qualifier,
        }
    }
}

/// Join flavor. Comma joins and `CROSS JOIN` are Inner with no condition;
/// `JOIN` without `ON` probes as a cross join too.
#[derive(Clone, Debug, Copy, PartialEq)]
enum JoinKind {
    Inner,
    Left,
}

/// One join step after the first table: `<kind> <table> [ON <e> |
/// USING (c, ...)]`.
#[derive(Clone, Debug)]
struct JoinStep {
    table: TableRef,
    kind: JoinKind,
    on: Option<Expr>,
    using: Vec<String>,
}

#[derive(Clone, Debug)]
enum OrdKind {
    Ordinal(Lit),
    Word(String),
    Const(Lit),
    /// A full expression in `ORDER BY` (SELECT only: `upper(c)`,
    /// `COUNT(*)`, `id + 1` …). DML never produces this.
    Expr(Expr),
}

#[derive(Clone, Debug)]
struct OrdSpec {
    kind: OrdKind,
    asc: bool,
    nulls_first: Option<bool>,
}

#[derive(Clone, Debug)]
enum LimVal {
    /// Decimal literal outside i64 range: a datatype mismatch, not a
    /// REAL/BLOB syntax error (reference behaviour in LIMIT position).
    Big,
    /// Full expression, evaluated without any table context.
    Expr(Expr),
}

#[derive(Clone, Debug, Default)]
struct LimitClause {
    limit: Option<LimVal>,
    offset: Option<LimVal>,
}

#[derive(Clone, Debug)]
enum Stmt {
    Create {
        name: String,
        def: TableDef,
    },
    Insert {
        table: String,
        cols: Option<Vec<String>>,
        rows: Vec<Vec<Lit>>,
    },
    /// `INSERT INTO t [cols] <select>` (ticket 10): the select's rows go
    /// through the same per-row pipeline as `VALUES`.
    InsertSel {
        table: String,
        cols: Option<Vec<String>>,
        select: Box<Stmt>,
    },
    Select {
        distinct: bool,
        items: Vec<SelItem>,
        from: Option<TableRef>,
        joins: Vec<JoinStep>,
        wexpr: Option<Expr>,
        group: Vec<Expr>,
        having: Option<Expr>,
        order: Vec<OrdSpec>,
        limit: LimitClause,
    },
    Update {
        table: String,
        /// The name expressions qualify the table with (alias, else table).
        vis: String,
        sets: Vec<(String, Expr)>,
        wexpr: Option<Expr>,
        order: Vec<OrdSpec>,
        limit: LimitClause,
    },
    Delete {
        table: String,
        vis: String,
        wexpr: Option<Expr>,
        order: Vec<OrdSpec>,
        limit: LimitClause,
    },
    /// `CREATE [UNIQUE] INDEX name ON table (col, ...)` (ticket 11).
    CreateIndex {
        unique: bool,
        name: String,
        table: String,
        cols: Vec<String>,
    },
    /// `DROP INDEX name` (ticket 11).
    DropIndex {
        name: String,
    },
    /// `BEGIN [TRANSACTION]` — open the connection's transaction.
    TxnBegin,
    /// `COMMIT [TRANSACTION]` / `END [TRANSACTION]` — commit it.
    TxnCommit,
    /// `ROLLBACK [TRANSACTION]` — discard it.
    TxnRollback,
}

/// What a schema name currently resolves to, for the parse-time
/// `CREATE TABLE` collision check: a table is `exists`, an index the
/// reference reports as `syntax` ("there is already an index named x").
#[derive(Clone, Copy, PartialEq)]
enum NameHit {
    Free,
    Table,
    Index,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Right-hand side of an `IN` predicate.
enum InRhs {
    Sub(Box<Stmt>),
    List(Vec<Expr>),
}

struct P<'a> {
    toks: Vec<Token>,
    src: &'a str,
    pos: usize,
    exists: &'a dyn Fn(&str) -> NameHit,
    /// Sub-select nesting depth: inside one, `SELECT` need not end the
    /// input (the enclosing `)` does).
    depth: usize,
}

impl<'a> P<'a> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn peek_word_lower(&self) -> Option<String> {
        match self.peek() {
            Some(t) => match &t.kind {
                Kind::Word(w) => Some(w.to_ascii_lowercase()),
                _ => None,
            },
            None => None,
        }
    }

    fn peek_word_raw(&self) -> Option<String> {
        match self.peek() {
            Some(t) => match &t.kind {
                Kind::Word(w) => Some(w.clone()),
                _ => None,
            },
            None => None,
        }
    }

    fn is_punct(&self, c: u8) -> bool {
        matches!(self.peek(), Some(t) if matches!(&t.kind, Kind::Punct(p) if *p == c))
    }

    fn is_op2(&self, op: &[u8; 2]) -> bool {
        matches!(self.peek(), Some(t) if matches!(&t.kind, Kind::Op2(o) if o == op))
    }

    fn eat_op2(&mut self, op: &[u8; 2]) -> bool {
        if self.is_op2(op) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_punct(&mut self, c: u8) -> bool {
        if self.is_punct(c) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn is_word_kw(&self, kw: &str) -> bool {
        match self.peek_word_lower() {
            Some(w) => w == kw,
            None => false,
        }
    }

    fn eat_word_kw(&mut self, kw: &str) -> bool {
        if self.is_word_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn tok_text(&self) -> String {
        match self.peek() {
            Some(t) => self.src[t.start..t.end].to_string(),
            None => String::new(),
        }
    }

    fn err_near(&self) -> SqErr {
        SqErr::syn(format!("near \"{}\": syntax error", self.tok_text()))
    }

    fn expect_eof(&self) -> R<()> {
        if self.depth > 0 {
            // Inside a sub-select the enclosing `)` ends the statement.
            return Ok(());
        }
        if self.pos >= self.toks.len() {
            Ok(())
        } else {
            Err(self.err_near())
        }
    }

    /// Read an identifier at a name position; reject reserved words.
    fn read_name(&mut self, reserved: fn(&str) -> bool) -> R<String> {
        match self.peek() {
            Some(t) => {
                if let Kind::Word(w) = &t.kind {
                    let lw = w.to_ascii_lowercase();
                    if reserved(&lw) {
                        return Err(SqErr::syn(format!(
                            "near \"{}\": syntax error",
                            w
                        )));
                    }
                    self.advance();
                    return Ok(lw);
                }
                Err(SqErr::syn("incomplete input".to_string()))
            }
            None => Err(SqErr::syn("incomplete input".to_string())),
        }
    }

    /// Optional `AS alias` / bare alias after an item; returns the raw alias.
    fn maybe_alias(&mut self) -> R<Option<String>> {
        if self.eat_word_kw("as") {
            let tok = self.peek().cloned();
            match tok {
                Some(t) => {
                    if let Kind::Word(w) = &t.kind {
                        let lw = w.to_ascii_lowercase();
                        if res_name(&lw) {
                            return Err(SqErr::syn(format!("near \"{}\": syntax error", w)));
                        }
                        self.advance();
                        return Ok(Some(w.clone()));
                    }
                    Err(SqErr::syn("incomplete input".to_string()))
                }
                None => Err(SqErr::syn("incomplete input".to_string())),
            }
        } else if let Some(w) = self.peek_word_raw() {
            let lw = w.to_ascii_lowercase();
            if CONTINUE.contains(&lw.as_str()) {
                return Ok(None);
            }
            if res_name(&lw) {
                return Err(SqErr::syn(format!("near \"{}\": syntax error", w)));
            }
            self.advance();
            Ok(Some(w))
        } else {
            Ok(None)
        }
    }

    /// An optional alias after a table name. Unlike a select-item alias,
    /// join operators and ON/USING end the alias position.
    fn table_alias(&mut self) -> R<Option<String>> {
        if self.eat_word_kw("as") {
            match self.peek().cloned() {
                Some(t) => {
                    if let Kind::Word(w) = &t.kind {
                        if res_name(&w.to_ascii_lowercase()) {
                            return Err(SqErr::syn(format!("near \"{}\": syntax error", w)));
                        }
                        self.advance();
                        return Ok(Some(w.clone()));
                    }
                    Err(SqErr::syn("incomplete input".to_string()))
                }
                None => Err(SqErr::syn("incomplete input".to_string())),
            }
        } else if let Some(w) = self.peek_word_raw() {
            let lw = w.to_ascii_lowercase();
            if TABLE_CONT.contains(&lw.as_str()) {
                return Ok(None);
            }
            if res_name(&lw) {
                return Err(SqErr::syn(format!("near \"{}\": syntax error", w)));
            }
            self.advance();
            Ok(Some(w))
        } else {
            Ok(None)
        }
    }

    /// Parse an integer/string/NULL literal, returning it with its source span.
    fn parse_lit_span(&mut self) -> R<(Lit, usize, usize)> {
        let mut neg = false;
        let mut signed = false;
        let start = self
            .peek()
            .map(|t| t.start)
            .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
        if self.is_punct(b'-') || self.is_punct(b'+') {
            signed = true;
            neg = self.is_punct(b'-');
            self.advance();
        }
        let tok = self
            .peek()
            .cloned()
                .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
        match tok.kind {
            Kind::Num(raw) => {
                self.advance();
                let lit = if neg {
                    match format!("-{}", raw).parse::<i64>() {
                        Ok(i) => Lit::Int(i),
                        Err(_) => Lit::Big { raw, neg: true },
                    }
                } else {
                    match raw.parse::<i64>() {
                        Ok(i) => Lit::Int(i),
                        Err(_) => Lit::Big { raw, neg: false },
                    }
                };
                Ok((lit, start, tok.end))
            }
            Kind::Str(s) => {
                if signed {
                    return Err(SqErr::syn("incomplete input".to_string()));
                }
                self.advance();
                Ok((Lit::Str(s), tok.start, tok.end))
            }
            Kind::Word(w) => {
                if w.to_ascii_lowercase() == "null" && !signed {
                    self.advance();
                    Ok((Lit::Null, tok.start, tok.end))
                } else {
                    Err(SqErr::syn(format!("near \"{}\": syntax error", w)))
                }
            }
            _ => Err(SqErr::syn("incomplete input".to_string())),
        }
    }

    fn parse_create(&mut self) -> R<Stmt> {
        // `CREATE [UNIQUE] INDEX ...` shares the head with `CREATE TABLE`.
        if self.is_word_kw("unique") || self.is_word_kw("index") {
            let unique = self.eat_word_kw("unique");
            if !self.eat_word_kw("index") {
                return Err(self.err_near());
            }
            return self.parse_index_tail(unique);
        }
        if !self.eat_word_kw("table") {
            return Err(self.err_near());
        }
        let name = self.read_name(res_create)?;
        if !self.eat_punct(b'(') {
            return Err(SqErr::syn("incomplete input".to_string()));
        }
        // The reference rejects an existing table once the column list opens,
        // before parsing the column definitions themselves. An index name
        // collides the same way, but as a syntax error.
        match (self.exists)(&name) {
            NameHit::Table => return Err(SqErr::exists(&name)),
            NameHit::Index => {
                return Err(SqErr::syn(format!(
                    "there is already an index named {}",
                    name
                )))
            }
            NameHit::Free => {}
        }
        let mut cols: Vec<(String, ColType)> = Vec::new();
        let mut meta: Vec<ColMeta> = Vec::new();
        let mut pk: Option<Vec<usize>> = None;
        let mut pk_decls = 0usize;
        let mut autoinc = false;
        let mut multi_uq: Vec<Vec<usize>> = Vec::new();
        let mut fks: Vec<FkDef> = Vec::new();
        let mut table_cons = false;
        loop {
            let starts_tc = matches!(
                self.peek_word_lower().as_deref(),
                Some("primary") | Some("unique") | Some("foreign") | Some("check")
                    | Some("constraint")
            );
            if !table_cons && starts_tc {
                table_cons = true;
            }
            if table_cons {
                // Once a table constraint appears only table constraints may
                // follow (the reference rejects a column definition there).
                if !starts_tc {
                    return Err(self.err_near());
                }
                if self.is_word_kw("constraint") {
                    self.advance();
                    self.read_name(res_name)?;
                }
                let lw = self
                    .peek_word_lower()
                    .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
                match lw.as_str() {
                    "primary" => {
                        self.advance();
                        if !self.eat_word_kw("key") {
                            return Err(self.err_near());
                        }
                        let idxs = self.read_col_list(&cols)?;
                        pk_decls += 1;
                        pk = Some(idxs);
                        if self.eat_word_kw("autoincrement") {
                            autoinc = true;
                        }
                        self.skip_conflict();
                    }
                    "unique" => {
                        self.advance();
                        let idxs = self.read_col_list(&cols)?;
                        if idxs.len() == 1 {
                            meta[idxs[0]].unique = true;
                        } else {
                            multi_uq.push(idxs);
                        }
                        self.skip_conflict();
                    }
                    "foreign" => {
                        self.advance();
                        if !self.eat_word_kw("key") {
                            return Err(self.err_near());
                        }
                        let fcols = self.read_fk_cols(&cols)?;
                        if !self.eat_word_kw("references") {
                            return Err(self.err_near());
                        }
                        let ptab = self.read_name(res_create)?;
                        let pcols = self.read_parent_cols()?;
                        self.skip_fk_tail()?;
                        fks.push(FkDef {
                            cols: fcols,
                            ptab,
                            pcols,
                        });
                    }
                    "check" => {
                        self.advance();
                        self.skip_parens()?;
                        self.skip_conflict();
                    }
                    _ => return Err(self.err_near()),
                }
            } else {
                let cname = self.read_name(res_name)?;
                let ci = cols.len();
                let ct = match self.peek_word_lower() {
                    Some(w) if w == "integer" => {
                        self.advance();
                        ColType::Int
                    }
                    Some(w) if w == "text" => {
                        self.advance();
                        ColType::Text
                    }
                    Some(w) => {
                        return Err(SqErr::syn(format!(
                            "unknown datatype for {}.{}: \"{}\"",
                            name, cname, w
                        )))
                    }
                    None => return Err(SqErr::syn("incomplete input".to_string())),
                };
                let mut m = ColMeta::default();
                loop {
                    if self.is_word_kw("constraint") {
                        self.advance();
                        self.read_name(res_name)?;
                    }
                    let cw = match self.peek_word_lower() {
                        Some(w) => w,
                        None => break,
                    };
                    match cw.as_str() {
                        "primary" => {
                            self.advance();
                            if !self.eat_word_kw("key") {
                                return Err(self.err_near());
                            }
                            pk_decls += 1;
                            pk = Some(vec![ci]);
                            if self.eat_word_kw("autoincrement") {
                                autoinc = true;
                            }
                            self.skip_conflict();
                        }
                        "not" => {
                            self.advance();
                            if !self.eat_word_kw("null") {
                                return Err(self.err_near());
                            }
                            m.not_null = true;
                            self.skip_conflict();
                        }
                        "null" => {
                            self.advance();
                        }
                        "unique" => {
                            self.advance();
                            m.unique = true;
                            self.skip_conflict();
                        }
                        "default" => {
                            self.advance();
                            m.default = Some(self.parse_lit_span()?.0);
                        }
                        "references" => {
                            self.advance();
                            let ptab = self.read_name(res_create)?;
                            let pcols = self.read_parent_cols()?;
                            self.skip_fk_tail()?;
                            fks.push(FkDef {
                                cols: vec![ci],
                                ptab,
                                pcols,
                            });
                        }
                        "check" => {
                            self.advance();
                            self.skip_parens()?;
                            self.skip_conflict();
                        }
                        "collate" => {
                            self.advance();
                            self.read_name(res_name)?;
                        }
                        _ => break,
                    }
                }
                cols.push((cname, ct));
                meta.push(m);
            }
            if self.eat_punct(b',') {
                continue;
            }
            if self.eat_punct(b')') {
                break;
            }
            return Err(self.err_near());
        }
        for i in 0..cols.len() {
            for j in (i + 1)..cols.len() {
                if cols[i].0 == cols[j].0 {
                    return Err(SqErr::new(
                        "exists",
                        format!("duplicate column name: {}", cols[i].0),
                    ));
                }
            }
        }
        if pk_decls > 1 {
            return Err(SqErr::syn(format!(
                "table \"{}\" has more than one primary key",
                name
            )));
        }
        self.expect_eof()?;
        Ok(Stmt::Create {
            name,
            def: TableDef {
                cols,
                meta,
                pk,
                autoinc,
                multi_uq,
                fks,
            },
        })
    }

    /// The tail of `CREATE [UNIQUE] INDEX name ON table (col, ...)`: name
    /// resolution (table, columns) is deferred to execution so the errors
    /// come out in the reference's order. An optional partial-index
    /// `WHERE ...` predicate is accepted and ignored — it only narrows
    /// which rows a unique index would guard, and every answer this
    /// engine gives must be the unindexed one anyway.
    fn parse_index_tail(&mut self, unique: bool) -> R<Stmt> {
        let name = self.read_name(res_create)?;
        if !self.eat_word_kw("on") {
            return Err(self.err_near());
        }
        let table = self.read_name(res_create)?;
        if !self.eat_punct(b'(') {
            return Err(SqErr::syn("incomplete input".to_string()));
        }
        let mut cols = Vec::new();
        loop {
            cols.push(self.read_name(res_name)?);
            if self.eat_punct(b',') {
                continue;
            }
            break;
        }
        if !self.eat_punct(b')') {
            return Err(self.err_near());
        }
        if self.eat_word_kw("where") {
            // Skip the predicate expression: statements are one line, so
            // everything left belongs to it.
            while self.pos < self.toks.len() {
                self.advance();
            }
        }
        self.expect_eof()?;
        Ok(Stmt::CreateIndex {
            unique,
            name,
            table,
            cols,
        })
    }

    /// `(col, col, ...)` after `PRIMARY KEY` / `UNIQUE`: a missing name is
    /// `no such column`.
    fn read_col_list(&mut self, cols: &[(String, ColType)]) -> R<Vec<usize>> {
        if !self.eat_punct(b'(') {
            return Err(self.err_near());
        }
        let mut names = Vec::new();
        loop {
            names.push(self.read_name(res_name)?);
            if self.eat_punct(b',') {
                continue;
            }
            if self.eat_punct(b')') {
                break;
            }
            return Err(self.err_near());
        }
        let mut out = Vec::with_capacity(names.len());
        for n in &names {
            match cols.iter().position(|(c, _)| c == n) {
                Some(i) => out.push(i),
                None => return Err(SqErr::no_column(n)),
            }
        }
        Ok(out)
    }

    /// `(col, col, ...)` after `FOREIGN KEY`: a missing name is the
    /// reference's `syntax` "unknown column ... in foreign key definition".
    fn read_fk_cols(&mut self, cols: &[(String, ColType)]) -> R<Vec<usize>> {
        if !self.eat_punct(b'(') {
            return Err(self.err_near());
        }
        let mut names = Vec::new();
        loop {
            names.push(self.read_name(res_name)?);
            if self.eat_punct(b',') {
                continue;
            }
            if self.eat_punct(b')') {
                break;
            }
            return Err(self.err_near());
        }
        let mut out = Vec::with_capacity(names.len());
        for n in &names {
            match cols.iter().position(|(c, _)| c == n) {
                Some(i) => out.push(i),
                None => {
                    return Err(SqErr::syn(format!(
                        "unknown column \"{}\" in foreign key definition",
                        n
                    )))
                }
            }
        }
        Ok(out)
    }

    /// `(col, ...)` after `REFERENCES parent`: names are checked against the
    /// parent only when the foreign key runs, like the reference.
    fn read_parent_cols(&mut self) -> R<Vec<String>> {
        if !self.eat_punct(b'(') {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        loop {
            names.push(self.read_name(res_name)?);
            if self.eat_punct(b',') {
                continue;
            }
            if self.eat_punct(b')') {
                break;
            }
            return Err(self.err_near());
        }
        Ok(names)
    }

    /// `ON CONFLICT <action>` after a constraint: accepted, ignored (the
    /// default ABORT behavior is what the implementation enforces).
    fn skip_conflict(&mut self) {
        if self.is_word_kw("on") {
            let next = self.toks.get(self.pos + 1).and_then(|t| match &t.kind {
                Kind::Word(w) => Some(w.to_ascii_lowercase()),
                _ => None,
            });
            if next.as_deref() == Some("conflict") {
                self.advance();
                self.advance();
                if matches!(self.peek(), Some(t) if matches!(&t.kind, Kind::Word(_))) {
                    self.advance();
                }
            }
        }
    }

    /// `ON DELETE/UPDATE <action>`, `MATCH x`, `DEFERRABLE ...` after a
    /// `REFERENCES` clause: parsed and ignored — the dialect's foreign keys
    /// only ever use the default RESTRICT-style behavior.
    fn skip_fk_tail(&mut self) -> R<()> {
        loop {
            if self.is_word_kw("on") {
                let second = self.toks.get(self.pos + 1).and_then(|t| match &t.kind {
                    Kind::Word(w) => Some(w.to_ascii_lowercase()),
                    _ => None,
                });
                match second.as_deref() {
                    Some("delete") | Some("update") => {
                        self.advance();
                        self.advance();
                        let act = self
                            .peek_word_lower()
                            .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
                        match act.as_str() {
                            "cascade" | "restrict" => self.advance(),
                            "set" => {
                                self.advance();
                                let w = self
                                    .peek_word_lower()
                                    .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
                                if w == "null" || w == "default" {
                                    self.advance();
                                } else {
                                    return Err(self.err_near());
                                }
                            }
                            "no" => {
                                self.advance();
                                let w = self
                                    .peek_word_lower()
                                    .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
                                if w == "action" {
                                    self.advance();
                                } else {
                                    return Err(self.err_near());
                                }
                            }
                            _ => return Err(self.err_near()),
                        }
                    }
                    _ => break,
                }
            } else if self.is_word_kw("match") {
                self.advance();
                self.read_name(res_name)?;
            } else if self.is_word_kw("deferrable") {
                self.advance();
                if self.is_word_kw("initially") {
                    self.advance();
                    let w = self
                        .peek_word_lower()
                        .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
                    if w == "deferred" || w == "immediate" {
                        self.advance();
                    } else {
                        return Err(self.err_near());
                    }
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Skip a balanced `(...)` group (`CHECK (...)`): walked token by token,
    /// so parentheses inside string literals cannot confuse the depth count.
    fn skip_parens(&mut self) -> R<()> {
        if !self.eat_punct(b'(') {
            return Err(self.err_near());
        }
        let mut depth = 1usize;
        loop {
            let t = self
                .peek()
                .cloned()
                .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
            match t.kind {
                Kind::Punct(c) if c == b'(' => {
                    depth += 1;
                    self.advance();
                }
                Kind::Punct(c) if c == b')' => {
                    depth -= 1;
                    self.advance();
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => self.advance(),
            }
        }
    }

    fn parse_insert(&mut self) -> R<Stmt> {
        if !self.eat_word_kw("into") {
            return Err(self.err_near());
        }
        let table = self.read_name(res_name)?;
        let mut col_list: Option<Vec<String>> = None;
        if self.eat_punct(b'(') {
            let mut list = Vec::new();
            loop {
                list.push(self.read_name(res_name)?);
                if self.eat_punct(b',') {
                    continue;
                }
                if self.eat_punct(b')') {
                    break;
                }
                return Err(self.err_near());
            }
            col_list = Some(list);
        }
        if !self.eat_word_kw("values") {
            // `INSERT INTO t [cols] <select>`: same target/arity rules as
            // VALUES, with the select's rows.
            if self.is_word_kw("select") {
                let sel = self.parse_subselect_top()?;
                return Ok(Stmt::InsertSel {
                    table,
                    cols: col_list,
                    select: Box::new(sel),
                });
            }
            // `INSERT INTO t [cols] DEFAULT VALUES` — every column reads its
            // default (with a column list the reference reports arity).
            if self.is_word_kw("default") {
                self.advance();
                if !self.eat_word_kw("values") {
                    return Err(self.err_near());
                }
                self.expect_eof()?;
                return Ok(Stmt::Insert {
                    table,
                    cols: col_list,
                    rows: vec![Vec::new()],
                });
            }
            return Err(self.err_near());
        }
        let mut rows: Vec<Vec<Lit>> = Vec::new();
        loop {
            if !self.eat_punct(b'(') {
                return Err(SqErr::syn("incomplete input".to_string()));
            }
            let mut lits = vec![self.parse_lit_span()?.0];
            while self.eat_punct(b',') {
                lits.push(self.parse_lit_span()?.0);
            }
            if !self.eat_punct(b')') {
                return Err(self.err_near());
            }
            rows.push(lits);
            if self.eat_punct(b',') {
                continue;
            }
            break;
        }
        let n = rows[0].len();
        if rows.iter().any(|r| r.len() != n) {
            return Err(SqErr::syn(
                "all VALUES must have the same number of terms".to_string(),
            ));
        }
        self.expect_eof()?;
        Ok(Stmt::Insert {
            table,
            cols: col_list,
            rows,
        })
    }

    /// `UPDATE <t> SET <col> = <expr> [, ...] [WHERE ...]`
    /// with the optional `ORDER BY ... LIMIT/OFFSET ...` tail the reference
    /// accepts on UPDATE.
    fn parse_update(&mut self) -> R<Stmt> {
        let table = self.read_name(res_name)?;
        // `UPDATE t AS a ...` — the reference accepts (only) the AS form.
        let alias = self.dml_alias()?;
        if !self.eat_word_kw("set") {
            return Err(self.err_near());
        }
        let mut sets: Vec<(String, Expr)> = Vec::new();
        loop {
            let col = self.read_name(res_name)?;
            if !self.eat_punct(b'=') {
                return Err(self.err_near());
            }
            let e = self.parse_expr()?;
            sets.push((col, e));
            if self.eat_punct(b',') {
                continue;
            }
            break;
        }
        let mut wexpr: Option<Expr> = None;
        if self.is_word_kw("where") {
            self.advance();
            wexpr = Some(self.parse_expr()?);
        }
        let (order, limit) = self.parse_order_limit(Some("UPDATE"))?;
        let vis = alias.unwrap_or_else(|| table.clone());
        let sets: Vec<(String, Expr)> = sets
            .into_iter()
            .map(|(c, e)| (c, dml_qual(&e, &vis)))
            .collect();
        let wexpr = wexpr.map(|e| dml_qual(&e, &vis));
        let mut order = order;
        for spec in order.iter_mut() {
            if let OrdKind::Expr(e) = &spec.kind {
                let e2 = dml_qual(e, &vis);
                spec.kind = OrdKind::Expr(e2);
            }
        }
        Ok(Stmt::Update {
            table,
            vis,
            sets,
            wexpr,
            order,
            limit,
        })
    }

    /// `AS <alias>` after a DML table name; a bare alias is a syntax error
    /// in the reference.
    fn dml_alias(&mut self) -> R<Option<String>> {
        if self.eat_word_kw("as") {
            let a = self.read_name(res_name)?;
            return Ok(Some(a));
        }
        Ok(None)
    }

    /// `DELETE FROM <t> [WHERE ...]`, likewise with `ORDER BY ... LIMIT`.
    fn parse_delete(&mut self) -> R<Stmt> {
        if !self.eat_word_kw("from") {
            return Err(self.err_near());
        }
        let table = self.read_name(res_name)?;
        let alias = self.dml_alias()?;
        let mut wexpr: Option<Expr> = None;
        if self.is_word_kw("where") {
            self.advance();
            wexpr = Some(self.parse_expr()?);
        }
        let (order, limit) = self.parse_order_limit(Some("DELETE"))?;
        let vis = alias.unwrap_or_else(|| table.clone());
        let wexpr = wexpr.map(|e| dml_qual(&e, &vis));
        let mut order = order;
        for spec in order.iter_mut() {
            if let OrdKind::Expr(e) = &spec.kind {
                let e2 = dml_qual(e, &vis);
                spec.kind = OrdKind::Expr(e2);
            }
        }
        Ok(Stmt::Delete {
            table,
            vis,
            wexpr,
            order,
            limit,
        })
    }

    fn parse_order_term(&mut self, exprs: bool) -> R<OrdKind> {
        if self.is_punct(b'-') || self.is_punct(b'+') {
            // Sign + integer is an ordinal; anything else is an expression.
            let num_next = matches!(
                self.toks.get(self.pos + 1),
                Some(t) if matches!(&t.kind, Kind::Num(_))
            );
            if !num_next {
                if exprs {
                    let e = self.parse_expr()?;
                    return Ok(OrdKind::Expr(e));
                }
                return Err(SqErr::syn("incomplete input".to_string()));
            }
            let neg = self.is_punct(b'-');
            self.advance();
            let tok = self.peek().cloned().unwrap_or(Token {
                kind: Kind::Punct(b'?'),
                start: 0,
                end: 0,
            });
            if let Kind::Num(raw) = tok.kind {
                self.advance();
                let lit = if neg {
                    match format!("-{}", raw).parse::<i64>() {
                        Ok(i) => Lit::Int(i),
                        Err(_) => Lit::Big { raw, neg: true },
                    }
                } else {
                    match raw.parse::<i64>() {
                        Ok(i) => Lit::Int(i),
                        Err(_) => Lit::Big { raw, neg: false },
                    }
                };
                return Ok(OrdKind::Ordinal(lit));
            }
            return Err(SqErr::syn("incomplete input".to_string()));
        }
        match self.peek().cloned() {
            Some(t) => match t.kind {
                Kind::Num(raw) => {
                    self.advance();
                    let lit = match raw.parse::<i64>() {
                        Ok(i) => Lit::Int(i),
                        Err(_) => Lit::Big { raw, neg: false },
                    };
                    Ok(OrdKind::Ordinal(lit))
                }
                Kind::Str(s) => {
                    self.advance();
                    Ok(OrdKind::Const(Lit::Str(s)))
                }
                Kind::Word(w) => {
                    let lw = w.to_ascii_lowercase();
                    if lw == "null" {
                        self.advance();
                        return Ok(OrdKind::Const(Lit::Null));
                    }
                    if exprs {
                        // A full expression: `id`, `upper(c)`, `count(*)` …
                        // parse_primary still rejects reserved words the
                        // same way the old word path did.
                        let e = self.parse_expr()?;
                        return Ok(OrdKind::Expr(e));
                    }
                    if res_expr(&lw) {
                        return Err(SqErr::syn(format!("near \"{}\": syntax error", w)));
                    }
                    self.advance();
                    Ok(OrdKind::Word(lw))
                }
                Kind::Punct(b'(') if exprs => {
                    let e = self.parse_expr()?;
                    Ok(OrdKind::Expr(e))
                }
                _ => Err(SqErr::syn("incomplete input".to_string())),
            },
            None => Err(SqErr::syn("incomplete input".to_string())),
        }
    }

    /// A LIMIT/OFFSET value: a full expression (`1 + 1`, `NOT 1`, `(2)`,
    /// function calls...) that the reference evaluates with no table context.
    /// An integer literal outside i64 range is a datatype mismatch rather
    /// than a syntax error.
    fn parse_limval(&mut self) -> R<LimVal> {
        // Sign + oversized decimal literal -> datatype mismatch later.
        let mut k = 0usize;
        let mut neg = false;
        if self.is_punct(b'-') || self.is_punct(b'+') {
            neg = self.is_punct(b'-');
            k = 1;
        }
        if let Some(t) = self.toks.get(self.pos + k) {
            if let Kind::Num(raw) = &t.kind {
                let parsed = if neg {
                    format!("-{}", raw).parse::<i64>()
                } else {
                    raw.parse::<i64>()
                };
                if parsed.is_err() {
                    self.pos += k + 1;
                    return Ok(LimVal::Big);
                }
            }
        }
        let r = self.parse_expr();
        r.map(LimVal::Expr)
    }

    // ---- expression grammar (precedence, tightest first) ------------------
    //
    // or -> and -> not -> equality -> relational -> concat -> add -> mul
    //     -> unary -> primary
    //
    // Left-associative at every binary level.

    fn parse_expr(&mut self) -> R<Expr> {
        self.parse_or()
    }

    /// An operand position may start with `NOT`; its operand then absorbs
    /// every operator tighter than `AND`, matching the reference parser.
    fn maybe_not_prefix(&mut self) -> R<Option<Expr>> {
        if self.is_word_kw("not") {
            self.advance();
            let inner = self.parse_equality()?;
            Ok(Some(Expr::Not(Box::new(inner))))
        } else {
            Ok(None)
        }
    }

    fn parse_or(&mut self) -> R<Expr> {
        let mut left = self.parse_and()?;
        while self.eat_word_kw("or") {
            let right = self.parse_and()?;
            left = Expr::Bin(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> R<Expr> {
        let mut left = self.parse_not()?;
        while self.eat_word_kw("and") {
            let right = self.parse_not()?;
            left = Expr::Bin(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> R<Expr> {
        if self.eat_word_kw("not") {
            let inner = self.parse_not()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_equality()
        }
    }

    fn parse_equality(&mut self) -> R<Expr> {
        let mut left = self.parse_relational()?;
        loop {
            if self.eat_punct(b'=') {
                let right = self.parse_relational()?;
                left = Expr::Bin(BinOp::Eq, Box::new(left), Box::new(right));
            } else if self.eat_op2(b"<>") {
                let right = self.parse_relational()?;
                left = Expr::Bin(BinOp::Ne, Box::new(left), Box::new(right));
            } else if self.eat_op2(b"!=") {
                let right = self.parse_relational()?;
                left = Expr::Bin(BinOp::Ne, Box::new(left), Box::new(right));
            } else if self.is_word_kw("is") {
                self.advance();
                let neg = self.eat_word_kw("not");
                let right = self.parse_relational()?;
                let e = Expr::Bin(BinOp::Is, Box::new(left), Box::new(right));
                left = if neg { Expr::Not(Box::new(e)) } else { e };
            } else if self.is_word_kw("in") || self.is_not_in() {
                let neg = self.is_word_kw("not");
                if neg {
                    self.advance(); // NOT
                }
                self.advance(); // IN
                let rhs = self.parse_in_rhs()?;
                left = match rhs {
                    InRhs::Sub(sub) => Expr::InSub {
                        neg,
                        left: Box::new(left),
                        sub,
                    },
                    InRhs::List(list) => Expr::InList {
                        neg,
                        left: Box::new(left),
                        list,
                    },
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// `NOT IN` ahead (a lone `NOT` never continues an equality chain).
    fn is_not_in(&self) -> bool {
        if !self.is_word_kw("not") {
            return false;
        }
        matches!(
            self.toks.get(self.pos + 1),
            Some(t) if matches!(&t.kind, Kind::Word(w) if w.to_ascii_lowercase() == "in")
        )
    }

    /// The right-hand side of `IN`: `(SELECT ...)` or `(v, v, ...)` — an
    /// empty list is allowed, like the reference.
    fn parse_in_rhs(&mut self) -> R<InRhs> {
        if !self.eat_punct(b'(') {
            return Err(self.err_near());
        }
        if self.peek_word_lower().as_deref() == Some("select") {
            let sub = self.parse_subselect()?;
            if !self.eat_punct(b')') {
                return Err(self.err_near());
            }
            return Ok(InRhs::Sub(Box::new(sub)));
        }
        let mut list: Vec<Expr> = Vec::new();
        if !self.is_punct(b')') {
            loop {
                list.push(self.parse_expr()?);
                if self.eat_punct(b',') {
                    continue;
                }
                break;
            }
        }
        if !self.eat_punct(b')') {
            return Err(self.err_near());
        }
        Ok(InRhs::List(list))
    }

    /// Is the next token `(` followed by `select`?
    fn next_is_subselect_paren(&self) -> bool {
        matches!(self.toks.get(self.pos + 1), Some(t) if matches!(&t.kind, Kind::Punct(p) if *p == b'('))
            && self
                .toks
                .get(self.pos + 2)
                .map(|t| matches!(&t.kind, Kind::Word(w) if w.to_ascii_lowercase() == "select"))
                .unwrap_or(false)
    }

    /// Parse a `SELECT ...` that sits inside parentheses (the current token
    /// is `SELECT`); the enclosing `)` is left for the caller.
    fn parse_subselect(&mut self) -> R<Stmt> {
        if self.peek_word_lower().as_deref() != Some("select") {
            return Err(self.err_near());
        }
        self.advance();
        self.depth += 1;
        let st = self.parse_select();
        self.depth -= 1;
        st
    }

    /// A statement-level `SELECT` (current token `SELECT`), used by
    /// `INSERT ... SELECT` where the select runs to the end of the input.
    fn parse_subselect_top(&mut self) -> R<Stmt> {
        if self.peek_word_lower().as_deref() != Some("select") {
            return Err(self.err_near());
        }
        self.advance();
        self.parse_select()
    }

    fn parse_relational(&mut self) -> R<Expr> {
        if let Some(e) = self.maybe_not_prefix()? {
            return Ok(e);
        }
        let mut left = self.parse_add()?;
        loop {
            let op = if self.is_op2(b"<=") {
                self.advance();
                Some(BinOp::Le)
            } else if self.is_op2(b">=") {
                self.advance();
                Some(BinOp::Ge)
            } else if self.is_punct(b'<') {
                self.advance();
                Some(BinOp::Lt)
            } else if self.is_punct(b'>') {
                self.advance();
                Some(BinOp::Gt)
            } else {
                None
            };
            let op = match op {
                Some(o) => o,
                None => break,
            };
            let right = self.parse_add()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // `||` binds tighter than `* / %`, which bind tighter than `+ -`
    // (reference precedence), so the chain is add -> mul -> concat -> unary.
    fn parse_concat(&mut self) -> R<Expr> {
        if let Some(e) = self.maybe_not_prefix()? {
            return Ok(e);
        }
        let mut left = self.parse_unary()?;
        while self.eat_op2(b"||") {
            let right = self.parse_unary()?;
            left = Expr::Bin(BinOp::Concat, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_add(&mut self) -> R<Expr> {
        if let Some(e) = self.maybe_not_prefix()? {
            return Ok(e);
        }
        let mut left = self.parse_mul()?;
        loop {
            let op = if self.is_punct(b'+') {
                BinOp::Add
            } else if self.is_punct(b'-') {
                BinOp::Sub
            } else {
                break;
            };
            self.advance();
            let right = self.parse_mul()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> R<Expr> {
        if let Some(e) = self.maybe_not_prefix()? {
            return Ok(e);
        }
        let mut left = self.parse_concat()?;
        loop {
            let op = if self.is_punct(b'*') {
                BinOp::Mul
            } else if self.is_punct(b'/') {
                BinOp::Div
            } else if self.is_punct(b'%') {
                BinOp::Mod
            } else {
                break
            };
            self.advance();
            let right = self.parse_concat()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> R<Expr> {
        if self.is_punct(b'-') {
            self.advance();
            let inner = self.parse_unary()?;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        if self.is_punct(b'+') {
            self.advance();
            return self.parse_unary();
        }
        if let Some(e) = self.maybe_not_prefix()? {
            return Ok(e);
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> R<Expr> {
        let tok = self
            .peek()
            .cloned()
            .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
        match tok.kind {
            Kind::Punct(b'(') => {
                self.advance();
                // `(SELECT ...)` is a scalar subquery, not a parenthesized
                // expression.
                if self.peek_word_lower().as_deref() == Some("select") {
                    let sub = self.parse_subselect()?;
                    if !self.eat_punct(b')') {
                        return Err(self.err_near());
                    }
                    return Ok(Expr::Sub(Box::new(sub)));
                }
                let e = self.parse_expr()?;
                if !self.eat_punct(b')') {
                    return Err(self.err_near());
                }
                Ok(e)
            }
            Kind::Num(raw) => {
                self.advance();
                match raw.parse::<i64>() {
                    Ok(i) => Ok(Expr::Lit(Lit::Int(i))),
                    Err(_) => Err(SqErr::syn("REAL/BLOB outside the dialect".to_string())),
                }
            }
            Kind::Str(s) => {
                self.advance();
                Ok(Expr::Lit(Lit::Str(s)))
            }
            Kind::Word(w) => {
                let lw = w.to_ascii_lowercase();
                if lw == "null" {
                    self.advance();
                    return Ok(Expr::Lit(Lit::Null));
                }
                // `EXISTS (SELECT ...)`: the parenthesized select is part of
                // the operator (a bare `EXISTS x` is a syntax error).
                if lw == "exists" && self.next_is_subselect_paren() {
                    self.advance(); // EXISTS
                    self.advance(); // '('
                    let sub = self.parse_subselect()?;
                    if !self.eat_punct(b')') {
                        return Err(self.err_near());
                    }
                    return Ok(Expr::Exists { sub: Box::new(sub) });
                }
                if res_expr(&lw) {
                    return Err(SqErr::syn(format!("near \"{}\": syntax error", w)));
                }
                // Qualified reference: `t.c` or `t.*` (table name or alias).
                if matches!(
                    self.toks.get(self.pos + 1),
                    Some(t) if matches!(&t.kind, Kind::Punct(b'.'))
                ) {
                    let next = self.toks.get(self.pos + 2).cloned();
                    match next.map(|t| t.kind) {
                        Some(Kind::Word(c)) => {
                            self.advance(); // qualifier
                            self.advance(); // '.'
                            self.advance(); // column
                            return Ok(Expr::QCol(w, c));
                        }
                        Some(Kind::Punct(b'*')) => {
                            self.advance();
                            self.advance();
                            self.advance();
                            return Ok(Expr::QStar(w));
                        }
                        _ => {}
                    }
                }
                let followed_by_paren = matches!(
                    self.toks.get(self.pos + 1),
                    Some(t) if matches!(&t.kind, Kind::Punct(b'('))
                );
                if followed_by_paren {
                    // Function call. Name/arity checks happen at resolution
                    // time so `no_table` still wins, like the reference.
                    self.advance(); // function name
                    self.advance(); // '('
                    if is_agg_name(&lw) {
                        // Aggregate call: `count(*)` / `count()` count rows;
                        // `sum(*)` is an arity error at resolution time, so
                        // the star form is accepted by the parser for every
                        // aggregate (like the reference).
                        if self.is_punct(b'*') {
                            self.advance();
                            if !self.eat_punct(b')') {
                                return Err(self.err_near());
                            }
                            return Ok(Expr::Agg {
                                name: lw,
                                star: true,
                                distinct: false,
                                args: Vec::new(),
                            });
                        }
                        let distinct = self.eat_word_kw("distinct");
                        let mut args: Vec<Expr> = Vec::new();
                        if !self.is_punct(b')') {
                            loop {
                                args.push(self.parse_expr()?);
                                if self.eat_punct(b',') {
                                    continue;
                                }
                                break;
                            }
                        }
                        if !self.eat_punct(b')') {
                            return Err(self.err_near());
                        }
                        return Ok(Expr::Agg {
                            name: lw,
                            star: false,
                            distinct,
                            args,
                        });
                    }
                    let mut args: Vec<Expr> = Vec::new();
                    if !self.is_punct(b')') {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.eat_punct(b',') {
                                continue;
                            }
                            break;
                        }
                    }
                    if !self.eat_punct(b')') {
                        return Err(self.err_near());
                    }
                    return Ok(Expr::Func(lw, args));
                }
                self.advance();
                Ok(Expr::Col(lw))
            }
            _ => Err(self.err_near()),
        }
    }

    /// Optional `ORDER BY ... LIMIT/OFFSET ...` tail shared by SELECT and
    /// the DML statements. `dml` names the statement kind when ORDER BY
    /// requires a LIMIT (the reference rejects a bare ORDER BY there).
    fn parse_order_limit(&mut self, dml: Option<&str>) -> R<(Vec<OrdSpec>, LimitClause)> {
        let mut order: Vec<OrdSpec> = Vec::new();
        if self.is_word_kw("order") {
            self.advance();
            if !self.eat_word_kw("by") {
                return Err(SqErr::syn("incomplete input".to_string()));
            }
            loop {
                let kind = self.parse_order_term(true)?;
                let mut asc = true;
                if self.eat_word_kw("asc") {
                    asc = true;
                } else if self.eat_word_kw("desc") {
                    asc = false;
                }
                let mut nf = None;
                if self.is_word_kw("nulls") {
                    self.advance();
                    if self.eat_word_kw("first") {
                        nf = Some(true);
                    } else if self.eat_word_kw("last") {
                        nf = Some(false);
                    } else {
                        return Err(SqErr::syn("incomplete input".to_string()));
                    }
                }
                order.push(OrdSpec {
                    kind,
                    asc,
                    nulls_first: nf,
                });
                if self.eat_punct(b',') {
                    continue;
                }
                break;
            }
        }

        let mut limit = LimitClause::default();
        if self.is_word_kw("limit") {
            self.advance();
            let a = self.parse_limval()?;
            if self.is_word_kw("offset") {
                self.advance();
                let b = self.parse_limval()?;
                limit.limit = Some(a);
                limit.offset = Some(b);
            } else if self.eat_punct(b',') {
                let b = self.parse_limval()?;
                limit.offset = Some(a);
                limit.limit = Some(b);
            } else {
                limit.limit = Some(a);
            }
        }

        if let Some(what) = dml {
            if !order.is_empty() && limit.limit.is_none() {
                return Err(SqErr::syn(format!(
                    "ORDER BY without LIMIT on {}",
                    what
                )));
            }
        }
        self.expect_eof()?;
        Ok((order, limit))
    }

    fn parse_select(&mut self) -> R<Stmt> {
        // `SELECT [ALL | DISTINCT] <items> ...` (the reference accepts ALL).
        let mut distinct = false;
        if self.eat_word_kw("distinct") {
            distinct = true;
        } else if self.eat_word_kw("all") {
            distinct = false;
        }
        let mut items: Vec<SelItem> = Vec::new();
        loop {
            let item = if self.is_punct(b'*') {
                self.advance();
                SelItem::Star
            } else {
                if self.pos >= self.toks.len() {
                    return Err(SqErr::syn("incomplete input".to_string()));
                }
                let st = self.pos;
                let expr = self.parse_expr()?;
                let last = self.pos - 1;
                let raw = self.src[self.toks[st].start..self.toks[last].end].to_string();
                let alias = self.maybe_alias()?;
                SelItem::E { expr, alias, raw }
            };
            items.push(item);
            if self.eat_punct(b',') {
                continue;
            }
            break;
        }

        let mut from: Option<TableRef> = None;
        let mut joins: Vec<JoinStep> = Vec::new();
        if self.is_word_kw("from") {
            self.advance();
            let mut kind = JoinKind::Inner;
            let mut first = true;
            loop {
                // `FROM (SELECT ...)` is a derived table; its alias is
                // optional (like the reference).
                let (name, derived) = if self.is_punct(b'(') {
                    self.advance(); // '('
                    let st = self.parse_subselect()?;
                    if !self.eat_punct(b')') {
                        return Err(self.err_near());
                    }
                    (String::new(), Some(Box::new(st)))
                } else {
                    (self.read_name(res_name)?, None)
                };
                let alias = self.table_alias()?;
                let refer = TableRef {
                    name,
                    alias,
                    derived,
                };
                if first {
                    from = Some(refer);
                    first = false;
                } else {
                    // Every non-first table may carry ON/USING — the
                    // reference accepts it after a comma join as well as
                    // after an explicit JOIN operator.
                    let mut on = None;
                    let mut using: Vec<String> = Vec::new();
                    if self.is_word_kw("on") {
                        self.advance();
                        on = Some(self.parse_expr()?);
                    } else if self.is_word_kw("using") {
                        self.advance();
                        if !self.eat_punct(b'(') {
                            return Err(SqErr::syn("incomplete input".to_string()));
                        }
                        loop {
                            using.push(self.read_name(res_name)?);
                            if self.eat_punct(b',') {
                                continue;
                            }
                            break;
                        }
                        if !self.eat_punct(b')') {
                            return Err(self.err_near());
                        }
                    }
                    joins.push(JoinStep {
                        table: refer,
                        kind,
                        on,
                        using,
                    });
                    kind = JoinKind::Inner;
                }
                if self.eat_punct(b',') {
                    continue;
                }
                // Optional join operator before the next table.
                if self.is_word_kw("inner") {
                    self.advance();
                    if !self.eat_word_kw("join") {
                        return Err(SqErr::syn("incomplete input".to_string()));
                    }
                    kind = JoinKind::Inner;
                } else if self.is_word_kw("left") {
                    self.advance();
                    self.eat_word_kw("outer");
                    if !self.eat_word_kw("join") {
                        return Err(SqErr::syn("incomplete input".to_string()));
                    }
                    kind = JoinKind::Left;
                } else if self.is_word_kw("cross") {
                    self.advance();
                    if !self.eat_word_kw("join") {
                        return Err(SqErr::syn("incomplete input".to_string()));
                    }
                    kind = JoinKind::Inner;
                } else if self.is_word_kw("join") {
                    self.advance();
                    kind = JoinKind::Inner;
                } else {
                    break;
                }
            }
        }

        let mut wexpr: Option<Expr> = None;
        if self.is_word_kw("where") {
            self.advance();
            wexpr = Some(self.parse_expr()?);
        }

        // `GROUP BY <expr> [, <expr> …]` then optional `HAVING <expr>`.
        let mut group: Vec<Expr> = Vec::new();
        if self.is_word_kw("group") {
            self.advance();
            if !self.eat_word_kw("by") {
                return Err(SqErr::syn("incomplete input".to_string()));
            }
            loop {
                group.push(self.parse_expr()?);
                if self.eat_punct(b',') {
                    continue;
                }
                break;
            }
        }
        let mut having: Option<Expr> = None;
        if self.is_word_kw("having") {
            self.advance();
            having = Some(self.parse_expr()?);
        }

        let (order, limit) = self.parse_order_limit(None)?;
        Ok(Stmt::Select {
            distinct,
            items,
            from,
            joins,
            wexpr,
            group,
            having,
            order,
            limit,
        })
    }
}

fn parse(src: &str, exists: &dyn Fn(&str) -> NameHit) -> R<Stmt> {
    let toks = lex(src)?;
    let mut p = P {
        toks,
        src,
        pos: 0,
        exists,
        depth: 0,
    };
    let head = p.peek_word_lower();
    match head.as_deref() {
        Some("create") => {
            p.advance();
            p.parse_create()
        }
        Some("insert") => {
            p.advance();
            p.parse_insert()
        }
        Some("update") => {
            p.advance();
            p.parse_update()
        }
        Some("delete") => {
            p.advance();
            p.parse_delete()
        }
        Some("select") => {
            p.advance();
            p.parse_select()
        }
        Some("drop") => {
            p.advance();
            if !p.eat_word_kw("index") {
                return Err(p.err_near());
            }
            let name = p.read_name(res_create)?;
            p.expect_eof()?;
            Ok(Stmt::DropIndex { name })
        }
        // Transaction control: `[TRANSACTION]` is the only trailing word the
        // reference accepts (`BEGIN WORK` is a syntax error, `END` commits).
        Some("begin") => {
            p.advance();
            p.eat_word_kw("transaction");
            p.expect_eof()?;
            Ok(Stmt::TxnBegin)
        }
        Some("commit") | Some("end") => {
            p.advance();
            p.eat_word_kw("transaction");
            p.expect_eof()?;
            Ok(Stmt::TxnCommit)
        }
        Some("rollback") => {
            p.advance();
            p.eat_word_kw("transaction");
            p.expect_eof()?;
            Ok(Stmt::TxnRollback)
        }
        _ => Err(SqErr::syn("incomplete input".to_string())),
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Table {
    cols: Vec<(String, ColType)>,
    idx: HashMap<String, usize>,
    rows: Vec<Vec<Value>>,
    /// Constraint metadata, parallel to `cols`.
    meta: Vec<ColMeta>,
    /// Declared primary key columns (in order).
    pk: Option<Vec<usize>>,
    /// The single INTEGER primary key column: the rowid alias, assigned
    /// `max + 1` when NULL/omitted.
    rowid: Option<usize>,
    /// AUTOINCREMENT: the monotonic sequence (only with `rowid`).
    autoinc: bool,
    seq: i64,
    /// Highest rowid currently stored (`None` on an empty rowid table).
    rowid_max: Option<i64>,
    /// Single-column unique constraints (declared UNIQUE plus any single
    /// column primary key) with counts of non-NULL values. Always valid:
    /// born correct at CREATE/load and maintained by every write.
    single_uq: Vec<(usize, HashMap<Value, usize>)>,
    /// Composite unique constraints (declared ones plus a composite primary
    /// key): column list + count of non-NULL tuples. Rebuilt lazily.
    multi_uq: Vec<(Vec<usize>, HashMap<Vec<Value>, usize>)>,
    /// Per-foreign-key counts of non-NULL child tuples (parent-side
    /// restrict checks). Rebuilt lazily.
    fkcnt: Vec<HashMap<Vec<Value>, usize>>,
    /// The lazy maps (`multi_uq`, `fkcnt`) hold current counts.
    maps_ok: bool,
    /// `REFERENCES` clauses this table declares (child side).
    fks: Vec<FkDef>,
    /// Named indexes over this table (ticket 11): schema objects with
    /// their physical entry lists.
    indexes: Vec<IndexDef>,
    /// Physical row order equals rowid order (always true without a rowid
    /// alias, and maintained across writes): a rowid-ordered scan of
    /// `rows` needs no sort, which is how the reference scans.
    seq_scan: bool,
}

/// A named index (ticket 11). `entries` holds one
/// (first-column value, rowid key, row position) triple per row, sorted by
/// (value, rowid key): point and range predicates binary-search it, an
/// `ORDER BY` on the column reads it in order, and `cols.len() == 1`
/// makes that order the reference's index order exactly.
#[derive(Clone)]
struct IndexDef {
    name: String,
    cols: Vec<usize>,
    unique: bool,
    /// Tuple counts of non-NULL key tuples for a UNIQUE index over several
    /// columns. A single-column UNIQUE index rides on `Table::single_uq`
    /// instead, so every existing constraint check applies to it as-is.
    tcount: HashMap<Vec<Value>, usize>,
    entries: Vec<(Value, i64, usize)>,
}

impl IndexDef {
    fn sort_entries(&mut self) {
        self.entries
            .sort_by(|a, b| cmp_val(&a.0, &b.0).then(a.1.cmp(&b.1)));
    }
}

impl Table {
    fn from_def(def: TableDef) -> Table {
        let mut idx = HashMap::new();
        for (i, (n, _)) in def.cols.iter().enumerate() {
            idx.entry(n.clone()).or_insert(i);
        }
        let ncol = def.cols.len();
        let mut meta = def.meta;
        meta.resize(ncol, ColMeta::default());
        let rowid = match &def.pk {
            Some(p) if p.len() == 1 && def.cols[p[0]].1 == ColType::Int => Some(p[0]),
            _ => None,
        };
        if let Some(r) = rowid {
            // The rowid alias can never store NULL: INSERT assigns, UPDATE
            // rejects NULL as a datatype mismatch.
            meta[r].not_null = true;
        } else if let Some(p) = &def.pk {
            // A non-rowid primary key (TEXT or composite) is NOT NULL too.
            for &i in p {
                meta[i].not_null = true;
            }
        }
        let mut single: Vec<usize> = Vec::new();
        for (i, m) in meta.iter().enumerate() {
            if m.unique {
                single.push(i);
            }
        }
        if let Some(p) = &def.pk {
            if p.len() == 1 && !single.contains(&p[0]) {
                single.push(p[0]);
            }
        }
        single.sort_unstable();
        let single_uq = single
            .into_iter()
            .map(|i| (i, HashMap::new()))
            .collect::<Vec<_>>();
        let mut multi_keys = def.multi_uq;
        if let Some(p) = &def.pk {
            if p.len() > 1 {
                multi_keys.push(p.clone());
            }
        }
        let multi_uq = multi_keys
            .into_iter()
            .map(|k| (k, HashMap::new()))
            .collect::<Vec<_>>();
        let autoinc = def.autoinc && rowid.is_some();
        let fks = def.fks;
        let fkcnt = vec![HashMap::new(); fks.len()];
        Table {
            cols: def.cols,
            idx,
            rows: Vec::new(),
            meta,
            pk: def.pk,
            rowid,
            autoinc,
            seq: 0,
            rowid_max: None,
            single_uq,
            multi_uq,
            fkcnt,
            maps_ok: true,
            fks,
            indexes: Vec::new(),
            seq_scan: true,
        }
    }

    /// A row-less stand-in for join synthesis: reads only, never written.
    fn bare(cols: Vec<(String, ColType)>) -> Table {
        let mut idx = HashMap::new();
        for (i, (n, _)) in cols.iter().enumerate() {
            idx.entry(n.clone()).or_insert(i);
        }
        let ncol = cols.len();
        Table {
            cols,
            idx,
            rows: Vec::new(),
            meta: vec![ColMeta::default(); ncol],
            pk: None,
            rowid: None,
            autoinc: false,
            seq: 0,
            rowid_max: None,
            single_uq: Vec::new(),
            multi_uq: Vec::new(),
            fkcnt: Vec::new(),
            maps_ok: true,
            fks: Vec::new(),
            indexes: Vec::new(),
            seq_scan: true,
        }
    }

    /// Rebuild the lazily maintained count maps from `rows`. Idempotent, so
    /// it may run whenever a stale map is first read.
    fn ensure_maps(&mut self) {
        if self.maps_ok {
            return;
        }
        for (keycols, map) in self.multi_uq.iter_mut() {
            map.clear();
            for row in &self.rows {
                let mut key = Vec::with_capacity(keycols.len());
                let mut ok = true;
                for &c in keycols.iter() {
                    match &row[c] {
                        Value::Null => {
                            ok = false;
                            break;
                        }
                        v => key.push(v.clone()),
                    }
                }
                if ok {
                    *map.entry(key).or_insert(0) += 1;
                }
            }
        }
        for (fi, fk) in self.fks.iter().enumerate() {
            let map = &mut self.fkcnt[fi];
            map.clear();
            for row in &self.rows {
                let mut key = Vec::with_capacity(fk.cols.len());
                let mut ok = true;
                for &c in &fk.cols {
                    match &row[c] {
                        Value::Null => {
                            ok = false;
                            break;
                        }
                        v => key.push(v.clone()),
                    }
                }
                if ok {
                    *map.entry(key).or_insert(0) += 1;
                }
            }
        }
        self.maps_ok = true;
    }

    /// Every unique constraint's column list, single first (for structural
    /// foreign-key validation: the parent key must match one exactly).
    fn unique_keys(&self) -> Vec<Vec<usize>> {
        let mut out: Vec<Vec<usize>> = self.single_uq.iter().map(|(i, _)| vec![*i]).collect();
        out.extend(self.multi_uq.iter().map(|(k, _)| k.clone()));
        out
    }

    /// The rowid-order key of the stored row at `pos`: the rowid alias's
    /// value on a rowid table, the physical position otherwise (the
    /// implicit rowid *is* insertion order there). Index entries sort by
    /// it so ties order exactly like the reference's index scans.
    fn row_key_at(&self, pos: usize) -> i64 {
        self.row_key_of(&self.rows[pos], pos)
    }

    fn row_key_of(&self, row: &[Value], pos: usize) -> i64 {
        match self.rowid {
            Some(r) => match &row[r] {
                Value::Int(i) => *i,
                _ => pos as i64,
            },
            None => pos as i64,
        }
    }

    /// Recompute which columns carry a single-column unique constraint:
    /// declared UNIQUE, a single-column primary key, or a UNIQUE index.
    /// Count maps survive for columns that stay; a newly covered column
    /// gets its counts built from the rows.
    fn reindex_membership(&mut self) {
        let mut want: Vec<usize> = Vec::new();
        for (i, m) in self.meta.iter().enumerate() {
            if m.unique {
                want.push(i);
            }
        }
        if let Some(p) = &self.pk {
            if p.len() == 1 && !want.contains(&p[0]) {
                want.push(p[0]);
            }
        }
        for ix in &self.indexes {
            if ix.unique && ix.cols.len() == 1 && !want.contains(&ix.cols[0]) {
                want.push(ix.cols[0]);
            }
        }
        want.sort_unstable();
        let have: Vec<usize> = self.single_uq.iter().map(|(c, _)| *c).collect();
        if want == have {
            return;
        }
        let mut old: HashMap<usize, HashMap<Value, usize>> =
            std::mem::take(&mut self.single_uq).into_iter().collect();
        for c in want {
            let map = old.remove(&c).unwrap_or_else(|| {
                let mut m: HashMap<Value, usize> = HashMap::new();
                for row in &self.rows {
                    let v = &row[c];
                    if *v != Value::Null {
                        *m.entry(v.clone()).or_insert(0) += 1;
                    }
                }
                m
            });
            self.single_uq.push((c, map));
        }
    }

    /// Rebuild every index's physical entries (and multi-column unique
    /// counts) from the stored rows.
    fn rebuild_index_entries(&mut self) {
        let rid = self.rowid;
        for ix in self.indexes.iter_mut() {
            let c0 = ix.cols[0];
            let mut entries: Vec<(Value, i64, usize)> = Vec::with_capacity(self.rows.len());
            for (p, row) in self.rows.iter().enumerate() {
                let key = match rid {
                    Some(r) => match &row[r] {
                        Value::Int(i) => *i,
                        _ => p as i64,
                    },
                    None => p as i64,
                };
                entries.push((row[c0].clone(), key, p));
            }
            entries.sort_by(|a, b| cmp_val(&a.0, &b.0).then(a.1.cmp(&b.1)));
            ix.entries = entries;
            ix.tcount.clear();
            if ix.unique && ix.cols.len() > 1 {
                for row in &self.rows {
                    if let Some(k) = key_tuple(&ix.cols, row) {
                        *ix.tcount.entry(k).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    /// Physical order still equals rowid order?
    fn recompute_seq_scan(&mut self) {
        self.seq_scan = match self.rowid {
            None => true,
            Some(rid) => {
                let mut ok = true;
                let mut prev: Option<i64> = None;
                for row in &self.rows {
                    if let Value::Int(i) = &row[rid] {
                        if let Some(p) = prev {
                            if *i < p {
                                ok = false;
                                break;
                            }
                        }
                        prev = Some(*i);
                    }
                }
                ok
            }
        };
    }

    /// Position of `name` among this table's indexes.
    fn index_pos(&self, name: &str) -> Option<usize> {
        self.indexes.iter().position(|i| i.name == name)
    }

    /// Does `idxs` exactly match one of this table's unique constraints (the
    /// parent key requirement of `REFERENCES`)?
    fn exact_unique(&self, idxs: &[usize]) -> bool {
        self.unique_keys().iter().any(|k| k == idxs)
    }

    /// Recompute the always-valid maps from the loaded rows and mark the
    /// lazy ones stale (they rebuild on first use).
    fn rebuild_after_load(&mut self) {
        // UNIQUE-index columns join the single-column unique set before
        // the counts are filled, and the physical index structures are
        // rebuilt from the rows alongside them.
        self.reindex_membership();
        for (ci, map) in self.single_uq.iter_mut() {
            map.clear();
            let ci = *ci;
            for row in &self.rows {
                if let Value::Int(_) = &row[ci] {
                    *map.entry(row[ci].clone()).or_insert(0) += 1;
                } else if let Value::Text(_) = &row[ci] {
                    *map.entry(row[ci].clone()).or_insert(0) += 1;
                }
            }
        }
        if let Some(r) = self.rowid {
            self.rowid_max = self
                .rows
                .iter()
                .filter_map(|row| match &row[r] {
                    Value::Int(i) => Some(*i),
                    _ => None,
                })
                .max();
        }
        self.rebuild_index_entries();
        self.recompute_seq_scan();
        self.maps_ok = false;
    }

    /// Next auto-assigned rowid for a plain (non-AUTOINCREMENT) key.
    fn next_plain_rowid(&self) -> Option<i64> {
        match self.rowid_max {
            None => Some(1),
            Some(m) => {
                if m == i64::MAX {
                    // Exhausted: search upward from 1 for a free slot, like
                    // the reference falls back to.
                    let r = self.rowid.unwrap();
                    let mut cand = 1i64;
                    loop {
                        if !self.rows.iter().any(|row| row[r] == Value::Int(cand)) {
                            return Some(cand);
                        }
                        if cand == i64::MAX {
                            return None;
                        }
                        cand += 1;
                    }
                } else {
                    Some(m + 1)
                }
            }
        }
    }
}

struct Db {
    tables: HashMap<String, Table>,
    wal: Option<Wal>,
}

/// What `n` currently names in the schema: a table, an index (names are
/// global across both), or nothing. Feeds the parse-time collision check.
fn name_hit(tables: &HashMap<String, Table>, n: &str) -> NameHit {
    if tables.contains_key(n) {
        NameHit::Table
    } else if tables.values().any(|t| t.indexes.iter().any(|i| i.name == n)) {
        NameHit::Index
    } else {
        NameHit::Free
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

fn lit_value(l: &Lit) -> Value {
    match l {
        Lit::Int(i) => Value::Int(*i),
        Lit::Str(s) => Value::Text(s.clone()),
        Lit::Null => Value::Null,
        Lit::Big { .. } => Value::Int(0), // unreachable in SELECT (parse rejects)
    }
}

/// A select result value as an insert literal (lossless: INTEGER, TEXT and
/// NULL are exactly the dialect's literals) for `INSERT ... SELECT`.
fn value_to_lit(v: Value) -> Lit {
    match v {
        Value::Int(i) => Lit::Int(i),
        Value::Text(s) => Lit::Str(s),
        Value::Null => Lit::Null,
    }
}

/// Text-to-INTEGER conversion matching the reference's numeric grammar.
fn parse_real(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0usize;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut int_digits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0usize;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            frac_digits += 1;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == st {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    s.parse::<f64>().ok()
}

fn coerce(lit: &Lit, ct: ColType, tname: &str, cname: &str) -> R<Value> {
    match lit {
        Lit::Null => Ok(Value::Null),
        Lit::Int(i) => Ok(if ct == ColType::Int {
            Value::Int(*i)
        } else {
            Value::Text(i.to_string())
        }),
        Lit::Str(s) => {
            if ct == ColType::Text {
                return Ok(Value::Text(s.clone()));
            }
            let t = s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r');
            if let Ok(i) = t.parse::<i64>() {
                return Ok(Value::Int(i));
            }
            if let Some(v) = parse_real(t) {
                if v.fract() == 0.0
                    && v >= -9223372036854775808.0f64
                    && v < 9223372036854775808.0f64
                {
                    return Ok(Value::Int(v as i64));
                }
                return Err(SqErr::typ(format!(
                    "cannot store REAL value in INTEGER column {}.{}",
                    tname, cname
                )));
            }
            Err(SqErr::typ(format!(
                "cannot store TEXT value in INTEGER column {}.{}",
                tname, cname
            )))
        }
        Lit::Big { raw, neg } => {
            if ct == ColType::Int {
                Err(SqErr::typ(format!(
                    "cannot store REAL value in INTEGER column {}.{}",
                    tname, cname
                )))
            } else {
                let v: f64 = raw.parse().unwrap_or(0.0);
                let v = if *neg { -v } else { v };
                Ok(Value::Text(format!("{}", v)))
            }
        }
    }
}

/// Coerce an evaluated expression value into a column's declared type,
/// using exactly the rules INSERT applies to literals (UPDATE's right-hand
/// sides land in the same columns).
fn coerce_val(v: &Value, ct: ColType, tname: &str, cname: &str) -> R<Value> {
    let lit = match v {
        Value::Int(i) => Lit::Int(*i),
        Value::Text(s) => Lit::Str(s.clone()),
        Value::Null => Lit::Null,
    };
    coerce(&lit, ct, tname, cname)
}

// ---------------------------------------------------------------------------
// Durable storage: framed, checksummed WAL records (crash safety)
//
// Every acknowledged statement becomes one record:
//   [magic 4][kind 1][pad 3][seq 8][len 4][crc payload 4][crc header 4][pad 4]
//   [payload ..][zero padding ..]          (total a multiple of 1024 bytes)
//
// The record is fsynced before the answer is produced, so SIGKILL right
// after an ack can never lose it; a kill while writing leaves a short or
// invalid trailing record, which recovery treats as absent. Statement
// records are padded to >= 1024 bytes so a tail cut of a few hundred bytes
// can only ever damage the most recent record, never older ones. A COMMIT
// writes one record holding every statement of the transaction, so the
// transaction is recovered all or never. Snapshot records hold the whole
// state: recovery applies the newest snapshot plus the statements written
// after it (bounded by the snapshot cadence), and startup re-compacts the
// file into two identical snapshot copies, so even a damaged tail always
// leaves a complete copy of the store behind it.
// ---------------------------------------------------------------------------

const REC_MAGIC: [u8; 4] = [0xFF, b'S', b'D', b'B'];
const REC_ALIGN: usize = 1024;
const REC_HDR: usize = 32;
const KIND_STMT: u8 = 0;
const KIND_SNAP: u8 = 1;
/// Take a snapshot record into the log after this many statements ...
const SNAP_STMTS: u64 = 16_384;
/// ... or after this many bytes of records, so recovery replays little.
const SNAP_BYTES: u64 = 64 * 1024 * 1024;

/// CRC-32 (IEEE 802.3), table generated once: validates records on recovery.
fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            t[i as usize] = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// Size of a record carrying `payload` bytes, padded to the record
/// alignment (always >= one full block).
fn rec_total(payload: usize) -> usize {
    let n = REC_HDR + payload;
    (n + REC_ALIGN - 1) / REC_ALIGN * REC_ALIGN
}

fn read_exact_at(f: &File, buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    let mut i = 0usize;
    while i < buf.len() {
        let n = f.read_at(&mut buf[i..], off)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short read",
            ));
        }
        i += n;
        off += n as u64;
    }
    Ok(())
}

/// The write-ahead log: one framed record at a time, fsynced on append.
struct Wal {
    file: File,
    seq: u64,
    stmts_since_snap: u64,
    bytes_since_snap: u64,
}

impl Wal {
    /// Append one record and make it durable before returning.
    fn append(&mut self, kind: u8, payload: &[u8]) -> std::io::Result<()> {
        let total = rec_total(payload.len());
        let mut buf = vec![0u8; total];
        buf[0..4].copy_from_slice(&REC_MAGIC);
        buf[4] = kind;
        self.seq += 1;
        buf[8..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        buf[20..24].copy_from_slice(&crc32(payload).to_le_bytes());
        let hcrc = crc32(&buf[0..24]);
        buf[24..28].copy_from_slice(&hcrc.to_le_bytes());
        buf[REC_HDR..REC_HDR + payload.len()].copy_from_slice(payload);
        self.file.write_all(&buf)?;
        self.file.sync_data()?;
        if kind == KIND_STMT {
            self.stmts_since_snap += 1;
            self.bytes_since_snap += total as u64;
        }
        Ok(())
    }

    fn snapshot_due(&self) -> bool {
        self.stmts_since_snap >= SNAP_STMTS || self.bytes_since_snap >= SNAP_BYTES
    }

    fn reset_snapshot(&mut self, blob: u64) {
        self.stmts_since_snap = 0;
        self.bytes_since_snap = blob;
    }
}

/// Durably record one acknowledged statement: the fsync inside `append`
/// happens before the answer is produced, so nothing acknowledged can be
/// lost to a kill -9. Occasionally the full state follows as a snapshot
/// record, keeping recovery (newest snapshot + later statements) fast.
fn append_log(db: &mut Db, stmt: &str) -> R<()> {
    {
        let w = db
            .wal
            .as_mut()
            .ok_or_else(|| SqErr::new("io", "log unavailable".to_string()))?;
        w.append(KIND_STMT, stmt.as_bytes())
            .map_err(|e| SqErr::new("io", format!("log write failed: {}", e)))?;
    }
    let due = db.wal.as_ref().map(|w| w.snapshot_due()).unwrap_or(false);
    if due {
        let blob = serialize_tables(&db.tables);
        let w = db.wal.as_mut().expect("wal present");
        w.append(KIND_SNAP, &blob)
            .map_err(|e| SqErr::new("io", format!("log write failed: {}", e)))?;
        w.reset_snapshot(blob.len() as u64);
    }
    Ok(())
}

// --- snapshot payload codec ------------------------------------------------

fn put_blob(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

/// Serialize every table (sorted by name for a deterministic file) into one
/// snapshot payload: the legacy column/row stream, then a constraint trailer
/// (`SDB9` + per-table metadata). Stores written before ticket 9 end after
/// the row stream and deserialize with no constraints.
fn serialize_tables(tables: &HashMap<String, Table>) -> Vec<u8> {
    let mut names: Vec<&String> = tables.keys().collect();
    names.sort_unstable();
    let mut out = Vec::new();
    out.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for &name in &names {
        let t = &tables[name];
        put_blob(&mut out, name.as_bytes());
        out.extend_from_slice(&(t.cols.len() as u32).to_le_bytes());
        for (cn, ct) in &t.cols {
            out.push(match ct {
                ColType::Int => 0u8,
                ColType::Text => 1u8,
            });
            put_blob(&mut out, cn.as_bytes());
        }
        out.extend_from_slice(&(t.rows.len() as u64).to_le_bytes());
        for row in &t.rows {
            for v in row {
                match v {
                    Value::Null => out.push(0),
                    Value::Int(i) => {
                        out.push(1);
                        out.extend_from_slice(&i.to_le_bytes());
                    }
                    Value::Text(s) => {
                        out.push(2);
                        put_blob(&mut out, s.as_bytes());
                    }
                }
            }
        }
    }
    // --- index trailer (ticket 11): definitions only; entries and tuple
    // counts are rebuilt from the rows on load. Empty when no index
    // exists, so index-free stores keep their exact old shape. Built
    // before the per-table loop below consumes `names`.
    let mut idx_blob: Vec<u8> = Vec::new();
    if names.iter().any(|n| !tables[*n].indexes.is_empty()) {
        idx_blob.extend_from_slice(b"SDB11");
        let total: u32 = names.iter().map(|n| tables[*n].indexes.len() as u32).sum();
        idx_blob.extend_from_slice(&total.to_le_bytes());
        for name in &names {
            for ix in &tables[*name].indexes {
                put_blob(&mut idx_blob, name.as_bytes());
                put_blob(&mut idx_blob, ix.name.as_bytes());
                idx_blob.push(if ix.unique { 1u8 } else { 0u8 });
                idx_blob.extend_from_slice(&(ix.cols.len() as u32).to_le_bytes());
                for &c in &ix.cols {
                    idx_blob.extend_from_slice(&(c as u32).to_le_bytes());
                }
            }
        }
    }
    // --- constraint trailer ------------------------------------------------
    out.extend_from_slice(b"SDB9");
    out.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for name in names {
        let t = &tables[name];
        put_blob(&mut out, name.as_bytes());
        out.extend_from_slice(&(t.cols.len() as u32).to_le_bytes());
        match &t.pk {
            None => out.push(0u8),
            Some(p) => {
                out.push(p.len() as u8);
                for &i in p {
                    out.extend_from_slice(&(i as u32).to_le_bytes());
                }
            }
        }
        out.push(if t.autoinc { 1u8 } else { 0u8 });
        out.extend_from_slice(&t.seq.to_le_bytes());
        for m in &t.meta {
            let mut flags = 0u8;
            if m.not_null {
                flags |= 1;
            }
            if m.unique {
                flags |= 2;
            }
            out.push(flags);
            match &m.default {
                None => out.push(0u8),
                Some(l) => {
                    out.push(1u8);
                    match l {
                        Lit::Null => out.push(0u8),
                        Lit::Int(i) => {
                            out.push(1u8);
                            out.extend_from_slice(&i.to_le_bytes());
                        }
                        Lit::Str(s) => {
                            out.push(2u8);
                            put_blob(&mut out, s.as_bytes());
                        }
                        Lit::Big { raw, neg } => {
                            out.push(3u8);
                            out.push(if *neg { 1u8 } else { 0u8 });
                            put_blob(&mut out, raw.as_bytes());
                        }
                    }
                }
            }
        }
        let decl_multi: Vec<&Vec<usize>> = // declared composite UNIQUEs only
        {
            let mut v: Vec<&Vec<usize>> = Vec::new();
            for (k, _) in &t.multi_uq {
                if t.pk.as_ref().map(|p| p == k).unwrap_or(false) {
                    continue;
                }
                // A UNIQUE index over several columns owns its tuple map;
                // it is re-attached from the index defs on load.
                if t
                    .indexes
                    .iter()
                    .any(|ix| ix.unique && ix.cols.len() > 1 && &ix.cols == k)
                {
                    continue;
                }
                v.push(k);
            }
            v
        };
        out.extend_from_slice(&(decl_multi.len() as u32).to_le_bytes());
        for k in decl_multi {
            out.extend_from_slice(&(k.len() as u32).to_le_bytes());
            for &i in k {
                out.extend_from_slice(&(i as u32).to_le_bytes());
            }
        }
        out.extend_from_slice(&(t.fks.len() as u32).to_le_bytes());
        for fk in &t.fks {
            out.extend_from_slice(&(fk.cols.len() as u32).to_le_bytes());
            for &i in &fk.cols {
                out.extend_from_slice(&(i as u32).to_le_bytes());
            }
            put_blob(&mut out, fk.ptab.as_bytes());
            out.extend_from_slice(&(fk.pcols.len() as u32).to_le_bytes());
            for c in &fk.pcols {
                put_blob(&mut out, c.as_bytes());
            }
        }
    }
    out.extend_from_slice(&idx_blob);
    out
}

/// Bounds-checked cursor over a snapshot payload.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Cur<'a> {
        Cur { b, i: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }

    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn zstr(&mut self) -> Option<String> {
        let b = self.blob()?;
        std::str::from_utf8(b).ok().map(|s| s.to_string())
    }

    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

/// Rebuild all tables from a snapshot payload; any structural problem is an
/// error (the payload only ever gets here after its checksum matched).
/// Payloads from before ticket 9 end after the row stream and yield tables
/// with no constraints; newer ones carry an `SDB9` trailer with them.
fn deserialize_tables(b: &[u8]) -> Result<HashMap<String, Table>, String> {
    let bad = |m: &str| -> Result<HashMap<String, Table>, String> { Err(m.to_string()) };
    let mut c = Cur::new(b);
    let ntab = match c.u32() {
        Some(n) => n as usize,
        None => return bad("truncated snapshot"),
    };
    let mut parsed: Vec<(String, Vec<(String, ColType)>, Vec<Vec<Value>>)> = Vec::new();
    for _ in 0..ntab {
        let name = match c.zstr() {
            Some(s) => s,
            None => return bad("truncated snapshot"),
        };
        let ncol = match c.u32() {
            Some(n) => n as usize,
            None => return bad("truncated snapshot"),
        };
        if ncol == 0 || ncol > 4096 {
            return bad("bad column count");
        }
        let mut cols: Vec<(String, ColType)> = Vec::with_capacity(ncol);
        for _ in 0..ncol {
            let ct = match c.u8() {
                Some(0) => ColType::Int,
                Some(1) => ColType::Text,
                _ => return bad("bad column type"),
            };
            let cn = match c.zstr() {
                Some(s) => s,
                None => return bad("truncated snapshot"),
            };
            cols.push((cn, ct));
        }
        let nrow = match c.u64() {
            Some(n) => n as usize,
            None => return bad("truncated snapshot"),
        };
        if (nrow as u64) > (b.len() - c.i) as u64 {
            return bad("bad row count");
        }
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(nrow);
        for _ in 0..nrow {
            let mut row = Vec::with_capacity(ncol);
            for _ in 0..ncol {
                let v = match c.u8() {
                    Some(0) => Value::Null,
                    Some(1) => match c.u64() {
                        Some(x) => Value::Int(x as i64),
                        None => return bad("truncated snapshot"),
                    },
                    Some(2) => match c.zstr() {
                        Some(s) => Value::Text(s),
                        None => return bad("truncated snapshot"),
                    },
                    _ => return bad("bad value tag"),
                };
                row.push(v);
            }
            rows.push(row);
        }
        parsed.push((name, cols, rows));
    }

    // --- constraint trailer (optional; absent in pre-ticket-9 stores) -----
    let mut defs: HashMap<String, TableDef> = HashMap::new();
    let mut seqs: HashMap<String, i64> = HashMap::new();
    // Index defs (ticket 11), keyed by table name; entries rebuild on load.
    let mut idxdefs: HashMap<String, Vec<(String, bool, Vec<usize>)>> = HashMap::new();
    if !c.done() {
        match c.take(4) {
            Some(m) if m == b"SDB9" => {}
            _ => return bad("bad trailer magic"),
        }
        let n = match c.u32() {
            Some(n) => n as usize,
            None => return bad("truncated trailer"),
        };
        if n != ntab {
            return bad("trailer table count");
        }
        for _ in 0..ntab {
            let name = match c.zstr() {
                Some(s) => s,
                None => return bad("truncated trailer"),
            };
            let ncol_t = match c.u32() {
                Some(x) => x as usize,
                None => return bad("truncated trailer"),
            };
            let pk = match c.u8() {
                Some(0) => None,
                Some(k) => {
                    let mut p = Vec::with_capacity(k as usize);
                    for _ in 0..k {
                        match c.u32() {
                            Some(i) => p.push(i as usize),
                            None => return bad("truncated trailer"),
                        }
                    }
                    Some(p)
                }
                None => return bad("truncated trailer"),
            };
            let autoinc = match c.u8() {
                Some(0) => false,
                Some(_) => true,
                None => return bad("truncated trailer"),
            };
            let seq = match c.take(8) {
                Some(s) => i64::from_le_bytes(s.try_into().unwrap()),
                None => return bad("truncated trailer"),
            };
            let base = parsed.iter().position(|(tn, _, _)| *tn == name);
            let base = match base {
                Some(i) => i,
                None => return bad("trailer table missing"),
            };
            if parsed[base].1.len() != ncol_t {
                return bad("trailer column count");
            }
            let mut meta: Vec<ColMeta> = Vec::with_capacity(ncol_t);
            for _ in 0..ncol_t {
                let flags = match c.u8() {
                    Some(f) => f,
                    None => return bad("truncated trailer"),
                };
                let has_default = match c.u8() {
                    Some(x) => x,
                    None => return bad("truncated trailer"),
                };
                let default = if has_default == 0 {
                    None
                } else {
                    let lit = match c.u8() {
                        Some(0) => Lit::Null,
                        Some(1) => match c.take(8) {
                            Some(s) => Lit::Int(i64::from_le_bytes(s.try_into().unwrap())),
                            None => return bad("truncated trailer"),
                        },
                        Some(2) => match c.zstr() {
                            Some(s) => Lit::Str(s),
                            None => return bad("truncated trailer"),
                        },
                        Some(3) => {
                            let neg = match c.u8() {
                                Some(x) => x != 0,
                                None => return bad("truncated trailer"),
                            };
                            match c.zstr() {
                                Some(raw) => Lit::Big { raw, neg },
                                None => return bad("truncated trailer"),
                            }
                        }
                        _ => return bad("bad default tag"),
                    };
                    Some(lit)
                };
                meta.push(ColMeta {
                    not_null: flags & 1 != 0,
                    unique: flags & 2 != 0,
                    default,
                });
            }
            let nmulti = match c.u32() {
                Some(x) => x as usize,
                None => return bad("truncated trailer"),
            };
            let mut multi_uq = Vec::with_capacity(nmulti);
            for _ in 0..nmulti {
                let k = match c.u32() {
                    Some(x) => x as usize,
                    None => return bad("truncated trailer"),
                };
                let mut key = Vec::with_capacity(k);
                for _ in 0..k {
                    match c.u32() {
                        Some(i) => key.push(i as usize),
                        None => return bad("truncated trailer"),
                    }
                }
                multi_uq.push(key);
            }
            let nfk = match c.u32() {
                Some(x) => x as usize,
                None => return bad("truncated trailer"),
            };
            let mut fks = Vec::with_capacity(nfk);
            for _ in 0..nfk {
                let k = match c.u32() {
                    Some(x) => x as usize,
                    None => return bad("truncated trailer"),
                };
                let mut fcols = Vec::with_capacity(k);
                for _ in 0..k {
                    match c.u32() {
                        Some(i) => fcols.push(i as usize),
                        None => return bad("truncated trailer"),
                    }
                }
                let ptab = match c.zstr() {
                    Some(s) => s,
                    None => return bad("truncated trailer"),
                };
                let np = match c.u32() {
                    Some(x) => x as usize,
                    None => return bad("truncated trailer"),
                };
                let mut pcols = Vec::with_capacity(np);
                for _ in 0..np {
                    match c.zstr() {
                        Some(s) => pcols.push(s),
                        None => return bad("truncated trailer"),
                    }
                }
                fks.push(FkDef {
                    cols: fcols,
                    ptab,
                    pcols,
                });
            }
            defs.insert(
                name.clone(),
                TableDef {
                    cols: parsed[base].1.clone(),
                    meta,
                    pk,
                    autoinc,
                    multi_uq,
                    fks,
                },
            );
            seqs.insert(name, seq);
        }
        // --- index trailer (ticket 11); absent when no index exists -----
        if !c.done() {
            match c.take(4) {
                Some(m) if m == b"SDB11" => {}
                _ => return bad("bad trailer magic"),
            }
            let total = match c.u32() {
                Some(n) => n as usize,
                None => return bad("truncated trailer"),
            };
            for _ in 0..total {
                let tname = match c.zstr() {
                    Some(s) => s,
                    None => return bad("truncated trailer"),
                };
                let iname = match c.zstr() {
                    Some(s) => s,
                    None => return bad("truncated trailer"),
                };
                let unique = match c.u8() {
                    Some(x) => x != 0,
                    None => return bad("truncated trailer"),
                };
                let nc = match c.u32() {
                    Some(x) => x as usize,
                    None => return bad("truncated trailer"),
                };
                if nc == 0 || nc > 4096 {
                    return bad("bad index column count");
                }
                let mut cols = Vec::with_capacity(nc);
                for _ in 0..nc {
                    match c.u32() {
                        Some(i) => cols.push(i as usize),
                        None => return bad("truncated trailer"),
                    }
                }
                idxdefs.entry(tname).or_default().push((iname, unique, cols));
            }
        }
        if !c.done() {
            return bad("trailing bytes");
        }
    }

    let mut out: HashMap<String, Table> = HashMap::new();
    for (name, cols, rows) in parsed {
        let ncol = cols.len();
        let def = defs.remove(&name).unwrap_or_else(|| TableDef {
            cols: cols.clone(),
            meta: vec![ColMeta::default(); ncol],
            pk: None,
            autoinc: false,
            multi_uq: Vec::new(),
            fks: Vec::new(),
        });
        let mut t = Table::from_def(def);
        t.seq = seqs.get(&name).copied().unwrap_or(0);
        t.rows = rows;
        if let Some(list) = idxdefs.remove(&name) {
            for (iname, unique, cols) in list {
                // Column positions are validated against the loaded layout;
                // a snapshot written by this server always agrees.
                if cols.iter().any(|&c| c >= ncol) {
                    continue;
                }
                t.indexes.push(IndexDef {
                    name: iname,
                    cols,
                    unique,
                    tcount: HashMap::new(),
                    entries: Vec::new(),
                });
            }
        }
        t.rebuild_after_load();
        out.insert(name, t);
    }
    Ok(out)
}

/// Resolve the ORDER BY tail of UPDATE/DELETE: an ordinal addresses the
/// rowid (only `1` exists in the reference), a word is a table column.
fn resolve_dml_order(
    order: &[OrdSpec],
    t: &Table,
    cx: &Ecx,
) -> R<Vec<(OrdKey, bool, Option<bool>)>> {
    let mut terms = Vec::with_capacity(order.len());
    for spec in order {
        let key = match &spec.kind {
            OrdKind::Ordinal(l) => match l {
                Lit::Int(1) => OrdKey::RowIdx,
                _ => {
                    return Err(SqErr::syn(
                        "1st ORDER BY term out of range - should be between 1 and 1".to_string(),
                    ))
                }
            },
            OrdKind::Word(w) => match t.idx.get(w) {
                Some(&j) => OrdKey::TableCol(j),
                None => return Err(SqErr::no_column(w)),
            },
            OrdKind::Const(l) => OrdKey::Const(lit_value(l)),
            // DML ORDER BY takes full expressions (`a.k`, `k + 1`, ...):
            // resolve against the table, then sort by the value.
            OrdKind::Expr(e) => {
                resolve_expr(e, cx, false)?;
                match e {
                    Expr::Col(w) => match t.idx.get(w) {
                        Some(&j) => OrdKey::TableCol(j),
                        None => return Err(SqErr::no_column(w)),
                    },
                    Expr::Lit(l) => OrdKey::Const(lit_value(l)),
                    other => OrdKey::Expr(other.clone()),
                }
            }
        };
        terms.push((key, spec.asc, spec.nulls_first));
    }
    Ok(terms)
}

/// Sort matched row indices by the resolved terms (stable: equal keys keep
/// scan order, like the reference).
fn sort_dml(
    matched: &mut Vec<usize>,
    terms: &[(OrdKey, bool, Option<bool>)],
    t: &Table,
    cx: &Ecx,
) -> R<()> {
    if terms.is_empty() {
        return Ok(());
    }
    // Expression keys are evaluated once per candidate row; an evaluation
    // error aborts before any write.
    let mut expr_vals: Vec<Option<HashMap<usize, Value>>> = Vec::with_capacity(terms.len());
    for (key, _, _) in terms {
        match key {
            OrdKey::Expr(e) => {
                let mut m: HashMap<usize, Value> = HashMap::new();
                for &r in matched.iter() {
                    m.insert(r, eval(e, cx, r)?);
                }
                expr_vals.push(Some(m));
            }
            _ => expr_vals.push(None),
        }
    }
    matched.sort_by(|&a, &b| {
        let ia = Value::Int(a as i64);
        let ib = Value::Int(b as i64);
        for (ti, (key, asc, nf)) in terms.iter().enumerate() {
            let (va, vb): (&Value, &Value) = match key {
                OrdKey::TableCol(j) => (&t.rows[a][*j], &t.rows[b][*j]),
                OrdKey::Const(v) => (v, v),
                OrdKey::Expr(_) => {
                    let m = expr_vals[ti].as_ref().expect("expression key cache");
                    (&m[&a], &m[&b])
                }
                // An ordinal addresses the rowid: its value on a rowid
                // table, the insertion index otherwise (same order).
                OrdKey::RowIdx => match t.rowid {
                    Some(rid) => (&t.rows[a][rid], &t.rows[b][rid]),
                    None => (&ia, &ib),
                },
                // SELECT-side ordinal keys never reach the DML sorter.
                OrdKey::Item(_) => (&ia, &ib),
            };
            let c = cmp_term(va, vb, *asc, *nf);
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    });
    Ok(())
}

/// Evaluate the `[LIMIT n] [OFFSET n]` values before any row is examined
/// (the reference reports their errors even when no row matches).
fn dml_lim_off(limit: &LimitClause, cx: &Ecx) -> R<(i64, i64)> {
    let mut lim: i64 = -1;
    let mut off: i64 = 0;
    if let Some(v) = &limit.limit {
        lim = lim_value(v, cx)?;
    }
    if let Some(v) = &limit.offset {
        off = lim_value(v, cx)?;
    }
    Ok((lim, off))
}

/// The window of matched rows an UPDATE/DELETE actually affects.
fn dml_window(total: usize, lim: i64, off: i64) -> (usize, usize) {
    let s = (off.max(0) as usize).min(total);
    let e = if lim < 0 {
        total
    } else {
        s.saturating_add(lim as usize).min(total)
    };
    (s, e)
}

/// The reference's structural foreign-key error (`syntax` class).
fn fk_mismatch(child: &str, parent: &str) -> SqErr {
    SqErr::syn(format!(
        "foreign key mismatch - \"{}\" referencing \"{}\"",
        child, parent
    ))
}

/// Resolve an FK's parent key columns in the parent table and verify they
/// form an exact unique key — the check the reference runs at statement
/// preparation for any write that involves the foreign key.
fn fk_parent_key(db: &Db, child: &str, fk: &FkDef) -> R<Vec<usize>> {
    let p = db
        .tables
        .get(&fk.ptab)
        .ok_or_else(|| SqErr::no_table(&fk.ptab))?;
    let pidxs = if fk.pcols.is_empty() {
        match &p.pk {
            Some(pk) => pk.clone(),
            None => return Err(fk_mismatch(child, &fk.ptab)),
        }
    } else {
        let mut v = Vec::with_capacity(fk.pcols.len());
        for n in &fk.pcols {
            match p.idx.get(n) {
                Some(&i) => v.push(i),
                None => return Err(fk_mismatch(child, &fk.ptab)),
            }
        }
        v
    };
    if !p.exact_unique(&pidxs) {
        return Err(fk_mismatch(child, &fk.ptab));
    }
    Ok(pidxs)
}

/// Structural validation of the foreign keys `tname` declares (every INSERT,
/// and UPDATE/DELETE when the statement's columns are foreign-key relevant).
fn fk_check_own(db: &Db, tname: &str) -> R<()> {
    let t = match db.tables.get(tname) {
        Some(t) => t,
        None => return Ok(()),
    };
    for fk in &t.fks {
        fk_parent_key(db, tname, fk)?;
    }
    Ok(())
}

/// Structural validation of every foreign key referencing `tname` (every
/// DELETE and gated UPDATE; never for INSERT — inserts cannot orphan).
fn fk_check_incoming(db: &Db, tname: &str) -> R<()> {
    let t = match db.tables.get(tname) {
        Some(t) => t,
        None => return Ok(()),
    };
    for (cn, ct) in &db.tables {
        for fk in &ct.fks {
            if fk.ptab != tname {
                continue;
            }
            let key = if fk.pcols.is_empty() {
                match &t.pk {
                    Some(pk) => pk.clone(),
                    None => return Err(fk_mismatch(cn, tname)),
                }
            } else {
                let mut v = Vec::with_capacity(fk.pcols.len());
                for n in &fk.pcols {
                    match t.idx.get(n) {
                        Some(&i) => v.push(i),
                        None => return Err(fk_mismatch(cn, tname)),
                    }
                }
                v
            };
            if !t.exact_unique(&key) {
                return Err(fk_mismatch(cn, tname));
            }
        }
    }
    Ok(())
}

/// Does an UPDATE's SET list touch a foreign-key-relevant column (one this
/// table's own foreign keys read, or one another table's foreign keys
/// reference)? The reference runs its structural checks only then — and for
/// both directions at once.
fn update_fk_gate(db: &Db, tname: &str, set_idxs: &[usize]) -> bool {
    let t = match db.tables.get(tname) {
        Some(t) => t,
        None => return false,
    };
    for fk in &t.fks {
        if fk.cols.iter().any(|c| set_idxs.contains(c)) {
            return true;
        }
    }
    for ot in db.tables.values() {
        for fk in &ot.fks {
            if fk.ptab != tname {
                continue;
            }
            if fk.pcols.is_empty() {
                match &t.pk {
                    Some(pk) => {
                        if pk.iter().any(|c| set_idxs.contains(c)) {
                            return true;
                        }
                    }
                    // Unresolvable parent key: the structural check fires.
                    None => return true,
                }
            } else {
                for n in &fk.pcols {
                    match t.idx.get(n) {
                        Some(i) => {
                            if set_idxs.contains(i) {
                                return true;
                            }
                        }
                        None => return true,
                    }
                }
            }
        }
    }
    false
}

fn err_notnull(t: &str, c: &str) -> SqErr {
    SqErr::new(
        "constraint",
        format!("NOT NULL constraint failed: {}.{}", t, c),
    )
}

fn err_unique(t: &str, c: &str) -> SqErr {
    SqErr::new("constraint", format!("UNIQUE constraint failed: {}.{}", t, c))
}

fn err_unique_cols(t: &str, cs: &[String]) -> SqErr {
    let list: Vec<String> = cs.iter().map(|c| format!("{}.{}", t, c)).collect();
    SqErr::new(
        "constraint",
        format!("UNIQUE constraint failed: {}", list.join(", ")),
    )
}

fn err_fk() -> SqErr {
    SqErr::new("constraint", "FOREIGN KEY constraint failed".to_string())
}

/// The non-NULL tuple of `cols` in `row`, or None when any part is NULL
/// (foreign keys and composite uniques ignore NULL components).
fn key_tuple(cols: &[usize], row: &[Value]) -> Option<Vec<Value>> {
    let mut k = Vec::with_capacity(cols.len());
    for &c in cols {
        match row.get(c)? {
            Value::Null => return None,
            v => k.push(v.clone()),
        }
    }
    Some(k)
}

/// The next auto-assigned identifier: `max + 1` (`1` on an empty table);
/// at i64 exhaustion, the first free value upward.
fn bump_id(cur: Option<i64>, occupied: &dyn Fn(i64) -> bool) -> Option<i64> {
    match cur {
        None => Some(1),
        Some(m) if m != i64::MAX => Some(m + 1),
        Some(_) => {
            let mut c = 1i64;
            loop {
                if !occupied(c) {
                    return Some(c);
                }
                if c == i64::MAX {
                    return None;
                }
                c += 1;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FkPhase {
    Ins,
    Upd,
    Del,
}

/// End-of-statement foreign-key value checks (the reference's NO ACTION
/// semantics validate the final state, after every row's coercion and
/// unique/not-null checks have passed):
/// - a table's own foreign keys: new/changed child rows must find their
///   parent (for a self-reference every final row is checked);
/// - other tables' foreign keys into this one: an old parent key value that
///   the statement removed must no longer be referenced.
fn fk_final(db: &Db, tname: &str, changed: &HashMap<usize, Vec<Value>>, phase: FkPhase) -> R<()> {
    let t = match db.tables.get(tname) {
        Some(t) => t,
        None => return Ok(()),
    };
    let mut incoming: Vec<(String, usize, &FkDef)> = Vec::new();
    for (cn, ct) in &db.tables {
        for (fi, fk) in ct.fks.iter().enumerate() {
            if fk.ptab == tname {
                incoming.push((cn.clone(), fi, fk));
            }
        }
    }
    if t.fks.is_empty() && incoming.is_empty() {
        return Ok(());
    }
    let final_row = |i: usize| -> Option<&Vec<Value>> {
        match phase {
            FkPhase::Del => {
                if changed.contains_key(&i) {
                    None
                } else {
                    Some(&t.rows[i])
                }
            }
            _ => match changed.get(&i) {
                Some(f) => Some(f),
                None => Some(&t.rows[i]),
            },
        }
    };
    // Every row index of the final state: the surviving rows plus rows the
    // statement inserted (which live past `t.rows.len()`).
    let mut idxs: Vec<usize> = Vec::with_capacity(t.rows.len() + changed.len());
    for i in 0..t.rows.len() {
        if phase == FkPhase::Del && changed.contains_key(&i) {
            continue;
        }
        idxs.push(i);
    }
    for &k in changed.keys() {
        if k >= t.rows.len() {
            idxs.push(k);
        }
    }

    // (a) Own foreign keys: child-side values must exist in the final
    // state. Self-references are checked over every final row.
    for fk in &t.fks {
        if fk.ptab != tname {
            continue;
        }
        let pidxs = fk_parent_key(db, tname, fk)?;
        let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
        for &i in &idxs {
            if let Some(row) = final_row(i) {
                if let Some(k) = key_tuple(&pidxs, row) {
                    *counts.entry(k).or_insert(0) += 1;
                }
            }
        }
        for &i in &idxs {
            if let Some(row) = final_row(i) {
                if let Some(k) = key_tuple(&fk.cols, row) {
                    if !counts.contains_key(&k) {
                        return Err(err_fk());
                    }
                }
            }
        }
    }

    // (b) Child-side checks for rows this statement inserted or updated
    // (non-self parents are static, so only changed rows can go stale).
    if phase != FkPhase::Del {
        for fk in &t.fks {
            if fk.ptab == tname {
                continue;
            }
            let pidxs = fk_parent_key(db, tname, fk)?;
            let p = db.tables.get(&fk.ptab).ok_or_else(|| SqErr::no_table(&fk.ptab))?;
            for row in changed.values() {
                if let Some(k) = key_tuple(&fk.cols, row) {
                    let cnt = if pidxs.len() == 1 {
                        p.single_uq
                            .iter()
                            .find(|(i, _)| *i == pidxs[0])
                            .and_then(|(_, m)| m.get(&k[0]).copied())
                            .unwrap_or(0)
                    } else {
                        p.multi_uq
                            .iter()
                            .find(|(kc, _)| *kc == pidxs)
                            .and_then(|(_, m)| m.get(&k).copied())
                            .unwrap_or(0)
                    };
                    if cnt == 0 {
                        return Err(err_fk());
                    }
                }
            }
        }
    }

    // (c) Parent-side: an old key value that vanished from the final state
    // while a child row still points at it. (Self-references are covered
    // by (a).)
    if phase != FkPhase::Ins {
        for (cn, fi, fk) in &incoming {
            if cn == tname {
                continue;
            }
            let child = db.tables.get(cn).ok_or_else(|| SqErr::no_table(cn))?;
            let cm = child.fkcnt.get(*fi).ok_or_else(|| SqErr::new("io", "no fk map".into()))?;
            let pidxs = fk_parent_key(db, tname, fk)?;
            // Counts of the key over the final state.
            let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
            for i in 0..t.rows.len() {
                if let Some(row) = final_row(i) {
                    if let Some(k) = key_tuple(&pidxs, row) {
                        *counts.entry(k).or_insert(0) += 1;
                    }
                }
            }
            let mut seen: HashSet<Vec<Value>> = HashSet::new();
            for r in changed.keys() {
                let old = match key_tuple(&pidxs, &t.rows[*r]) {
                    Some(k) => k,
                    None => continue,
                };
                if !seen.insert(old.clone()) {
                    continue;
                }
                if counts.get(&old).copied().unwrap_or(0) == 0
                    && cm.get(&old).copied().unwrap_or(0) > 0
                {
                    return Err(err_fk());
                }
            }
        }
    }
    Ok(())
}

/// Mark the lazily built maps stale after a write to `tname`.
fn invalidate_maps(db: &mut Db, tname: &str) {
    if let Some(t) = db.tables.get_mut(tname) {
        t.maps_ok = false;
    }
}

fn exec_dml(db: &mut Db, st: &Stmt, write_log: bool, src: &str) -> R<String> {
    match st {
        Stmt::Create { name, def } => {
            if db.tables.contains_key(name) {
                return Err(SqErr::exists(name));
            }
            db.tables.insert(name.clone(), Table::from_def(def.clone()));
            if write_log {
                append_log(db, src)?;
            }
            Ok("OK 0\n".to_string())
        }
        Stmt::Insert { table, cols, rows } => {
            let ncols = {
                let t = db
                    .tables
                    .get(table)
                    .ok_or_else(|| SqErr::no_table(table))?;
                t.cols.len()
            };
            // Resolve target columns (first occurrence wins on duplicates).
            // `DEFAULT VALUES` with no column list targets nothing at all.
            let default_values = cols.is_none() && rows.first().map(|r| r.is_empty()).unwrap_or(false);
            let mut targets: Vec<Option<usize>> = Vec::new();
            if !default_values {
                let t = db.tables.get(table).unwrap();
                match cols {
                    None => {
                        for i in 0..ncols {
                            targets.push(Some(i));
                        }
                    }
                    Some(list) => {
                        for n in list {
                            let i = t
                                .idx
                                .get(n)
                                .copied()
                                .ok_or_else(|| SqErr::no_column(n))?;
                            if targets.iter().any(|x| *x == Some(i)) {
                                targets.push(None);
                            } else {
                                targets.push(Some(i));
                            }
                        }
                    }
                }
            }
            let first = rows
                .first()
                .ok_or_else(|| SqErr::syn("incomplete input".to_string()))?;
            if first.len() != targets.len() {
                return Err(SqErr::arity(if cols.is_some() {
                    format!("{} values for {} columns", first.len(), targets.len())
                } else {
                    format!(
                        "table {} has {} columns but {} values were supplied",
                        table,
                        ncols,
                        first.len()
                    )
                }));
            }
            // Structural foreign-key checks run before any coercion.
            fk_check_own(db, table)?;
            // The lazily built maps the value checks read must be current.
            let fks_def: Vec<FkDef> = {
                let t = db.tables.get(table).unwrap();
                t.fks.clone()
            };
            let nmulti = {
                let t = db.tables.get(table).unwrap();
                t.multi_uq.len()
            };
            // UNIQUE indexes over several columns: tuple counts checked
            // alongside the declared composite constraints.
            let nuidx = {
                let t = db.tables.get(table).unwrap();
                t.indexes
                    .iter()
                    .filter(|i| i.unique && i.cols.len() > 1)
                    .count()
            };
            for fk in &fks_def {
                if fk.ptab != *table {
                    if let Some(p) = db.tables.get_mut(&fk.ptab) {
                        p.ensure_maps();
                    }
                }
            }
            if fks_def.iter().any(|fk| fk.ptab == *table) {
                if let Some(s) = db.tables.get_mut(table) {
                    s.ensure_maps();
                }
            }
            // Row-by-row in the reference's per-row phase order: the rowid
            // block, then NOT NULL, then coercion (type), then the remaining
            // unique checks (constraint) — so the first failing phase of the
            // first failing row decides the error class.
            let mut overlay: HashMap<(usize, Value), usize> = HashMap::new();
            let mut moverlay: Vec<HashMap<Vec<Value>, usize>> = vec![HashMap::new(); nmulti];
            let mut ioverlay: Vec<HashMap<Vec<Value>, usize>> = vec![HashMap::new(); nuidx];
            let mut new_rows: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
            let mut run_max: Option<i64>;
            let mut run_seq: i64;
            {
                let t = db.tables.get(table).unwrap();
                run_max = t.rowid_max;
                run_seq = t.seq;
                for rl in rows {
                    // The literal feeding each column: the first occurrence
                    // in the column list, else the column's DEFAULT (never
                    // for the rowid alias, which auto-assigns).
                    let mut raw: Vec<Option<&Lit>> = vec![None; ncols];
                    for (k, lit) in rl.iter().enumerate() {
                        if let Some(ti) = targets[k] {
                            raw[ti] = Some(lit);
                        }
                    }
                    for i in 0..ncols {
                        if raw[i].is_none() && t.rowid != Some(i) {
                            raw[i] = t.meta[i].default.as_ref();
                        }
                    }
                    let mut vals = vec![Value::Null; ncols];
                    // Phase 0: the rowid alias — coerce, auto-assign and
                    // uniqueness all run before anything else in the row.
                    if let Some(r) = t.rowid {
                        let (cname, ct) = &t.cols[r];
                        let v = match raw[r] {
                            Some(l) => coerce(l, *ct, table, cname)?,
                            None => Value::Null,
                        };
                        let v = if v == Value::Null {
                            let rid = r;
                            let occupied = |v: i64| {
                                t.rows.iter().any(|row| row[rid] == Value::Int(v))
                                    || new_rows.iter().any(|row| row[rid] == Value::Int(v))
                            };
                            let next = if t.autoinc {
                                bump_id(Some(run_seq), &occupied)
                            } else {
                                bump_id(run_max, &occupied)
                            };
                            let next = match next {
                                Some(x) => x,
                                None => return Err(SqErr::typ("datatype mismatch".to_string())),
                            };
                            if t.autoinc && next > run_seq {
                                run_seq = next;
                            }
                            run_max = Some(match run_max {
                                None => next,
                                Some(m) => m.max(next),
                            });
                            Value::Int(next)
                        } else {
                            if let Value::Int(x) = v {
                                if t.autoinc && x > run_seq {
                                    run_seq = x;
                                }
                                run_max = Some(match run_max {
                                    None => x,
                                    Some(m) => m.max(x),
                                });
                            }
                            v
                        };
                        vals[r] = v;
                        for (ci, map) in &t.single_uq {
                            if *ci != r {
                                continue;
                            }
                            let v = &vals[r];
                            let key = (r, v.clone());
                            let cnt = map.get(v).copied().unwrap_or(0)
                                + overlay.get(&key).copied().unwrap_or(0);
                            if cnt > 0 {
                                return Err(err_unique(table, &t.cols[r].0));
                            }
                            *overlay.entry(key).or_insert(0) += 1;
                        }
                    }
                    // Phase 1: NOT NULL over the raw values (coercion never
                    // changes null-ness, and the rowid alias cannot be NULL
                    // by phase 0).
                    for i in 0..ncols {
                        if t.rowid == Some(i) {
                            continue;
                        }
                        if t.meta[i].not_null && raw[i].map_or(true, |l| matches!(l, Lit::Null)) {
                            return Err(err_notnull(table, &t.cols[i].0));
                        }
                    }
                    // Phase 2: coerce the remaining columns (type errors).
                    for i in 0..ncols {
                        if t.rowid == Some(i) {
                            continue;
                        }
                        if let Some(l) = raw[i] {
                            let (cname, ct) = &t.cols[i];
                            vals[i] = coerce(l, *ct, table, cname)?;
                        }
                    }
                    // Phase 3: the remaining unique constraints.
                    for (ci, map) in &t.single_uq {
                        if t.rowid == Some(*ci) {
                            continue;
                        }
                        let v = &vals[*ci];
                        if *v == Value::Null {
                            continue;
                        }
                        let key = (*ci, v.clone());
                        let cnt = map.get(v).copied().unwrap_or(0)
                            + overlay.get(&key).copied().unwrap_or(0);
                        if cnt > 0 {
                            return Err(err_unique(table, &t.cols[*ci].0));
                        }
                        *overlay.entry(key).or_insert(0) += 1;
                    }
                    for (mi, (keycols, map)) in t.multi_uq.iter().enumerate() {
                        let key = match key_tuple(keycols, &vals) {
                            Some(k) => k,
                            None => continue,
                        };
                        let cnt = map.get(&key).copied().unwrap_or(0)
                            + moverlay[mi].get(&key).copied().unwrap_or(0);
                        if cnt > 0 {
                            let names: Vec<String> =
                                keycols.iter().map(|&c| t.cols[c].0.clone()).collect();
                            return Err(err_unique_cols(table, &names));
                        }
                        *moverlay[mi].entry(key).or_insert(0) += 1;
                    }
                    {
                        let mut oi = 0usize;
                        for ix in t.indexes.iter() {
                            if !(ix.unique && ix.cols.len() > 1) {
                                continue;
                            }
                            let key = match key_tuple(&ix.cols, &vals) {
                                Some(k) => k,
                                None => {
                                    oi += 1;
                                    continue;
                                }
                            };
                            let cnt = ix.tcount.get(&key).copied().unwrap_or(0)
                                + ioverlay[oi].get(&key).copied().unwrap_or(0);
                            if cnt > 0 {
                                let names: Vec<String> =
                                    ix.cols.iter().map(|&c| t.cols[c].0.clone()).collect();
                                return Err(err_unique_cols(table, &names));
                            }
                            *ioverlay[oi].entry(key).or_insert(0) += 1;
                            oi += 1;
                        }
                    }
                    new_rows.push(vals);
                }
            }
            // Foreign keys validate at the end of the statement: a batch may
            // create the very parents its earlier rows point at.
            {
                let changed: HashMap<usize, Vec<Value>> = {
                    let t = db.tables.get(table).unwrap();
                    let base = t.rows.len();
                    (0..new_rows.len())
                        .map(|k| (base + k, new_rows[k].clone()))
                        .collect()
                };
                fk_final(db, table, &changed, FkPhase::Ins)?;
            }
            let n = new_rows.len();
            match db.tables.get_mut(table) {
                Some(t) => {
                    if t.rowid.is_some() {
                        t.rowid_max = run_max;
                        if t.autoinc {
                            t.seq = run_seq;
                        }
                    }
                    for row in &new_rows {
                        for (ci, map) in t.single_uq.iter_mut() {
                            let v = &row[*ci];
                            if *v != Value::Null {
                                *map.entry(v.clone()).or_insert(0) += 1;
                            }
                        }
                    }
                    for row in &new_rows {
                        for (keycols, map) in t.multi_uq.iter_mut() {
                            if let Some(k) = key_tuple(keycols, row) {
                                *map.entry(k).or_insert(0) += 1;
                            }
                        }
                    }
                    // Index entries for the appended positions, the unique
                    // multi-column tuple counts, and the rowid-order flag.
                    if !t.indexes.is_empty() {
                        let base = t.rows.len();
                        let rid = t.rowid;
                        let mut prev: Option<i64> = if rid.is_some() && base > 0 {
                            let r = rid.unwrap();
                            match &t.rows[base - 1][r] {
                                Value::Int(i) => Some(*i),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        for (k, row) in new_rows.iter().enumerate() {
                            let p = base + k;
                            let key = match rid {
                                Some(r) => match &row[r] {
                                    Value::Int(i) => *i,
                                    _ => p as i64,
                                },
                                None => p as i64,
                            };
                            if t.seq_scan {
                                if let (Some(pr), true) = (prev, rid.is_some()) {
                                    if key <= pr {
                                        // Explicit rowids arrived out of
                                        // sequence: physical order no longer
                                        // matches rowid order.
                                        t.seq_scan = false;
                                    }
                                }
                            }
                            prev = Some(key);
                            for ix in t.indexes.iter_mut() {
                                let v = row[ix.cols[0]].clone();
                                let at = ix
                                    .entries
                                    .partition_point(|e| {
                                        cmp_val(&e.0, &v).then(e.1.cmp(&key))
                                            == Ordering::Less
                                    });
                                ix.entries.insert(at, (v, key, p));
                                if ix.unique && ix.cols.len() > 1 {
                                    if let Some(tk) = key_tuple(&ix.cols, row) {
                                        *ix.tcount.entry(tk).or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                    }
                    t.rows.extend(new_rows);
                    t.maps_ok = false;
                }
                None => return Err(SqErr::no_table(table)),
            }
            if write_log {
                append_log(db, src)?;
            }
            Ok(format!("OK {}\n", n))
        }
        Stmt::InsertSel { table, cols, select } => {
            // The select runs first (read-only); its rows then go through
            // the exact `VALUES` pipeline (same per-row checks, ticket 10).
            let (hdr, srows) = exec_select_data(db, select, &[])?;
            if srows.is_empty() {
                // Zero rows: same target/arity checks, then nothing inserted.
                let t = db.tables.get(table).ok_or_else(|| SqErr::no_table(table))?;
                let ncols = t.cols.len();
                let mut targets: Vec<Option<usize>> = Vec::new();
                match cols {
                    None => {
                        for i in 0..ncols {
                            targets.push(Some(i));
                        }
                    }
                    Some(list) => {
                        for n in list {
                            let i = t
                                .idx
                                .get(n)
                                .copied()
                                .ok_or_else(|| SqErr::no_column(n))?;
                            if targets.iter().any(|x| *x == Some(i)) {
                                targets.push(None);
                            } else {
                                targets.push(Some(i));
                            }
                        }
                    }
                }
                if hdr.len() != targets.len() {
                    return Err(SqErr::arity(if cols.is_some() {
                        format!("{} values for {} columns", hdr.len(), targets.len())
                    } else {
                        format!(
                            "table {} has {} columns but {} values were supplied",
                            table,
                            ncols,
                            hdr.len()
                        )
                    }));
                }
                if write_log {
                    append_log(db, src)?;
                }
                return Ok("OK 0\n".to_string());
            }
            let rows: Vec<Vec<Lit>> = srows
                .into_iter()
                .map(|row| row.into_iter().map(value_to_lit).collect())
                .collect();
            return exec_dml(
                db,
                &Stmt::Insert {
                    table: table.clone(),
                    cols: cols.clone(),
                    rows,
                },
                write_log,
                src,
            );
        }
        Stmt::Update {
            table,
            vis,
            sets,
            wexpr,
            order,
            limit,
        } => {
            // Phase 1 (read-only): resolve names in the reference's order
            // (table, then per assignment right-hand side and target, then
            // WHERE, then ORDER BY), evaluate LIMIT/OFFSET, and compute the
            // matched window. Any resolution error aborts before any
            // constraint work, so `SET a = b, b = a` still reads the old row.
            let (matched, _terms, lim, off): (Vec<usize>, Vec<(OrdKey, bool, Option<bool>)>, i64, i64) = {
                let t = db
                    .tables
                    .get(table)
                    .ok_or_else(|| SqErr::no_table(table))?;
                let cx = Ecx {
                    db: &*db,
                    frames: vec![Frame::of_table(t, &[vis.clone()])],
                };
                for (col, e) in sets {
                    resolve_expr(e, &cx, false)?;
                    if !t.idx.contains_key(col) {
                        return Err(SqErr::no_column(col));
                    }
                }
                if let Some(w) = wexpr {
                    resolve_expr(w, &cx, false)?;
                }
                let terms = resolve_dml_order(order, t, &cx)?;
                let (lim, off) = dml_lim_off(limit, &cx)?;
                // Late subquery column-count checks (like SELECT's).
                for (_, e) in sets {
                    check_subs_width(e, &cx, SubCtx::Arity)?;
                }
                if let Some(w) = wexpr {
                    check_subs_width(w, &cx, SubCtx::Arity)?;
                }
                for spec in order {
                    if let OrdKind::Expr(e) = &spec.kind {
                        check_subs_width(e, &cx, SubCtx::Arity)?;
                    }
                }
                if let Some(LimVal::Expr(e)) = &limit.limit {
                    check_subs_width(e, &cx, SubCtx::Arity)?;
                }
                if let Some(LimVal::Expr(e)) = &limit.offset {
                    check_subs_width(e, &cx, SubCtx::Arity)?;
                }
                // Rows matched by WHERE (a NULL predicate keeps nothing).
                let mut matched: Vec<usize> = Vec::with_capacity(t.rows.len());
                for r in 0..t.rows.len() {
                    if let Some(w) = wexpr {
                        if !is_true(&eval(w, &cx, r)?) {
                            continue;
                        }
                    }
                    matched.push(r);
                }
                // The reference scans a rowid table in rowid order; the
                // physical (insertion) order differs when explicit rowids
                // arrived out of sequence.
                if let Some(rid) = t.rowid {
                    matched.sort_by(|&a, &b| match (&t.rows[a][rid], &t.rows[b][rid]) {
                        (Value::Int(x), Value::Int(y)) => x.cmp(y),
                        _ => Ordering::Equal,
                    });
                }
                sort_dml(&mut matched, &terms, t, &cx)?;
                (matched, terms, lim, off)
            };
            // Structural foreign-key checks fire only when the SET list
            // touches a foreign-key-relevant column (both directions).
            let set_idxs: Vec<usize> = {
                let t = db.tables.get(table).ok_or_else(|| SqErr::no_table(table))?;
                sets.iter()
                    .map(|(c, _)| t.idx.get(c).copied().unwrap_or(0))
                    .collect()
            };
            let gate = update_fk_gate(db, table, &set_idxs);
            if gate {
                fk_check_own(db, table)?;
                fk_check_incoming(db, table)?;
                // Current lazy maps for the value checks below.
                if let Some(t) = db.tables.get_mut(table) {
                    t.ensure_maps();
                }
                let own_fks: Vec<FkDef> = db.tables.get(table).map(|t| t.fks.clone()).unwrap_or_default();
                for fk in &own_fks {
                    if fk.ptab != *table {
                        if let Some(p) = db.tables.get_mut(&fk.ptab) {
                            p.ensure_maps();
                        }
                    }
                }
                let incoming: Vec<String> = {
                    let mut v = Vec::new();
                    for (cn, ct) in &db.tables {
                        if ct.fks.iter().any(|fk| fk.ptab == *table) && cn != table {
                            v.push(cn.clone());
                        }
                    }
                    v
                };
                for cn in &incoming {
                    if let Some(c) = db.tables.get_mut(cn) {
                        c.ensure_maps();
                    }
                }
            }
            // Phase 2: row-by-row coercion (type) then not-null/unique
            // (constraint) over the final row of each affected row — the
            // incremental unique check matches the reference exactly, while
            // foreign keys validate at the end of the statement.
            let mut finals: Vec<(usize, Vec<Value>)> = Vec::new();
            {
                let t = db
                    .tables
                    .get(table)
                    .ok_or_else(|| SqErr::no_table(table))?;
                let cx = Ecx {
                    db: &*db,
                    frames: vec![Frame::of_table(t, &[vis.clone()])],
                };
                let (s, e) = dml_window(matched.len(), lim, off);
                let mut delta: HashMap<(usize, Value), i64> = HashMap::new();
                let mut mdelta: Vec<HashMap<Vec<Value>, i64>> =
                    vec![HashMap::new(); t.multi_uq.len()];
                let uidxs: Vec<usize> = t
                    .indexes
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| i.unique && i.cols.len() > 1)
                    .map(|(k, _)| k)
                    .collect();
                let mut idelta: Vec<HashMap<Vec<Value>, i64>> =
                    vec![HashMap::new(); uidxs.len()];
                for &r in &matched[s..e] {
                    // Evaluate every right-hand side against the OLD row
                    // (SET order; `SET a = b, b = a` swaps).
                    let mut evals: Vec<(usize, Value)> = Vec::with_capacity(sets.len());
                    for (col, e2) in sets {
                        let v = eval(e2, &cx, r)?;
                        let i = t.idx.get(col).copied().unwrap_or(0);
                        evals.push((i, v));
                    }
                    // Raw and coerced final rows (last assignment wins).
                    let mut fin_raw = t.rows[r].clone();
                    for (i, v) in &evals {
                        fin_raw[*i] = v.clone();
                    }
                    let mut fin = t.rows[r].clone();
                    // Phase 0: the rowid alias — NULL is a datatype
                    // mismatch, then coercion and uniqueness, all before
                    // the rest of the row (the reference's order).
                    if let Some(rid) = t.rowid {
                        if evals.iter().any(|(i, _)| *i == rid) {
                            let v = fin_raw[rid].clone();
                            let nv = if v == Value::Null {
                                return Err(SqErr::typ("datatype mismatch".to_string()));
                            } else {
                                let (cname, ct) = &t.cols[rid];
                                coerce_val(&v, *ct, table, cname)?
                            };
                            fin[rid] = nv.clone();
                            let old = &t.rows[r][rid];
                            if *old != Value::Null {
                                *delta.entry((rid, old.clone())).or_insert(0) -= 1;
                            }
                            *delta.entry((rid, nv.clone())).or_insert(0) += 1;
                            let key = (rid, nv);
                            for (ci, map) in &t.single_uq {
                                if *ci != rid {
                                    continue;
                                }
                                let cnt = map.get(&fin[rid]).copied().unwrap_or(0) as i64
                                    + delta.get(&key).copied().unwrap_or(0);
                                if cnt > 1 {
                                    return Err(err_unique(table, &t.cols[rid].0));
                                }
                            }
                        }
                    }
                    // Phase 1: NOT NULL over the raw finals (null-ness is
                    // coercion-independent; the rowid alias cannot be NULL
                    // past phase 0).
                    for i in 0..fin_raw.len() {
                        if t.rowid == Some(i) {
                            continue;
                        }
                        if t.meta[i].not_null && fin_raw[i] == Value::Null {
                            return Err(err_notnull(table, &t.cols[i].0));
                        }
                    }
                    // Phase 2: coerce the assigned columns (type errors).
                    for (i, v) in &evals {
                        if t.rowid == Some(*i) {
                            continue;
                        }
                        let (cname, ct) = &t.cols[*i];
                        fin[*i] = coerce_val(v, *ct, table, cname)?;
                    }
                    // Phase 3: the remaining unique constraints, incremental
                    // (the state after this row, like the reference).
                    for (ci, map) in &t.single_uq {
                        if t.rowid == Some(*ci) {
                            continue;
                        }
                        if !evals.iter().any(|(i, _)| *i == *ci) {
                            continue;
                        }
                        let old = &t.rows[r][*ci];
                        let new = &fin[*ci];
                        if *old != Value::Null {
                            *delta.entry((*ci, old.clone())).or_insert(0) -= 1;
                        }
                        if *new != Value::Null {
                            *delta.entry((*ci, new.clone())).or_insert(0) += 1;
                            let key = (*ci, new.clone());
                            let cnt = map.get(new).copied().unwrap_or(0) as i64
                                + delta.get(&key).copied().unwrap_or(0);
                            if cnt > 1 {
                                return Err(err_unique(table, &t.cols[*ci].0));
                            }
                        }
                    }
                    for (mi, (keycols, map)) in t.multi_uq.iter().enumerate() {
                        if !keycols
                            .iter()
                            .any(|c| evals.iter().any(|(i, _)| i == c))
                        {
                            continue;
                        }
                        let oldk = key_tuple(keycols, &t.rows[r]);
                        let newk = key_tuple(keycols, &fin);
                        if let Some(k) = &oldk {
                            *mdelta[mi].entry(k.clone()).or_insert(0) -= 1;
                        }
                        if let Some(k) = &newk {
                            *mdelta[mi].entry(k.clone()).or_insert(0) += 1;
                            let cnt = map.get(k).copied().unwrap_or(0) as i64
                                + mdelta[mi].get(k).copied().unwrap_or(0);
                            if cnt > 1 {
                                let names: Vec<String> =
                                    keycols.iter().map(|&c| t.cols[c].0.clone()).collect();
                                return Err(err_unique_cols(table, &names));
                            }
                        }
                    }
                    for (oi, &ii) in uidxs.iter().enumerate() {
                        let ix = &t.indexes[ii];
                        if !ix
                            .cols
                            .iter()
                            .any(|c| evals.iter().any(|(i, _)| i == c))
                        {
                            continue;
                        }
                        let oldk = key_tuple(&ix.cols, &t.rows[r]);
                        let newk = key_tuple(&ix.cols, &fin);
                        if let Some(k) = &oldk {
                            *idelta[oi].entry(k.clone()).or_insert(0) -= 1;
                        }
                        if let Some(k) = &newk {
                            *idelta[oi].entry(k.clone()).or_insert(0) += 1;
                            let cnt = ix.tcount.get(k).copied().unwrap_or(0) as i64
                                + idelta[oi].get(k).copied().unwrap_or(0);
                            if cnt > 1 {
                                let names: Vec<String> =
                                    ix.cols.iter().map(|&c| t.cols[c].0.clone()).collect();
                                return Err(err_unique_cols(table, &names));
                            }
                        }
                    }
                    finals.push((r, fin));
                }
            }
            // Phase 3: end-of-statement foreign-key validation.
            if gate {
                let changed: HashMap<usize, Vec<Value>> = finals.iter().cloned().collect();
                fk_final(db, table, &changed, FkPhase::Upd)?;
            }
            // Phase 4: apply the validated final rows.
            let n = finals.len();
            if n > 0 {
                let t = match db.tables.get_mut(table) {
                    Some(t) => t,
                    None => return Err(SqErr::no_table(table)),
                };
                let mut rowid_changed = false;
                if let Some(rid) = t.rowid {
                    for (r, fin) in &finals {
                        if t.rows[*r][rid] != fin[rid] {
                            rowid_changed = true;
                            break;
                        }
                    }
                }
                for (r, fin) in finals {
                    for (ci, map) in t.single_uq.iter_mut() {
                        let old = &t.rows[r][*ci];
                        let new = &fin[*ci];
                        if old != new {
                            if *old != Value::Null {
                                if let Some(c) = map.get_mut(old) {
                                    *c -= 1;
                                }
                            }
                            if *new != Value::Null {
                                *map.entry(new.clone()).or_insert(0) += 1;
                            }
                        }
                    }
                    for (keycols, map) in t.multi_uq.iter_mut() {
                        let oldk = key_tuple(keycols, &t.rows[r]);
                        let newk = key_tuple(keycols, &fin);
                        if oldk != newk {
                            if let Some(k) = &oldk {
                                if let Some(c) = map.get_mut(k) {
                                    *c -= 1;
                                }
                            }
                            if let Some(k) = &newk {
                                *map.entry(k.clone()).or_insert(0) += 1;
                            }
                        }
                    }
                    // Index maintenance: the row's position never moves,
                    // but its indexed value or rowid key may — both feed
                    // the entry order.
                    if !t.indexes.is_empty() {
                        let old_key = match t.rowid {
                            Some(rid) => match &t.rows[r][rid] {
                                Value::Int(i) => *i,
                                _ => r as i64,
                            },
                            None => r as i64,
                        };
                        let new_key = match t.rowid {
                            Some(rid) => match &fin[rid] {
                                Value::Int(i) => *i,
                                _ => r as i64,
                            },
                            None => r as i64,
                        };
                        for ix in t.indexes.iter_mut() {
                            let c0 = ix.cols[0];
                            let ov = t.rows[r][c0].clone();
                            let nv = fin[c0].clone();
                            if ov != nv || old_key != new_key {
                                let at = ix.entries.partition_point(|e| {
                                    cmp_val(&e.0, &ov).then(e.1.cmp(&old_key)) == Ordering::Less
                                });
                                if at < ix.entries.len()
                                    && cmp_val(&ix.entries[at].0, &ov) == Ordering::Equal
                                    && ix.entries[at].1 == old_key
                                {
                                    ix.entries.remove(at);
                                }
                                let ins = ix.entries.partition_point(|e| {
                                    cmp_val(&e.0, &nv).then(e.1.cmp(&new_key)) == Ordering::Less
                                });
                                ix.entries.insert(ins, (nv, new_key, r));
                            }
                            if ix.unique && ix.cols.len() > 1 {
                                let oldk = key_tuple(&ix.cols, &t.rows[r]);
                                let newk = key_tuple(&ix.cols, &fin);
                                if oldk != newk {
                                    if let Some(k) = &oldk {
                                        if let Some(c) = ix.tcount.get_mut(k) {
                                            *c -= 1;
                                        }
                                    }
                                    if let Some(k) = &newk {
                                        *ix.tcount.entry(k.clone()).or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                    }
                    t.rows[r] = fin;
                }
                if let Some(rid) = t.rowid {
                    if rowid_changed {
                        let m = t
                            .rows
                            .iter()
                            .filter_map(|row| match &row[rid] {
                                Value::Int(i) => Some(*i),
                                _ => None,
                            })
                            .max();
                        if t.autoinc {
                            if let Some(m) = m {
                                if m > t.seq {
                                    t.seq = m;
                                }
                            }
                        } else {
                            t.rowid_max = m;
                        }
                        // Physical order may now differ from rowid order.
                        t.recompute_seq_scan();
                    }
                }
                t.maps_ok = false;
            }
            if write_log {
                append_log(db, src)?;
            }
            Ok(format!("OK {}\n", n))
        }
        Stmt::Delete {
            table,
            vis,
            wexpr,
            order,
            limit,
        } => {
            // Phase 1: resolve, evaluate LIMIT/OFFSET, then evaluate the
            // predicate for every row before deleting anything; an
            // evaluation error changes nothing.
            let (keep, n, deleted): (Vec<bool>, usize, Vec<usize>) = {
                let t = db
                    .tables
                    .get(table)
                    .ok_or_else(|| SqErr::no_table(table))?;
                let cx = Ecx {
                    db: &*db,
                    frames: vec![Frame::of_table(t, &[vis.clone()])],
                };
                if let Some(w) = wexpr {
                    resolve_expr(w, &cx, false)?;
                }
                let terms = resolve_dml_order(order, t, &cx)?;
                let (lim, off) = dml_lim_off(limit, &cx)?;
                if let Some(w) = wexpr {
                    check_subs_width(w, &cx, SubCtx::Arity)?;
                }
                for spec in order {
                    if let OrdKind::Expr(e) = &spec.kind {
                        check_subs_width(e, &cx, SubCtx::Arity)?;
                    }
                }
                if let Some(LimVal::Expr(e)) = &limit.limit {
                    check_subs_width(e, &cx, SubCtx::Arity)?;
                }
                if let Some(LimVal::Expr(e)) = &limit.offset {
                    check_subs_width(e, &cx, SubCtx::Arity)?;
                }
                let mut matched: Vec<usize> = Vec::with_capacity(t.rows.len());
                for r in 0..t.rows.len() {
                    let hit = match wexpr {
                        Some(w) => is_true(&eval(w, &cx, r)?),
                        None => true,
                    };
                    if hit {
                        matched.push(r);
                    }
                }
                // Rowid order, like the reference's table scan (see UPDATE).
                if let Some(rid) = t.rowid {
                    matched.sort_by(|&a, &b| match (&t.rows[a][rid], &t.rows[b][rid]) {
                        (Value::Int(x), Value::Int(y)) => x.cmp(y),
                        _ => Ordering::Equal,
                    });
                }
                sort_dml(&mut matched, &terms, t, &cx)?;
                let (s, e) = dml_window(matched.len(), lim, off);
                let mut keep = vec![true; t.rows.len()];
                for &r in &matched[s..e] {
                    keep[r] = false;
                }
                (keep, e - s, matched[s..e].to_vec())
            };
            // Structural foreign-key checks always run for DELETE, in both
            // directions.
            fk_check_own(db, table)?;
            fk_check_incoming(db, table)?;
            if n > 0 {
                // The child-side count maps the parent-side checks read.
                let incoming: Vec<String> = {
                    let mut v = Vec::new();
                    for (cn, ct) in &db.tables {
                        if ct.fks.iter().any(|fk| fk.ptab == *table) && cn != table {
                            v.push(cn.clone());
                        }
                    }
                    v
                };
                for cn in &incoming {
                    if let Some(c) = db.tables.get_mut(cn) {
                        c.ensure_maps();
                    }
                }
                let changed: HashMap<usize, Vec<Value>> = {
                    let t = db.tables.get(table).ok_or_else(|| SqErr::no_table(table))?;
                    deleted.iter().map(|&r| (r, t.rows[r].clone())).collect()
                };
                fk_final(db, table, &changed, FkPhase::Del)?;
            }
            if n > 0 {
                let t = match db.tables.get_mut(table) {
                    Some(t) => t,
                    None => return Err(SqErr::no_table(table)),
                };
                // Release the unique counts of the disappearing rows.
                for (r, row) in t.rows.iter().enumerate() {
                    if keep[r] {
                        continue;
                    }
                    for (ci, map) in t.single_uq.iter_mut() {
                        let v = &row[*ci];
                        if *v != Value::Null {
                            if let Some(c) = map.get_mut(v) {
                                *c -= 1;
                            }
                        }
                    }
                    for (keycols, map) in t.multi_uq.iter_mut() {
                        if let Some(k) = key_tuple(keycols, row) {
                            if let Some(c) = map.get_mut(&k) {
                                *c -= 1;
                            }
                        }
                    }
                    for ix in t.indexes.iter_mut() {
                        if ix.unique && ix.cols.len() > 1 {
                            if let Some(k) = key_tuple(&ix.cols, row) {
                                if let Some(c) = ix.tcount.get_mut(&k) {
                                    *c -= 1;
                                }
                            }
                        }
                    }
                }
                // Drop the deleted rows' index entries and re-base every
                // survivor's position; the (value, rowid) sort key only
                // moves for a table without a rowid alias, where it *is*
                // the position — a monotone shift, so order is preserved.
                if !t.indexes.is_empty() {
                    let mut del: Vec<usize> = Vec::new();
                    for (i, &k) in keep.iter().enumerate() {
                        if !k {
                            del.push(i);
                        }
                    }
                    let has_rid = t.rowid.is_some();
                    for ix in t.indexes.iter_mut() {
                        let mut out: Vec<(Value, i64, usize)> =
                            Vec::with_capacity(ix.entries.len());
                        for (v, key, pos) in ix.entries.drain(..) {
                            match del.binary_search(&pos) {
                                Ok(_) => {}
                                Err(d) => {
                                    let npos = pos - d;
                                    let nkey = if has_rid { key } else { npos as i64 };
                                    out.push((v, nkey, npos));
                                }
                            }
                        }
                        ix.entries = out;
                    }
                }
                let mut i = 0usize;
                t.rows.retain(|_| {
                    let k = keep[i];
                    i += 1;
                    k
                });
                if let Some(rid) = t.rowid {
                    if !t.autoinc {
                        t.rowid_max = t
                            .rows
                            .iter()
                            .filter_map(|row| match &row[rid] {
                                Value::Int(x) => Some(*x),
                                _ => None,
                            })
                            .max();
                    }
                }
                t.maps_ok = false;
            }
            if write_log {
                append_log(db, src)?;
            }
            Ok(format!("OK {}\n", n))
        }
        // Transaction control never reaches exec_dml (process handles it);
        // recognized so recovery replaying an old log stays total.
        Stmt::TxnBegin | Stmt::TxnCommit | Stmt::TxnRollback => Ok("OK 0\n".to_string()),
        Stmt::Select { .. } => Ok(String::new()),
        Stmt::CreateIndex {
            unique,
            name,
            table,
            cols,
        } => {
            // The reference's check order: table, name-is-a-table,
            // name-is-an-index, columns, then the unique-data scan. A
            // failed creation registers nothing.
            let mut cidx: Vec<usize>;
            let mut entries: Vec<(Value, i64, usize)>;
            let mut tcount: HashMap<Vec<Value>, usize> = HashMap::new();
            {
                let t = db.tables.get(table).ok_or_else(|| SqErr::no_table(table))?;
                if db.tables.contains_key(name) {
                    return Err(SqErr::new(
                        "syntax",
                        format!("there is already a table named {}", name),
                    ));
                }
                if db.tables.values().any(|tt| tt.index_pos(name).is_some()) {
                    return Err(SqErr::new(
                        "exists",
                        format!("index {} already exists", name),
                    ));
                }
                cidx = Vec::with_capacity(cols.len());
                for c in cols {
                    cidx.push(t.idx.get(c).copied().ok_or_else(|| SqErr::no_column(c))?);
                }
                let rid = t.rowid;
                entries = Vec::with_capacity(t.rows.len());
                for (p, row) in t.rows.iter().enumerate() {
                    let key = match rid {
                        Some(r) => match &row[r] {
                            Value::Int(i) => *i,
                            _ => p as i64,
                        },
                        None => p as i64,
                    };
                    entries.push((row[cidx[0]].clone(), key, p));
                }
                entries.sort_by(|a, b| cmp_val(&a.0, &b.0).then(a.1.cmp(&b.1)));
                if *unique {
                    if cidx.len() == 1 {
                        let c0 = cidx[0];
                        let mut map: HashMap<Value, usize> = HashMap::new();
                        for row in &t.rows {
                            let v = &row[c0];
                            if *v != Value::Null {
                                *map.entry(v.clone()).or_insert(0) += 1;
                            }
                        }
                        if map.values().any(|&n| n > 1) {
                            return Err(err_unique(table, &t.cols[c0].0));
                        }
                    } else {
                        for row in &t.rows {
                            if let Some(k) = key_tuple(&cidx, row) {
                                *tcount.entry(k).or_insert(0) += 1;
                            }
                        }
                        if tcount.values().any(|&n| n > 1) {
                            let names: Vec<String> =
                                cidx.iter().map(|&c| t.cols[c].0.clone()).collect();
                            return Err(err_unique_cols(table, &names));
                        }
                    }
                }
            }
            let single = *unique && cidx.len() == 1;
            let t = db.tables.get_mut(table).expect("table present");
            t.indexes.push(IndexDef {
                name: name.clone(),
                cols: std::mem::take(&mut cidx),
                unique: *unique,
                tcount,
                entries,
            });
            if single {
                // The column joins the single-column unique set every
                // INSERT/UPDATE check already walks.
                t.reindex_membership();
            }
            if write_log {
                append_log(db, src)?;
            }
            Ok("OK 0\n".to_string())
        }
        Stmt::DropIndex { name } => {
            let mut target: Option<(String, usize)> = None;
            for (tn, t) in db.tables.iter() {
                if let Some(i) = t.index_pos(name) {
                    target = Some((tn.clone(), i));
                    break;
                }
            }
            let (tn, i) = match target {
                Some(x) => x,
                None => {
                    return Err(SqErr::new("no_table", format!("no such index: {}", name)));
                }
            };
            let t = db.tables.get_mut(&tn).expect("table present");
            t.indexes.remove(i);
            // A UNIQUE index's column may still be declared unique.
            t.reindex_membership();
            if write_log {
                append_log(db, src)?;
            }
            Ok("OK 0\n".to_string())
        }
    }
}

struct Proj {
    expr: Expr,
    hdr: String,
}

/// Three-valued logic: NULL -> None; integers by zero-ness; text converts
/// numerically (non-numeric text is 0), like the reference.
fn as_logic(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(*i != 0),
        Value::Text(s) => Some(
            parse_real(s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r'))
                .map(|f| f != 0.0)
                .unwrap_or(false),
        ),
    }
}

/// A WHERE predicate keeps a row only for a truthy (non-NULL, non-zero) value.
fn is_true(v: &Value) -> bool {
    matches!(as_logic(v), Some(true))
}

// ---------------------------------------------------------------------------
// Subquery scopes (ticket 10): the enclosing queries a subquery sees.
// ---------------------------------------------------------------------------

/// One query scope visible from inside a subquery: the column layout (`None`
/// for a table-less scope), the scope's current row (`NO_ROW` when there is
/// none), and name lookups for unqualified and qualified references. Lookup
/// indices address `tbl`'s rows in layout order.
#[derive(Clone)]
struct Frame<'a> {
    tbl: Option<&'a Table>,
    row: usize,
    bare: HashMap<String, Vec<usize>>,
    quals: HashMap<String, HashMap<String, Vec<usize>>>,
}

impl<'a> Frame<'a> {
    fn empty() -> Frame<'a> {
        Frame {
            tbl: None,
            row: NO_ROW,
            bare: HashMap::new(),
            quals: HashMap::new(),
        }
    }

    /// A single-table scope, visible under every name in `quals_names`.
    fn of_table(tbl: &'a Table, quals_names: &[String]) -> Frame<'a> {
        let mut cols: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, (n, _)) in tbl.cols.iter().enumerate() {
            cols.entry(n.clone()).or_default().push(i);
        }
        let mut quals = HashMap::new();
        for q in quals_names {
            quals.insert(q.clone(), cols.clone());
        }
        let bare = cols;
        Frame {
            tbl: Some(tbl),
            row: NO_ROW,
            bare,
            quals,
        }
    }

    /// Resolve a name against this scope: `None` when the scope does not
    /// carry it, `ambiguous` when several of its columns share it.
    fn find(&self, name: &str) -> R<Option<usize>> {
        let hit = if let Some(dot) = name.find('.') {
            let (q, c) = (&name[..dot], &name[dot + 1..]);
            match self.quals.get(q).and_then(|m| m.get(c)) {
                None => return Ok(None),
                Some(v) => v,
            }
        } else {
            match self.bare.get(name) {
                None => return Ok(None),
                Some(v) => v,
            }
        };
        if hit.len() == 1 {
            Ok(Some(hit[0]))
        } else {
            Err(SqErr::new(
                "ambiguous",
                format!("ambiguous column name: {}", name),
            ))
        }
    }

    /// The value of layout column `idx` on the scope's stored row (NULL when
    /// the scope has no current row).
    fn value(&self, idx: usize) -> Value {
        match self.tbl {
            Some(t) if self.row != NO_ROW && self.row < t.rows.len() => {
                t.rows[self.row][idx].clone()
            }
            _ => Value::Null,
        }
    }
}

/// A join scope: one qualifier per source table (its alias or name) and bare
/// names with `rewrite_join`'s ambiguity rules (USING right-sides merged into
/// the left side).
fn frame_maps(
    ctx: &JoinCtx,
) -> (
    HashMap<String, Vec<usize>>,
    HashMap<String, HashMap<String, Vec<usize>>>,
) {
    let mut quals: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
    let mut bare: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (r, tb)) in ctx.refs.iter().enumerate() {
        let q = r.alias.clone().unwrap_or_else(|| r.name.clone());
        let m = quals.entry(q).or_insert_with(HashMap::new);
        for (j, (n, _)) in tb.cols.iter().enumerate() {
            m.entry(n.clone()).or_default().push(ctx.offs[i] + j);
            if !ctx.using_skip.contains(&(i, n.clone())) {
                bare.entry(n.clone()).or_default().push(ctx.offs[i] + j);
            }
        }
    }
    (bare, quals)
}

fn frame_of_ctx<'x>(ctx: &JoinCtx, tbl: &'x Table) -> Frame<'x> {
    let (bare, quals) = frame_maps(ctx);
    Frame {
        tbl: Some(tbl),
        row: NO_ROW,
        bare,
        quals,
    }
}

/// What an expression may resolve against: the database plus the scope
/// chain. `frames[0]` is the scope the expression runs in (its row is passed
/// separately to `eval`); `frames[1..]` are enclosing queries, innermost
/// first, each with its current row already fixed.
struct Ecx<'a> {
    db: &'a Db,
    frames: Vec<Frame<'a>>,
}

/// The frame chain a subquery of `frames[0]` receives: that scope with its
/// current row `row`, then every enclosing scope unchanged.
fn sub_frames<'a>(cx: &Ecx<'a>, row: usize) -> Vec<Frame<'a>> {
    let mut out = Vec::with_capacity(cx.frames.len());
    out.push(Frame {
        row,
        ..cx.frames[0].clone()
    });
    out.extend(cx.frames[1..].iter().cloned());
    out
}

/// A constant false built only from the literal `0` and `AND`. The reference
/// treats the other operand of such an `AND` as completely inert: not
/// subquery-checked, not name-resolved, not evaluated.
fn lit0_false(e: &Expr) -> bool {
    match e {
        Expr::Lit(Lit::Int(0)) => true,
        Expr::Bin(BinOp::And, a, b) => lit0_false(a) || lit0_false(b),
        _ => false,
    }
}

/// Three-valued `[NOT] IN`: an empty set is false (even for a NULL left
/// side, so `NOT IN ()` is true); a match wins; otherwise a NULL on either
/// side makes the result NULL.
fn in_value(left: &Value, set: &[Value], neg: bool) -> Value {
    let truth = |b: bool| Value::Int(if b { 1 } else { 0 });
    if set.is_empty() {
        // An empty set is false even for a NULL left side (`NOT IN ()` true).
        return truth(neg);
    }
    let mut nulls = matches!(left, Value::Null);
    for v in set {
        if matches!(v, Value::Null) {
            nulls = true;
            continue;
        }
        if matches!(cmp_values(left, v), Some(Ordering::Equal)) {
            return truth(!neg);
        }
    }
    if nulls {
        Value::Null
    } else {
        truth(neg)
    }
}

/// The context a scalar subquery's column count is validated in: a bare use
/// (select item, ORDER BY, arithmetic operand, IN-list...) is `arity`; a
/// direct operand of a comparison is the reference's `row value misused`.
#[derive(Clone, Copy, PartialEq)]
enum SubCtx {
    Arity,
    Cmp,
}

fn sub_width_err(c: SubCtx) -> SqErr {
    match c {
        SubCtx::Arity => SqErr::arity("sub-select returns 2 columns - expected 1".to_string()),
        SubCtx::Cmp => SqErr::syn("row value misused".to_string()),
    }
}

/// The subqueries of one expression, prepared in full (tables, names,
/// nested checks) before any plain name of the statement is resolved —
/// the reference reports an error inside a subquery ahead of every
/// ordinary column error. Skipped operands of a literal-0 `AND` are not
/// checked at all.
fn precheck_subs(e: &Expr, cx: &Ecx) -> R<()> {
    match e {
        Expr::Sub(st) | Expr::Exists { sub: st } => {
            exec_select_data(cx.db, st, &sub_frames(cx, NO_ROW))?;
            Ok(())
        }
        Expr::InSub { left, sub, .. } => {
            precheck_subs(left, cx)?;
            exec_select_data(cx.db, sub, &sub_frames(cx, NO_ROW))?;
            Ok(())
        }
        Expr::InList { left, list, .. } => {
            precheck_subs(left, cx)?;
            for it in list {
                precheck_subs(it, cx)?;
            }
            Ok(())
        }
        Expr::Neg(x) | Expr::Not(x) => precheck_subs(x, cx),
        Expr::Bin(BinOp::And, a, b) => {
            if !lit0_false(b) {
                precheck_subs(a, cx)?;
            }
            if !lit0_false(a) {
                precheck_subs(b, cx)?;
            }
            Ok(())
        }
        Expr::Bin(_, a, b) => {
            precheck_subs(a, cx)?;
            precheck_subs(b, cx)
        }
        Expr::Func(_, args) | Expr::Agg { args, .. } => {
            for a in args {
                precheck_subs(a, cx)?;
            }
            Ok(())
        }
        Expr::Lit(_) | Expr::Col(_) | Expr::QCol(_, _) | Expr::QStar(_) => Ok(()),
    }
}

/// Late column-count validation of every subquery in one expression (run
/// after all plain names resolve, like the reference): a scalar subquery
/// with more than one column is `arity`, or `row value misused` directly
/// under a comparison; an `IN` subquery is `arity`; `EXISTS` may take any
/// width.
fn check_subs_width(e: &Expr, cx: &Ecx, c: SubCtx) -> R<()> {
    match e {
        Expr::Sub(st) => {
            let (hdr, _) = exec_select_data(cx.db, st, &sub_frames(cx, NO_ROW))?;
            if hdr.len() > 1 {
                return Err(sub_width_err(c));
            }
            Ok(())
        }
        Expr::Exists { .. } => Ok(()),
        Expr::InSub { left, sub, .. } => {
            check_subs_width(left, cx, SubCtx::Arity)?;
            let (hdr, _) = exec_select_data(cx.db, sub, &sub_frames(cx, NO_ROW))?;
            if hdr.len() > 1 {
                return Err(sub_width_err(SubCtx::Arity));
            }
            Ok(())
        }
        Expr::InList { left, list, .. } => {
            check_subs_width(left, cx, SubCtx::Arity)?;
            for it in list {
                check_subs_width(it, cx, SubCtx::Arity)?;
            }
            Ok(())
        }
        Expr::Neg(x) | Expr::Not(x) => check_subs_width(x, cx, SubCtx::Arity),
        Expr::Bin(op, a, b) => {
            if matches!(op, BinOp::And) {
                if !lit0_false(b) {
                    check_subs_width(a, cx, SubCtx::Arity)?;
                }
                if !lit0_false(a) {
                    check_subs_width(b, cx, SubCtx::Arity)?;
                }
                return Ok(());
            }
            let cmp = matches!(
                op,
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            );
            let ca = if cmp && matches!(**a, Expr::Sub(_)) {
                SubCtx::Cmp
            } else {
                SubCtx::Arity
            };
            let cb = if cmp && matches!(**b, Expr::Sub(_)) {
                SubCtx::Cmp
            } else {
                SubCtx::Arity
            };
            check_subs_width(a, cx, ca)?;
            check_subs_width(b, cx, cb)
        }
        Expr::Func(_, args) | Expr::Agg { args, .. } => {
            for a in args {
                check_subs_width(a, cx, SubCtx::Arity)?;
            }
            Ok(())
        }
        Expr::Lit(_) | Expr::Col(_) | Expr::QCol(_, _) | Expr::QStar(_) => Ok(()),
    }
}

/// Resolve a name innermost-first: the current scope's layout, its
/// qualifier/bare maps, then every enclosing scope. `Ok(None)` means no
/// scope carries it.
fn resolve_scoped(cx: &Ecx, name: &str) -> R<Option<(usize, usize)>> {
    // The current scope keeps its exact layout lookup first (it holds the
    // join's mangled duplicate names too).
    if let Some(t) = &cx.frames[0].tbl {
        if let Some(&i) = t.idx.get(name) {
            return Ok(Some((0, i)));
        }
    }
    for (fi, f) in cx.frames.iter().enumerate() {
        if let Some(i) = f.find(name)? {
            return Ok(Some((fi, i)));
        }
    }
    Ok(None)
}

/// Up-front name resolution (matches the reference: errors surface even on
/// empty tables). `allow_agg` marks the positions where aggregates are legal
/// (SELECT items, HAVING, and — when the query is aggregate — ORDER BY);
/// anywhere else an aggregate is a syntax error, found positionally while
/// walking the expression left to right.
fn resolve_expr(e: &Expr, cx: &Ecx, allow_agg: bool) -> R<()> {
    match e {
        Expr::Lit(_) => Ok(()),
        Expr::Col(n) => {
            if resolve_scoped(cx, n)?.is_some() {
                Ok(())
            } else {
                Err(SqErr::no_column(n))
            }
        }
        Expr::Neg(x) | Expr::Not(x) => resolve_expr(x, cx, allow_agg),
        Expr::Bin(BinOp::And, a, b) => {
            // The reference leaves the other operand of a literal-0 `AND`
            // unchecked.
            if !lit0_false(b) {
                resolve_expr(a, cx, allow_agg)?;
            }
            if !lit0_false(a) {
                resolve_expr(b, cx, allow_agg)?;
            }
            Ok(())
        }
        Expr::Bin(_, a, b) => {
            resolve_expr(a, cx, allow_agg)?;
            resolve_expr(b, cx, allow_agg)
        }
        // Qualified/star references that were not rewritten (no FROM, or
        // nested): a surviving qualifier may name an enclosing scope's
        // table; anything else the reference reports as a missing column.
        Expr::QCol(tname, cname) => {
            let dotted = format!("{}.{}", tname, cname);
            for f in cx.frames.iter().skip(1) {
                match f.find(&dotted)? {
                    Some(_) => return Ok(()),
                    None => continue,
                }
            }
            Err(SqErr::no_column(&dotted))
        }
        Expr::QStar(tname) => Err(SqErr::no_table(tname)),
        // Subqueries resolve fully at this point (their own FROM, nested
        // subqueries and names — correlated names against these frames).
        // The column-count checks are late: `check_subs_width`.
        Expr::Sub(st) => {
            exec_select_data(cx.db, st, &sub_frames(cx, NO_ROW))?;
            Ok(())
        }
        Expr::Exists { sub } => {
            exec_select_data(cx.db, sub, &sub_frames(cx, NO_ROW))?;
            Ok(())
        }
        Expr::InSub { left, sub, .. } => {
            resolve_expr(left, cx, allow_agg)?;
            exec_select_data(cx.db, sub, &sub_frames(cx, NO_ROW))?;
            Ok(())
        }
        Expr::InList { left, list, .. } => {
            resolve_expr(left, cx, allow_agg)?;
            for it in list {
                resolve_expr(it, cx, allow_agg)?;
            }
            Ok(())
        }
        Expr::Agg {
            name,
            star,
            args,
            distinct: _,
        } => {
            if !allow_agg {
                return Err(SqErr::syn(format!(
                    "misuse of aggregate function {}()",
                    name
                )));
            }
            // Arity at resolution time so `no_table` still wins. `count`
            // with no argument (or with `*`) counts rows; the others need
            // exactly one argument (star is arity for them).
            let ok = match name.as_str() {
                "count" => *star || args.len() <= 1,
                _ => !*star && args.len() == 1,
            };
            if !ok {
                return Err(SqErr::arity(format!(
                    "wrong number of arguments to function {}()",
                    name
                )));
            }
            // A nested aggregate is a misuse, like the reference.
            for a in args {
                resolve_expr(a, cx, false)?;
            }
            Ok(())
        }
        Expr::Func(name, args) => {
            let known = matches!(
                name.as_str(),
                "abs" | "length" | "lower" | "upper" | "coalesce"
            );
            if !known {
                return Err(SqErr::new(
                    "no_column",
                    format!("no such function: {}", name),
                ));
            }
            let ok = if name == "coalesce" {
                !args.is_empty()
            } else {
                args.len() == 1
            };
            if !ok {
                return Err(SqErr::arity(format!(
                    "wrong number of arguments to function {}()",
                    name
                )));
            }
            for a in args {
                resolve_expr(a, cx, allow_agg)?;
            }
            Ok(())
        }
    }
}

/// Does the expression contain an aggregate call anywhere? Subqueries are
/// their own queries: their aggregates never make the enclosing query an
/// aggregate query.
fn has_agg(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Col(_) | Expr::QCol(_, _) | Expr::QStar(_) => false,
        Expr::Neg(x) | Expr::Not(x) => has_agg(x),
        Expr::Bin(_, a, b) => has_agg(a) || has_agg(b),
        Expr::Func(_, args) => args.iter().any(has_agg),
        Expr::Agg { .. } => true,
        // The IN operands belong to the enclosing query (their aggregates
        // count); the subquery is its own query.
        Expr::Sub(_) | Expr::Exists { .. } => false,
        Expr::InSub { left, .. } => has_agg(left),
        Expr::InList { left, list, .. } => has_agg(left) || list.iter().any(has_agg),
    }
}

/// Does the expression contain a MIN/MAX aggregate (their winning row feeds
/// the bare columns of the result row, like the reference)?
fn has_minmax(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Col(_) | Expr::QCol(_, _) | Expr::QStar(_) => false,
        Expr::Neg(x) | Expr::Not(x) => has_minmax(x),
        Expr::Bin(_, a, b) => has_minmax(a) || has_minmax(b),
        Expr::Func(_, args) => args.iter().any(has_minmax),
        Expr::Agg { name, .. } => name == "min" || name == "max",
        Expr::Sub(_) | Expr::Exists { .. } => false,
        Expr::InSub { left, .. } => has_minmax(left),
        Expr::InList { left, list, .. } => has_minmax(left) || list.iter().any(has_minmax),
    }
}

/// Replace references to output aliases with the aliased expression
/// (GROUP BY, HAVING and ORDER BY expressions resolve aliases first, like
/// the reference). Select items themselves are never substituted.
fn subst_aliases(e: &Expr, aliases: &HashMap<String, usize>, proj: &[Proj]) -> Expr {
    match e {
        Expr::Col(w) => match aliases.get(w) {
            Some(&i) => proj[i].expr.clone(),
            None => e.clone(),
        },
        Expr::Neg(x) => Expr::Neg(Box::new(subst_aliases(x, aliases, proj))),
        Expr::Not(x) => Expr::Not(Box::new(subst_aliases(x, aliases, proj))),
        Expr::Bin(op, a, b) => Expr::Bin(
            op.clone(),
            Box::new(subst_aliases(a, aliases, proj)),
            Box::new(subst_aliases(b, aliases, proj)),
        ),
        Expr::Func(n, args) => Expr::Func(
            n.clone(),
            args.iter()
                .map(|a| subst_aliases(a, aliases, proj))
                .collect(),
        ),
        Expr::Agg {
            name,
            star,
            distinct,
            args,
        } => Expr::Agg {
            name: name.clone(),
            star: *star,
            distinct: *distinct,
            args: args
                .iter()
                .map(|a| subst_aliases(a, aliases, proj))
                .collect(),
        },
        // IN operands resolve aliases here; the subquery never sees the
        // enclosing query's aliases.
        Expr::InSub { neg, left, sub } => Expr::InSub {
            neg: *neg,
            left: Box::new(subst_aliases(left, aliases, proj)),
            sub: sub.clone(),
        },
        Expr::InList { neg, left, list } => Expr::InList {
            neg: *neg,
            left: Box::new(subst_aliases(left, aliases, proj)),
            list: list
                .iter()
                .map(|x| subst_aliases(x, aliases, proj))
                .collect(),
        },
        _ => e.clone(),
    }
}

/// NULL-propagating comparison; None means the result is NULL.
fn cmp_values(a: &Value, b: &Value) -> Option<Ordering> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return None;
    }
    Some(cmp_val(a, b))
}

fn to_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(i.to_string()),
        Value::Text(s) => Some(s.clone()),
    }
}

/// The reference's upper()/lower() convert ASCII letters only.
fn case_map(s: &str, up: bool) -> String {
    s.chars()
        .map(|c| {
            if up {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c
                }
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// Numeric coercion for arithmetic (the reference converts TEXT with its
/// numeric grammar; non-numeric text becomes 0).
fn arith_num(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(*i),
        Value::Text(s) => Some(
            parse_real(s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r'))
                .map(|f| f as i64)
                .unwrap_or(0),
        ),
    }
}

fn eval_bin(op: &BinOp, a: &Value, b: &Value) -> Value {
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            if matches!(a, Value::Null) || matches!(b, Value::Null) {
                return Value::Null;
            }
            let (x, y) = match (arith_num(a), arith_num(b)) {
                (Some(x), Some(y)) => (x, y),
                _ => return Value::Null,
            };
            // Division truncates toward zero and `%` takes the sign of the
            // dividend (Rust's / and % already do both); division or
            // modulo by zero yields NULL (checked_* returns None).
            let r = match op {
                BinOp::Add => x.checked_add(y),
                BinOp::Sub => x.checked_sub(y),
                BinOp::Mul => x.checked_mul(y),
                BinOp::Div => x.checked_div(y),
                BinOp::Mod => x.checked_rem(y),
                _ => None,
            };
            r.map(Value::Int).unwrap_or(Value::Null)
        }
        BinOp::Concat => match (to_text(a), to_text(b)) {
            (Some(x), Some(y)) => Value::Text(format!("{}{}", x, y)),
            _ => Value::Null,
        },
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            match cmp_values(a, b) {
                None => Value::Null,
                Some(ord) => {
                    let t = match op {
                        BinOp::Eq => ord == Ordering::Equal,
                        BinOp::Ne => ord != Ordering::Equal,
                        BinOp::Lt => ord == Ordering::Less,
                        BinOp::Le => ord != Ordering::Greater,
                        BinOp::Gt => ord == Ordering::Greater,
                        BinOp::Ge => ord != Ordering::Less,
                        _ => false,
                    };
                    Value::Int(if t { 1 } else { 0 })
                }
            }
        }
        BinOp::And => {
            let (x, y) = (as_logic(a), as_logic(b));
            match (x, y) {
                (Some(false), _) | (_, Some(false)) => Value::Int(0),
                (None, _) | (_, None) => Value::Null,
                _ => Value::Int(1),
            }
        }
        BinOp::Or => {
            let (x, y) = (as_logic(a), as_logic(b));
            match (x, y) {
                (Some(true), _) | (_, Some(true)) => Value::Int(1),
                (None, _) | (_, None) => Value::Null,
                _ => Value::Int(0),
            }
        }
        BinOp::Is => {
            let eq = match (a, b) {
                (Value::Null, Value::Null) => true,
                (Value::Null, _) | (_, Value::Null) => false,
                _ => matches!(cmp_val(a, b), Ordering::Equal),
            };
            Value::Int(if eq { 1 } else { 0 })
        }
    }
}

fn eval_func(name: &str, args: &[Value]) -> Value {
    match name {
        "length" => match &args[0] {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Int(i.to_string().len() as i64),
            Value::Text(s) => Value::Int(s.chars().count() as i64),
        },
        "upper" => match &args[0] {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Text(i.to_string()),
            Value::Text(s) => Value::Text(case_map(s, true)),
        },
        "lower" => match &args[0] {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Text(i.to_string()),
            Value::Text(s) => Value::Text(case_map(s, false)),
        },
        "abs" => match &args[0] {
            Value::Null => Value::Null,
            Value::Int(i) => i.checked_abs().map(Value::Int).unwrap_or(Value::Null),
            Value::Text(s) => match s.trim().parse::<i64>() {
                Ok(v) => v.checked_abs().map(Value::Int).unwrap_or(Value::Null),
                Err(_) => Value::Int(0),
            },
        },
        _ => {
            // coalesce: first non-NULL argument.
            for v in args {
                if !matches!(v, Value::Null) {
                    return v.clone();
                }
            }
            Value::Null
        }
    }
}

/// The current scope's column value at `row` (NULL when there is no row).
fn row_value(cx: &Ecx, fi: usize, idx: usize, row: usize) -> Value {
    match &cx.frames[fi].tbl {
        Some(t) if row != NO_ROW && row < t.rows.len() => t.rows[row][idx].clone(),
        _ => Value::Null,
    }
}

/// An unqualified column: the current scope at `row`, then each enclosing
/// scope innermost-first (their rows are stored on the frame).
fn eval_col(cx: &Ecx, n: &str, row: usize) -> R<Value> {
    if let Some(t) = &cx.frames[0].tbl {
        if let Some(&i) = t.idx.get(n) {
            return Ok(row_value(cx, 0, i, row));
        }
    }
    for (fi, f) in cx.frames.iter().enumerate() {
        if let Some(i) = f.find(n)? {
            return Ok(if fi == 0 {
                row_value(cx, 0, i, row)
            } else {
                f.value(i)
            });
        }
    }
    Err(SqErr::no_column(n))
}

/// A qualified column that survived preparation names an enclosing scope's
/// table (the current scope was consumed by `rewrite_join`/`dml_qual`).
fn eval_qcol(cx: &Ecx, q: &str, c: &str) -> R<Value> {
    let dotted = format!("{}.{}", q, c);
    for f in cx.frames.iter().skip(1) {
        if let Some(i) = f.find(&dotted)? {
            return Ok(f.value(i));
        }
    }
    // Unreachable after resolution; kept for safety.
    Err(SqErr::syn("qualified column outside a join".to_string()))
}

/// Run one subquery and return its data rows (headers unused here).
fn run_sub(
    cx: &Ecx,
    sub: &Stmt,
    row: usize,
) -> R<(Vec<String>, Vec<Vec<Value>>)> {
    exec_select_data(cx.db, sub, &sub_frames(cx, row))
}

fn eval(e: &Expr, cx: &Ecx, r: usize) -> R<Value> {
    match e {
        Expr::Lit(l) => Ok(lit_value(l)),
        Expr::Col(n) => eval_col(cx, n, r),
        Expr::Neg(x) => {
            let v = eval(x, cx, r)?;
            neg_value(v)
        }
        Expr::Not(x) => {
            let v = eval(x, cx, r)?;
            Ok(match as_logic(&v) {
                None => Value::Null,
                Some(true) => Value::Int(0),
                Some(false) => Value::Int(1),
            })
        }
        Expr::Bin(BinOp::And, a, b) => {
            // A literal-0 `AND` makes the other operand inert: the
            // reference never evaluates it (nor prepares its subqueries).
            if lit0_false(a) || lit0_false(b) {
                return Ok(Value::Int(0));
            }
            let (va, vb) = (eval(a, cx, r)?, eval(b, cx, r)?);
            Ok(eval_bin(&BinOp::And, &va, &vb))
        }
        Expr::Bin(op, a, b) => {
            // Eager evaluation: name/type errors surface like the reference's
            // up-front resolution.
            let (va, vb) = (eval(a, cx, r)?, eval(b, cx, r)?);
            Ok(eval_bin(op, &va, &vb))
        }
        Expr::Func(name, args) => {
            let mut vs = Vec::with_capacity(args.len());
            for a in args {
                vs.push(eval(a, cx, r)?);
            }
            Ok(eval_func(name, &vs))
        }
        // Row context never evaluates an aggregate: resolve_expr rejects
        // them everywhere except aggregate positions.
        Expr::Agg { name, .. } => Err(SqErr::syn(format!(
            "misuse of aggregate function {}()",
            name
        ))),
        Expr::QCol(q, c) => eval_qcol(cx, q, c),
        Expr::QStar(_) => Err(SqErr::syn(
            "qualified column outside a join".to_string(),
        )),
        // Correlated subqueries re-run per outer row.
        Expr::Sub(st) => {
            let (_, rows) = run_sub(cx, st, r)?;
            match rows.first() {
                Some(row) => Ok(row.first().cloned().unwrap_or(Value::Null)),
                None => Ok(Value::Null),
            }
        }
        Expr::Exists { sub } => {
            let (_, rows) = run_sub(cx, sub, r)?;
            Ok(Value::Int(if rows.is_empty() { 0 } else { 1 }))
        }
        Expr::InSub { neg, left, sub } => {
            let lv = eval(left, cx, r)?;
            let (_, rows) = run_sub(cx, sub, r)?;
            let set: Vec<Value> = rows
                .into_iter()
                .map(|row| row.into_iter().next().unwrap_or(Value::Null))
                .collect();
            Ok(in_value(&lv, &set, *neg))
        }
        Expr::InList { neg, left, list } => {
            let lv = eval(left, cx, r)?;
            let mut set = Vec::with_capacity(list.len());
            for it in list {
                set.push(eval(it, cx, r)?);
            }
            Ok(in_value(&lv, &set, *neg))
        }
    }
}

/// Unary minus over an already-evaluated operand (shared by row and group
/// evaluation).
fn neg_value(v: Value) -> R<Value> {
    Ok(match v {
        Value::Int(i) => i.checked_neg().map(Value::Int).unwrap_or(Value::Null),
        Value::Null => Value::Null,
        Value::Text(s) => {
            // Unary minus coerces text numerically, like `+`/`-` do:
            // non-numeric text is 0; a real-valued result is outside
            // the dialect.
            match parse_real(s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r'))
            {
                None => Value::Int(0),
                Some(f) => {
                    if f.fract() == 0.0
                        && f >= -9223372036854775808.0f64
                        && f < 9223372036854775808.0f64
                    {
                        (f as i64)
                            .checked_neg()
                            .map(Value::Int)
                            .unwrap_or(Value::Null)
                    } else {
                        return Err(SqErr::syn("REAL/BLOB outside the dialect".to_string()));
                    }
                }
            }
        }
    })
}

fn real_err() -> SqErr {
    SqErr::syn("REAL/BLOB outside the dialect".to_string())
}

/// Evaluate an expression in a group's context: aggregates aggregate over
/// `rows`, bare columns read the group's chosen row `rep` (NULL when the
/// group is empty). Non-aggregate subtrees behave exactly like `eval`.
fn eval_group(e: &Expr, cx: &Ecx, rows: &[usize], rep: Option<usize>) -> R<Value> {
    let row = rep.unwrap_or(NO_ROW);
    match e {
        Expr::Lit(l) => Ok(lit_value(l)),
        Expr::Col(n) => eval_col(cx, n, row),
        Expr::Agg {
            name,
            star,
            distinct,
            args,
        } => agg_value(name, *star, *distinct, args, cx, rows),
        Expr::Neg(x) => neg_value(eval_group(x, cx, rows, rep)?),
        Expr::Not(x) => Ok(match as_logic(&eval_group(x, cx, rows, rep)?) {
            None => Value::Null,
            Some(true) => Value::Int(0),
            Some(false) => Value::Int(1),
        }),
        Expr::Bin(BinOp::And, a, b) => {
            if lit0_false(a) || lit0_false(b) {
                return Ok(Value::Int(0));
            }
            let (va, vb) = (
                eval_group(a, cx, rows, rep)?,
                eval_group(b, cx, rows, rep)?,
            );
            Ok(eval_bin(&BinOp::And, &va, &vb))
        }
        Expr::Bin(op, a, b) => {
            let (va, vb) = (
                eval_group(a, cx, rows, rep)?,
                eval_group(b, cx, rows, rep)?,
            );
            Ok(eval_bin(op, &va, &vb))
        }
        Expr::Func(name, args) => {
            let mut vs = Vec::with_capacity(args.len());
            for a in args {
                vs.push(eval_group(a, cx, rows, rep)?);
            }
            Ok(eval_func(name, &vs))
        }
        Expr::QCol(q, c) => eval_qcol(cx, q, c),
        Expr::QStar(_) => Err(SqErr::syn(
            "qualified column outside a join".to_string(),
        )),
        // In group context a subquery correlates with the group's
        // representative row (NULL when the group is empty).
        Expr::Sub(st) => {
            let (_, out) = run_sub(cx, st, row)?;
            match out.first() {
                Some(r) => Ok(r.first().cloned().unwrap_or(Value::Null)),
                None => Ok(Value::Null),
            }
        }
        Expr::Exists { sub } => {
            let (_, out) = run_sub(cx, sub, row)?;
            Ok(Value::Int(if out.is_empty() { 0 } else { 1 }))
        }
        Expr::InSub { neg, left, sub } => {
            let lv = eval_group(left, cx, rows, rep)?;
            let (_, out) = run_sub(cx, sub, row)?;
            let set: Vec<Value> = out
                .into_iter()
                .map(|r| r.into_iter().next().unwrap_or(Value::Null))
                .collect();
            Ok(in_value(&lv, &set, *neg))
        }
        Expr::InList { neg, left, list } => {
            let lv = eval_group(left, cx, rows, rep)?;
            let mut set = Vec::with_capacity(list.len());
            for it in list {
                set.push(eval_group(it, cx, rows, rep)?);
            }
            Ok(in_value(&lv, &set, *neg))
        }
    }
}

/// One aggregate's value over the group's rows. `COUNT(*)`/`COUNT()` count
/// rows; every other aggregate skips NULLs; empty input gives COUNT 0 and
/// NULL for SUM/MIN/MAX/AVG. SUM over text follows the reference's
/// numerify rule (integer-looking text adds as integer, anything else is
/// REAL and thus outside the dialect), and AVG would always be REAL.
fn agg_value(
    name: &str,
    star: bool,
    distinct: bool,
    args: &[Expr],
    cx: &Ecx,
    rows: &[usize],
) -> R<Value> {
    if name == "count" && (star || args.is_empty()) {
        return Ok(Value::Int(rows.len() as i64));
    }
    let arg = match args.first() {
        Some(a) => a,
        None => return Ok(Value::Null),
    };
    let mut vals: Vec<Value> = Vec::with_capacity(rows.len());
    for &r in rows {
        vals.push(eval(arg, cx, r)?);
    }
    if distinct {
        let mut seen: HashSet<Value> = HashSet::with_capacity(vals.len());
        vals.retain(|v| seen.insert(v.clone()));
    }
    match name {
        "count" => Ok(Value::Int(
            vals.iter().filter(|v| !matches!(v, Value::Null)).count() as i64,
        )),
        "sum" => {
            let mut acc: i64 = 0;
            let mut any = false;
            for v in vals.iter() {
                match v {
                    Value::Null => continue,
                    Value::Int(i) => {
                        any = true;
                        acc = match acc.checked_add(*i) {
                            Some(a) => a,
                            None => return Err(real_err()),
                        };
                    }
                    Value::Text(s) => {
                        any = true;
                        match parse_real(s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r'))
                        {
                            Some(f)
                                if f.fract() == 0.0
                                    && f >= -9223372036854775808.0f64
                                    && f < 9223372036854775808.0f64 =>
                            {
                                acc = match acc.checked_add(f as i64) {
                                    Some(a) => a,
                                    None => return Err(real_err()),
                                };
                            }
                            _ => return Err(real_err()),
                        }
                    }
                }
            }
            Ok(if any { Value::Int(acc) } else { Value::Null })
        }
        "min" | "max" => {
            let mut best: Option<Value> = None;
            for v in vals.iter() {
                if matches!(v, Value::Null) {
                    continue;
                }
                let better = match &best {
                    None => true,
                    Some(b) => {
                        let c = cmp_val(v, b);
                        if name == "min" {
                            c == Ordering::Less
                        } else {
                            c == Ordering::Greater
                        }
                    }
                };
                if better {
                    best = Some(v.clone());
                }
            }
            Ok(best.unwrap_or(Value::Null))
        }
        _ => {
            if vals.iter().any(|v| !matches!(v, Value::Null)) {
                Err(real_err())
            } else {
                Ok(Value::Null)
            }
        }
    }
}

/// The source row that produced a MIN/MAX aggregate's value (first row
/// wins ties), or None when there is no non-NULL input.
fn agg_win(e: &Expr, cx: &Ecx, rows: &[usize]) -> Option<usize> {
    match e {
        Expr::Agg { name, args, .. } => {
            let arg = args.first()?;
            let mut best: Option<(usize, Value)> = None;
            for &r in rows {
                let v = eval(arg, cx, r).ok()?;
                if matches!(v, Value::Null) {
                    continue;
                }
                let better = match &best {
                    None => true,
                    Some((_, b)) => {
                        let c = cmp_val(&v, b);
                        if name == "min" {
                            c == Ordering::Less
                        } else {
                            c == Ordering::Greater
                        }
                    }
                };
                if better {
                    best = Some((r, v));
                }
            }
            best.map(|(r, _)| r)
        }
        _ => None,
    }
}

/// Walk the select list in evaluation order, remembering the last
/// MIN/MAX node's winning row: the reference feeds the result row's other
/// expressions from it.
fn scan_rep(e: &Expr, cx: &Ecx, rows: &[usize], slot: &mut Option<usize>) {
    match e {
        Expr::Agg { name, args, .. } if name == "min" || name == "max" => {
            if args.is_empty() {
                return;
            }
            if let Some(r) = agg_win(e, cx, rows) {
                *slot = Some(r);
            }
        }
        Expr::Agg { .. } => {}
        Expr::Neg(x) | Expr::Not(x) => scan_rep(x, cx, rows, slot),
        Expr::Bin(_, a, b) => {
            scan_rep(a, cx, rows, slot);
            scan_rep(b, cx, rows, slot);
        }
        Expr::Func(_, args) => {
            for a in args {
                scan_rep(a, cx, rows, slot);
            }
        }
        // A subquery's own MIN/MAX never feeds the enclosing group's row;
        // the IN operands do.
        Expr::Sub(_) | Expr::Exists { .. } => {}
        Expr::InSub { left, .. } => scan_rep(left, cx, rows, slot),
        Expr::InList { left, list, .. } => {
            scan_rep(left, cx, rows, slot);
            for a in list {
                scan_rep(a, cx, rows, slot);
            }
        }
        Expr::Lit(_) | Expr::Col(_) | Expr::QCol(_, _) | Expr::QStar(_) => {}
    }
}

enum OrdKey {
    Item(usize),
    TableCol(usize),
    Const(Value),
    /// UPDATE/DELETE `ORDER BY 1`: the row's position in scan order.
    RowIdx,
    /// A SELECT `ORDER BY` expression, evaluated per output row.
    Expr(Expr),
}

fn val_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Int(_) => 1,
        Value::Text(_) => 2,
    }
}

fn cmp_val(a: &Value, b: &Value) -> Ordering {
    let (ra, rb) = (val_rank(a), val_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Group-key tuple comparison (same class ordering as ORDER BY: NULL
/// first, then INTEGER, then TEXT bytewise) for emitting groups in
/// sorted-key order.
fn cmp_key_tuple(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let c = cmp_val(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

fn cmp_term(a: &Value, b: &Value, asc: bool, nulls_first: Option<bool>) -> Ordering {
    let a_null = matches!(a, Value::Null);
    let b_null = matches!(b, Value::Null);
    if a_null && b_null {
        return Ordering::Equal;
    }
    if a_null || b_null {
        if let Some(f) = nulls_first {
            let a_less = a_null;
            return if a_less == f { Ordering::Less } else { Ordering::Greater };
        }
        // Default: NULLs sort first ascending, last descending (full reverse).
        let base = if a_null { Ordering::Less } else { Ordering::Greater };
        return if asc { base } else { base.reverse() };
    }
    let c = cmp_val(a, b);
    if asc {
        c
    } else {
        c.reverse()
    }
}

/// Evaluate a LIMIT/OFFSET value: the expression itself sees no table
/// (columns are `no_column`) but may contain subqueries; the result must be
/// an integer.
fn lim_value(v: &LimVal, cx: &Ecx) -> R<i64> {
    match v {
        LimVal::Big => Err(SqErr::typ("datatype mismatch".to_string())),
        LimVal::Expr(e) => {
            let mut frames = vec![Frame::empty()];
            frames.extend(cx.frames[1..].iter().cloned());
            let lcx = Ecx {
                db: cx.db,
                frames,
            };
            resolve_expr(e, &lcx, false)?;
            match eval(e, &lcx, 0) {
                Ok(Value::Int(i)) => Ok(i),
                Ok(Value::Text(s)) => s
                    .trim_matches(|c| c == ' ' || c == '\t' || c == '\r')
                    .parse::<i64>()
                    .map_err(|_| SqErr::typ("datatype mismatch".to_string())),
                Ok(Value::Null) => Err(SqErr::typ("datatype mismatch".to_string())),
                // A real-valued LIMIT is a datatype mismatch, not the
                // dialect's REAL syntax error.
                Err(err) => {
                    if err.msg.starts_with("REAL/BLOB") {
                        Err(SqErr::typ("datatype mismatch".to_string()))
                    } else {
                        Err(err)
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Joins: the FROM list is resolved against a per-query column layout and
// materialized into one synthetic table that the rest of exec_select runs
// on unchanged (projection, WHERE, aggregates, HAVING, ORDER BY, LIMIT).
// ---------------------------------------------------------------------------

/// Column layout of a (possibly multi-table) FROM list. Display names feed
/// `*` headers (duplicates allowed); lookup names key the synthetic table's
/// `idx` — a name that appears in more than one source table is mangled
/// there, so a bare reference to it fails the ambiguity check first.
struct JoinCtx<'a> {
    refs: Vec<(&'a TableRef, &'a Table)>,
    /// per source table: starting offset in the synthetic row layout
    offs: Vec<usize>,
    /// synthetic lookup name per (ref, col)
    lookup: HashMap<(usize, usize), String>,
    /// source table index per synthetic lookup name
    owner: HashMap<String, usize>,
    /// column names present in more than one source table
    dups: HashSet<String>,
    /// display name per synthetic column, in row order
    display: Vec<String>,
    /// `USING` right-sides: excluded from `*` and from bare-column
    /// ambiguity (the reference merges them into the left side).
    using_skip: HashSet<(usize, String)>,
}

impl<'a> JoinCtx<'a> {
    fn build(refs: Vec<(&'a TableRef, &'a Table)>) -> JoinCtx<'a> {
        let mut count: HashMap<String, usize> = HashMap::new();
        for (_, tb) in &refs {
            for (n, _) in &tb.cols {
                *count.entry(n.clone()).or_insert(0) += 1;
            }
        }
        let dups: HashSet<String> = count
            .iter()
            .filter(|(_, &c)| c > 1)
            .map(|(n, _)| n.clone())
            .collect();
        let mut offs = Vec::with_capacity(refs.len());
        let mut at = 0usize;
        for (_, tb) in &refs {
            offs.push(at);
            at += tb.cols.len();
        }
        let mut lookup: HashMap<(usize, usize), String> = HashMap::new();
        let mut owner: HashMap<String, usize> = HashMap::new();
        let mut display: Vec<String> = Vec::with_capacity(at);
        for (i, (_, tb)) in refs.iter().enumerate() {
            for (j, (n, _)) in tb.cols.iter().enumerate() {
                let name = if dups.contains(n) {
                    format!("{}\u{1}{}", n, i)
                } else {
                    n.clone()
                };
                owner.insert(name.clone(), i);
                lookup.insert((i, j), name);
                display.push(n.clone());
            }
        }
        JoinCtx {
            refs,
            offs,
            lookup,
            owner,
            dups,
            display,
            using_skip: HashSet::new(),
        }
    }

    fn synth_name(&self, ref_i: usize, col_i: usize) -> String {
        self.lookup[&(ref_i, col_i)].clone()
    }

    fn holders(&self, n: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (i, (_, tb)) in self.refs.iter().enumerate() {
            if let Some(&j) = tb.idx.get(n) {
                if self.using_skip.contains(&(i, n.to_string())) {
                    continue;
                }
                out.push((i, j));
            }
        }
        out
    }

    /// The synthetic table's structure (columns + idx, no rows).
    fn structure(&self) -> Table {
        let mut cols: Vec<(String, ColType)> = Vec::with_capacity(self.display.len());
        for (i, (_, tb)) in self.refs.iter().enumerate() {
            for (j, (_, ct)) in tb.cols.iter().enumerate() {
                let d = self.display[self.offs[i] + j].clone();
                cols.push((d, *ct));
            }
        }
        let mut t = Table::bare(cols);
        for (&(i, j), name) in self.lookup.iter() {
            t.idx.insert(name.clone(), self.offs[i] + j);
        }
        t
    }
}

/// Rewrite qualified references against the FROM list and reject ambiguous
/// bare columns (the reference's `ambiguous` class). Bare columns that live
/// in at most one table — including every single-table query — come back
/// unchanged, so the existing single-table paths see exactly what they did
/// before. Invalid qualifiers become a sentinel column that resolution
/// reports as `no_column`.
fn rewrite_join(e: &Expr, ctx: &JoinCtx) -> R<Expr> {
    match e {
        Expr::Col(n) => {
            let hs = ctx.holders(n);
            if hs.len() > 1 {
                return Err(SqErr::new(
                    "ambiguous",
                    format!("ambiguous column name: {}", n),
                ));
            }
            match hs.first() {
                // A single holder resolves (identity for non-dup names).
                Some(&(i, j)) => Ok(Expr::Col(ctx.synth_name(i, j))),
                None => Ok(e.clone()),
            }
        }
        Expr::QCol(q, c) => {
            // A qualifier may match several sources (a duplicated alias or
            // table name); the column then resolves when exactly one of
            // them owns it, is `ambiguous` when several do, and is a
            // missing column otherwise (sentinel -> no_column).
            let mut hits: Vec<(usize, usize)> = Vec::new();
            for (i, (r, tb)) in ctx.refs.iter().enumerate() {
                if r.matches(q) {
                    if let Some(&j) = tb.idx.get(c) {
                        hits.push((i, j));
                    }
                }
            }
            match hits.len() {
                0 => Ok(Expr::Col(format!("{}.{}", q, c))),
                1 => Ok(Expr::Col(ctx.synth_name(hits[0].0, hits[0].1))),
                _ => Err(SqErr::new(
                    "ambiguous",
                    format!("ambiguous column name: {}.{}", q, c),
                )),
            }
        }
        Expr::QStar(_) => Err(SqErr::syn("misuse of \"*\" in an expression".to_string())),
        // The IN operands belong to this query; the subquery (if any) is
        // resolved against its own FROM when prepared.
        Expr::InSub { neg, left, sub } => Ok(Expr::InSub {
            neg: *neg,
            left: Box::new(rewrite_join(left, ctx)?),
            sub: sub.clone(),
        }),
        Expr::InList { neg, left, list } => Ok(Expr::InList {
            neg: *neg,
            left: Box::new(rewrite_join(left, ctx)?),
            list: list
                .iter()
                .map(|x| rewrite_join(x, ctx))
                .collect::<R<Vec<Expr>>>()?,
        }),
        Expr::Neg(x) => Ok(Expr::Neg(Box::new(rewrite_join(x, ctx)?))),
        Expr::Not(x) => Ok(Expr::Not(Box::new(rewrite_join(x, ctx)?))),
        Expr::Bin(op, a, b) => Ok(Expr::Bin(
            op.clone(),
            Box::new(rewrite_join(a, ctx)?),
            Box::new(rewrite_join(b, ctx)?),
        )),
        Expr::Func(n, args) => {
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                out.push(rewrite_join(a, ctx)?);
            }
            Ok(Expr::Func(n.clone(), out))
        }
        Expr::Agg {
            name,
            star,
            distinct,
            args,
        } => {
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                out.push(rewrite_join(a, ctx)?);
            }
            Ok(Expr::Agg {
                name: name.clone(),
                star: *star,
                distinct: *distinct,
                args: out,
            })
        }
        _ => Ok(e.clone()),
    }
}

/// DML names one table: rewrite `q.c` against its visible name (the alias
/// if present, else the table name) so a hidden or unknown qualifier falls
/// through resolution as `no_column`, like the reference.
fn dml_qual(e: &Expr, vis: &str) -> Expr {
    match e {
        Expr::QCol(q, c) => {
            if q.as_str() == vis {
                Expr::Col(c.clone())
            } else {
                Expr::Col(format!("{}.{}", q, c))
            }
        }
        // The IN operands are this query's; a nested subquery keeps its own
        // qualifiers untouched.
        Expr::InSub { neg, left, sub } => Expr::InSub {
            neg: *neg,
            left: Box::new(dml_qual(left, vis)),
            sub: sub.clone(),
        },
        Expr::InList { neg, left, list } => Expr::InList {
            neg: *neg,
            left: Box::new(dml_qual(left, vis)),
            list: list.iter().map(|x| dml_qual(x, vis)).collect(),
        },
        Expr::Neg(x) => Expr::Neg(Box::new(dml_qual(x, vis))),
        Expr::Not(x) => Expr::Not(Box::new(dml_qual(x, vis))),
        Expr::Bin(op, a, b) => Expr::Bin(
            op.clone(),
            Box::new(dml_qual(a, vis)),
            Box::new(dml_qual(b, vis)),
        ),
        Expr::Func(n, args) => Expr::Func(
            n.clone(),
            args.iter().map(|a| dml_qual(a, vis)).collect(),
        ),
        Expr::Agg {
            name,
            star,
            distinct,
            args,
        } => Expr::Agg {
            name: name.clone(),
            star: *star,
            distinct: *distinct,
            args: args.iter().map(|a| dml_qual(a, vis)).collect(),
        },
        _ => e.clone(),
    }
}

/// Collect the columns an expression references, as (ref, col) positions —
/// names outside the layout yield None (never hashed, safe fallback).
fn expr_cols(e: &Expr, ctx: &JoinCtx, out: &mut Vec<(usize, usize)>) -> bool {
    match e {
        Expr::Col(n) => {
            for (&(ri, cj), name) in ctx.lookup.iter() {
                if name == n {
                    out.push((ri, cj));
                    return true;
                }
            }
            false
        }
        Expr::Lit(_) | Expr::QCol(_, _) | Expr::QStar(_) => false,
        Expr::Neg(x) | Expr::Not(x) => expr_cols(x, ctx, out),
        Expr::Bin(_, a, b) => expr_cols(a, ctx, out) && expr_cols(b, ctx, out),
        Expr::Func(_, args) => args.iter().all(|a| expr_cols(a, ctx, out)),
        Expr::Agg { args, .. } => args.iter().all(|a| expr_cols(a, ctx, out)),
        // Subqueries resolve against their own FROM (and enclosing scopes),
        // never against this layout.
        Expr::Sub(_) | Expr::Exists { .. } => false,
        Expr::InSub { left, .. } => expr_cols(left, ctx, out),
        Expr::InList { left, list, .. } => {
            expr_cols(left, ctx, out) && list.iter().all(|a| expr_cols(a, ctx, out))
        }
    }
}

/// The source-table indices an expression touches (after resolution every
/// column lives in the layout, so the owner map is exact). Used to spot an
/// ON condition that references tables the join has not reached yet.
fn expr_ref_idxs(e: &Expr, ctx: &JoinCtx, out: &mut Vec<usize>) {
    match e {
        Expr::Col(n) => {
            if let Some(&i) = ctx.owner.get(n) {
                out.push(i);
            }
        }
        Expr::Lit(_) | Expr::QCol(_, _) | Expr::QStar(_) => {}
        Expr::Neg(x) | Expr::Not(x) => expr_ref_idxs(x, ctx, out),
        Expr::Bin(_, a, b) => {
            expr_ref_idxs(a, ctx, out);
            expr_ref_idxs(b, ctx, out);
        }
        Expr::Func(_, args) => {
            for a in args {
                expr_ref_idxs(a, ctx, out);
            }
        }
        Expr::Agg { args, .. } => {
            for a in args {
                expr_ref_idxs(a, ctx, out);
            }
        }
        Expr::Sub(_) | Expr::Exists { .. } => {}
        Expr::InSub { left, .. } => expr_ref_idxs(left, ctx, out),
        Expr::InList { left, list, .. } => {
            expr_ref_idxs(left, ctx, out);
            for a in list {
                expr_ref_idxs(a, ctx, out);
            }
        }
    }
}

/// One side of a usable hash key: the earlier-side expression (evaluated
/// against the already-joined prefix) and the joining table's column to
/// index. Walks the top-level AND chain of an ON or WHERE condition.
fn extract_hash_key(e: &Expr, ctx: &JoinCtx, ref_i: usize) -> Option<(Expr, usize)> {
    // helper: is this expr a column owned by ref_i?
    let side = |x: &Expr| -> Option<usize> {
        if let Expr::Col(n) = x {
            for (&(ri, cj), name) in ctx.lookup.iter() {
                if ri == ref_i && name == n {
                    return Some(ctx.offs[ref_i] + cj);
                }
            }
        }
        None
    };
    let only_earlier = |x: &Expr| -> bool {
        let mut v = Vec::new();
        if !expr_cols(x, ctx, &mut v) {
            return false;
        }
        v.iter().all(|&(ri, _)| ri < ref_i)
    };
    match e {
        Expr::Bin(BinOp::Eq, a, b) => {
            if let Some(g) = side(b) {
                if only_earlier(a) {
                    return Some(((**a).clone(), g));
                }
            }
            if let Some(g) = side(a) {
                if only_earlier(b) {
                    return Some(((**b).clone(), g));
                }
            }
            None
        }
        Expr::Bin(BinOp::And, a, b) => extract_hash_key(a, ctx, ref_i)
            .or_else(|| extract_hash_key(b, ctx, ref_i)),
        _ => None,
    }
}

/// `USING (c, ...)` as a conjunction of equalities between the joined
/// table's column and the first earlier table that has it.
fn build_using(step: &JoinStep, ctx: &JoinCtx, ref_i: usize) -> R<Option<Expr>> {
    if step.using.is_empty() {
        return Ok(None);
    }
    let (_, tbl) = &ctx.refs[ref_i];
    let mut acc: Option<Expr> = None;
    for name in &step.using {
        // The reference validates USING columns up front: anything not
        // present on both sides is a syntax error.
        let not_both = || {
            SqErr::syn(format!(
                "cannot join using column {} - column not present in both tables",
                name
            ))
        };
        let rj = match tbl.idx.get(name) {
            Some(&j) => j,
            None => return Err(not_both()),
        };
        let mut left: Option<(usize, usize)> = None;
        for i in 0..ref_i {
            if let Some(&j) = ctx.refs[i].1.idx.get(name) {
                left = Some((i, j));
                break;
            }
        }
        let (li, lj) = match left {
            Some(x) => x,
            None => return Err(not_both()),
        };
        let eq = Expr::Bin(
            BinOp::Eq,
            Box::new(Expr::Col(ctx.synth_name(li, lj))),
            Box::new(Expr::Col(ctx.synth_name(ref_i, rj))),
        );
        acc = Some(match acc {
            None => eq,
            Some(prev) => Expr::Bin(BinOp::And, Box::new(prev), Box::new(eq)),
        });
    }
    Ok(acc)
}

/// A context for evaluating against the synthetic table while it is being
/// filled: the join layout's name maps with the rows as they currently
/// stand, then every enclosing scope.
fn mk_ecx<'m>(
    outer: &Ecx<'m>,
    bare: &HashMap<String, Vec<usize>>,
    quals: &HashMap<String, HashMap<String, Vec<usize>>>,
    synth: &'m Table,
) -> Ecx<'m> {
    let mut frames = Vec::with_capacity(outer.frames.len());
    frames.push(Frame {
        tbl: Some(synth),
        row: NO_ROW,
        bare: bare.clone(),
        quals: quals.clone(),
    });
    frames.extend(outer.frames[1..].iter().cloned());
    Ecx {
        db: outer.db,
        frames,
    }
}

/// Fill the synthetic table's rows from the join steps: a nested loop over
/// each step's candidate rows (hash-pruned when an equality keys the
/// joining table), ON/USING per step, and WHERE once every table is in the
/// row. Failed candidates are dropped immediately, so a cross product that
/// WHERE trims never materializes.
fn materialize(
    mut synth: Table,
    ctx: &JoinCtx,
    joins: &[JoinStep],
    on_res: &[Option<Expr>],
    using_res: &[Option<Expr>],
    wexpr: Option<&Expr>,
    deferred_on: &[Expr],
    outer: &Ecx,
) -> R<Table> {
    let total = synth.cols.len();
    let (bare, quals) = frame_maps(ctx);
    // Seed with the first table (already in the row layout; other tables
    // stay NULL until their step fills them).
    let (_, t0) = ctx.refs[0];
    for r in 0..t0.rows.len() {
        let mut row = vec![Value::Null; total];
        for (j, v) in t0.rows[r].iter().enumerate() {
            row[ctx.offs[0] + j] = v.clone();
        }
        synth.rows.push(row);
    }

    for (si, step) in joins.iter().enumerate() {
        let ref_i = si + 1;
        let (_, tbl) = ctx.refs[ref_i];
        let last = si + 1 == joins.len();
        // ON/USING were resolved against the layout up front.
        let cond_on: Option<&Expr> = on_res[si].as_ref();
        let cond_using: Option<&Expr> = using_res[si].as_ref();
        let cond_where: Option<&Expr> = if last { wexpr } else { None };

        // Hash-prune candidates when a top-level equality keys this step's
        // table by a value available from the already-joined prefix.
        let plan: Option<(Expr, usize)> = cond_on
            .and_then(|e| extract_hash_key(e, ctx, ref_i))
            .or_else(|| cond_where.and_then(|e| extract_hash_key(e, ctx, ref_i)));
        let index: Option<HashMap<Value, Vec<usize>>> = plan.as_ref().map(|(_, g)| {
            let mut m: HashMap<Value, Vec<usize>> = HashMap::new();
            for (ri, row) in tbl.rows.iter().enumerate() {
                let v = &row[g - ctx.offs[ref_i]];
                if !matches!(v, Value::Null) {
                    m.entry(v.clone()).or_default().push(ri);
                }
            }
            m
        });
        let all: Vec<usize> = (0..tbl.rows.len()).collect();

        let nprev = synth.rows.len();
        let mut kept: Vec<Vec<Value>> = Vec::new();
        let mut owned: Option<Vec<usize>> = None;
        for bi in 0..nprev {
            let cands: &[usize] = match (&index, &plan) {
                (Some(m), Some((kexpr, _))) => {
                    let kv = {
                        let cx = mk_ecx(outer, &bare, &quals, &synth);
                        eval(kexpr, &cx, bi)?
                    };
                    owned = Some(if matches!(kv, Value::Null) {
                        Vec::new()
                    } else {
                        m.get(&kv).cloned().unwrap_or_default()
                    });
                    owned.as_deref().unwrap()
                }
                _ => &all,
            };
            let mut any = false;
            for &ri in cands {
                let mut cand = synth.rows[bi].clone();
                for (j, v) in tbl.rows[ri].iter().enumerate() {
                    cand[ctx.offs[ref_i] + j] = v.clone();
                }
                synth.rows.push(cand);
                let at = synth.rows.len() - 1;
                // Evaluate the conditions with a context that borrows the
                // filled rows; the borrow ends before the next mutation.
                let (on_ok, using_ok, keep_row) = {
                    let cx = mk_ecx(outer, &bare, &quals, &synth);
                    let mut on_ok = true;
                    if let Some(e) = cond_on {
                        on_ok = is_true(&eval(e, &cx, at)?);
                    }
                    let mut using_ok = true;
                    if on_ok {
                        if let Some(e) = cond_using {
                            using_ok = is_true(&eval(e, &cx, at)?);
                        }
                    }
                    let mut keep_row = true;
                    if on_ok && using_ok {
                        if let Some(e) = cond_where {
                            keep_row = is_true(&eval(e, &cx, at)?);
                        }
                    }
                    (on_ok, using_ok, keep_row)
                };
                if on_ok && using_ok {
                    // ON/USING decide whether a LEFT row matched (and so
                    // needs padding); WHERE only filters the final row.
                    any = true;
                    if keep_row {
                        kept.push(synth.rows.pop().expect("candidate row"));
                    } else {
                        synth.rows.pop();
                    }
                } else {
                    synth.rows.pop();
                }
            }
            if step.kind == JoinKind::Left && !any {
                // Base row already carries NULLs for this table.
                synth.rows.push(synth.rows[bi].clone());
                let at = synth.rows.len() - 1;
                let ok = {
                    let cx = mk_ecx(outer, &bare, &quals, &synth);
                    let mut ok = true;
                    if let Some(e) = cond_where {
                        ok = is_true(&eval(e, &cx, at)?);
                    }
                    ok
                };
                if ok {
                    kept.push(synth.rows.pop().expect("padded row"));
                } else {
                    synth.rows.pop();
                }
            }
        }
        // Bases are consumed: the row set becomes exactly the kept rows.
        synth.rows.clear();
        synth.rows.append(&mut kept);
    }
    // Inner joins whose ON reaches tables further right are predicates over
    // the complete row; apply them once every table has been joined.
    if !deferred_on.is_empty() {
        let mut kept: Vec<Vec<Value>> = Vec::with_capacity(synth.rows.len());
        for i in 0..synth.rows.len() {
            let ok = {
                let cx = mk_ecx(outer, &bare, &quals, &synth);
                let mut ok = true;
                for e in deferred_on {
                    if !is_true(&eval(e, &cx, i)?) {
                        ok = false;
                        break;
                    }
                }
                ok
            };
            if ok {
                kept.push(synth.rows[i].clone());
            }
        }
        synth.rows = kept;
    }
    Ok(synth)
}

/// Expand `*` / `t.*` for one source table: display names as headers,
/// lookup names as expressions (and `USING` right-sides skipped).
fn push_star(proj: &mut Vec<Proj>, ctx: &JoinCtx, ref_i: usize) {
    let (_, tb) = ctx.refs[ref_i];
    for (j, (n, _)) in tb.cols.iter().enumerate() {
        if ctx.using_skip.contains(&(ref_i, n.clone())) {
            continue;
        }
        proj.push(Proj {
            expr: Expr::Col(ctx.synth_name(ref_i, j)),
            hdr: n.clone(),
        });
    }
}

/// Representative-row marker for a group with no source rows (a scalar
/// aggregate over an empty input still returns one row, and its bare
/// columns read as NULL).
const NO_ROW: usize = usize::MAX;

/// "1st", "2nd", "3rd", "4th" … — error-message text only (the suites
/// compare error classes, never messages).
fn ord_ordinal(n: usize) -> String {
    let suf = match n % 100 {
        11 | 12 | 13 => "th",
        _ => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{}{}", n, suf)
}

/// An ORDER BY word: an output alias wins over the bare column, like the
/// reference; anything else is `no_column`.
fn order_col_key(
    w: &str,
    aliases: &HashMap<String, usize>,
    t: Option<&Table>,
) -> R<OrdKey> {
    if let Some(&i) = aliases.get(w) {
        Ok(OrdKey::Item(i))
    } else if let Some(tt) = t {
        match tt.idx.get(w) {
            Some(&j) => Ok(OrdKey::TableCol(j)),
            None => Err(SqErr::no_column(w)),
        }
    } else {
        Err(SqErr::no_column(w))
    }
}

fn push_val(out: &mut String, v: &Value) {
    match v {
        Value::Int(i) => {
            let s = i.to_string();
            out.push_str(&s);
        }
        Value::Null => out.push_str("\\N"),
        Value::Text(t) => {
            for ch in t.chars() {
                match ch {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    _ => out.push(ch),
                }
            }
        }
    }
}

/// Prepare and run one SELECT, returning its headers and data rows.
/// `parent` is the chain of enclosing scopes for correlated subqueries.
/// Materialize a derived `FROM (SELECT ...)` source: the select's headers
/// become the column names, the value shapes its types.
fn derived_table(hdr: &[String], rows: Vec<Vec<Value>>) -> Table {
    let cols: Vec<(String, ColType)> = hdr
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let ct = rows
                .iter()
                .find_map(|r| match r.get(i) {
                    Some(Value::Int(_)) => Some(ColType::Int),
                    Some(Value::Text(_)) => Some(ColType::Text),
                    _ => None,
                })
                .unwrap_or(ColType::Text);
            (h.clone(), ct)
        })
        .collect();
    let mut t = Table::bare(cols);
    t.rows = rows;
    t
}

/// A WHERE conjunct-derived bound on one column: comparison code (0 Eq,
/// 1 Lt, 2 Le, 3 Gt, 4 Ge) plus the constant the other side evaluated to.
fn as_bound_const(e: &Expr) -> Option<Value> {
    match e {
        Expr::Lit(Lit::Int(i)) => Some(Value::Int(*i)),
        Expr::Lit(Lit::Str(s)) => Some(Value::Text(s.clone())),
        _ => None,
    }
}

fn col_ref_is(e: &Expr, col: &str) -> bool {
    matches!(e, Expr::Col(n) if n == col) || matches!(e, Expr::QCol(_, n) if n == col)
}

fn flip_cmp(op: u8) -> u8 {
    match op {
        1 => 3,
        3 => 1,
        2 => 4,
        4 => 2,
        o => o,
    }
}

/// Every top-level conjunct that bounds `col` against a literal. Only
/// `AND` contributes: anything under an `OR`/`NOT` may be false for a row
/// the conjunction still accepts, so it cannot restrict the scan.
fn collect_bnds(e: &Expr, col: &str, out: &mut Vec<(u8, Value)>) {
    match e {
        Expr::Bin(BinOp::And, l, r) => {
            collect_bnds(l, col, out);
            collect_bnds(r, col, out);
        }
        Expr::Bin(op, l, r) => {
            let code = match op {
                BinOp::Eq => 0u8,
                BinOp::Lt => 1,
                BinOp::Le => 2,
                BinOp::Gt => 3,
                BinOp::Ge => 4,
                _ => return,
            };
            if col_ref_is(l, col) {
                if let Some(v) = as_bound_const(r) {
                    out.push((code, v));
                }
            } else if col_ref_is(r, col) {
                if let Some(v) = as_bound_const(l) {
                    out.push((flip_cmp(code), v));
                }
            }
        }
        _ => {}
    }
}

fn tighten_lo(cur: Option<(Value, bool)>, v: Value, excl: bool) -> Option<(Value, bool)> {
    match cur {
        None => Some((v, excl)),
        Some((cv, ce)) => match cmp_val(&v, &cv) {
            Ordering::Greater => Some((v, excl)),
            Ordering::Equal if excl && !ce => Some((v, excl)),
            _ => Some((cv, ce)),
        },
    }
}

fn tighten_hi(cur: Option<(Value, bool)>, v: Value, excl: bool) -> Option<(Value, bool)> {
    match cur {
        None => Some((v, excl)),
        Some((cv, ce)) => match cmp_val(&v, &cv) {
            Ordering::Less => Some((v, excl)),
            Ordering::Equal if excl && !ce => Some((v, excl)),
            _ => Some((cv, ce)),
        },
    }
}

/// The contiguous entry range the bounds imply. Entries sort by the same
/// rank-ordered `cmp_val` the comparisons evaluate with, so every row
/// satisfying the conjunction lands inside — a superset the WHERE clause
/// then re-checks in full.
fn bnd_range(entries: &[(Value, i64, usize)], bnds: &[(u8, Value)]) -> (usize, usize) {
    let mut lo: Option<(Value, bool)> = None;
    let mut hi: Option<(Value, bool)> = None;
    for (op, v) in bnds {
        match op {
            0 => {
                lo = tighten_lo(lo, v.clone(), false);
                hi = tighten_hi(hi, v.clone(), false);
            }
            1 => hi = tighten_hi(hi, v.clone(), true),
            2 => hi = tighten_hi(hi, v.clone(), false),
            3 => lo = tighten_lo(lo, v.clone(), true),
            4 => lo = tighten_lo(lo, v.clone(), false),
            _ => {}
        }
    }
    let start = match &lo {
        None => 0,
        Some((v, false)) => entries.partition_point(|e| cmp_val(&e.0, v) == Ordering::Less),
        Some((v, true)) => entries.partition_point(|e| cmp_val(&e.0, v) != Ordering::Greater),
    };
    let mut end = match &hi {
        None => entries.len(),
        Some((v, false)) => entries.partition_point(|e| cmp_val(&e.0, v) != Ordering::Greater),
        Some((v, true)) => entries.partition_point(|e| cmp_val(&e.0, v) == Ordering::Less),
    };
    if end < start {
        end = start;
    }
    (start, end)
}

/// Can this expression fail while a row evaluates it? Subqueries run a
/// nested statement whose errors depend on the outer row, and unary minus
/// over a computed value can leave the dialect; everything else in the
/// row context is total. A WHERE clause of `safe` expressions cannot hide
/// an error by scanning fewer rows, which is what lets an index narrow
/// the scan at all.
fn eval_safe(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Col(_) | Expr::QCol(..) => true,
        Expr::Neg(x) => matches!(**x, Expr::Lit(_)),
        Expr::Not(x) => eval_safe(x),
        Expr::Bin(_, l, r) => eval_safe(l) && eval_safe(r),
        Expr::Func(_, args) => args.iter().all(eval_safe),
        Expr::InList { left, list, .. } => eval_safe(left) && list.iter().all(eval_safe),
        Expr::Agg { .. }
        | Expr::QStar(_)
        | Expr::Sub(_)
        | Expr::Exists { .. }
        | Expr::InSub { .. } => false,
    }
}

/// How a single-table SELECT gets its candidate rows.
enum ScanPlan {
    /// Every row, in physical order (the classic path).
    Full,
    /// An index range, walked in physical order through a bitmap: the
    /// answer is byte-identical to `Full` restricted to the same rows.
    Narrow {
        ix: usize,
        lo: usize,
        hi: usize,
    },
    /// An index range walked in the reference's index order — which for
    /// a single-column index *is* the final `ORDER BY` order, ties
    /// included, so the sort is skipped entirely.
    Ordered {
        ix: usize,
        lo: usize,
        hi: usize,
        desc: bool,
        nulls_first: bool,
    },
}

fn exec_select_data<'x>(
    db: &'x Db,
    st: &Stmt,
    parent: &[Frame<'x>],
) -> R<(Vec<String>, Vec<Vec<Value>>)> {
    let (distinct, items, from, joins, wexpr, group, having, order, limit) = match st {
        Stmt::Select {
            distinct,
            items,
            from,
            joins,
            wexpr,
            group,
            having,
            order,
            limit,
        } => (distinct, items, from, joins, wexpr, group, having, order, limit),
        _ => return Ok((Vec::new(), Vec::new())),
    };

    // Look the FROM tables up left to right (first miss is no_table, like
    // the reference) and build the query's column layout. A derived table
    // `FROM (SELECT ...)` runs first (in FROM order) and materializes into
    // an owned table the layout borrows.
    let mut refs_list: Vec<&TableRef> = Vec::new();
    if let Some(f) = &from {
        refs_list.push(f);
    }
    for j in joins {
        refs_list.push(&j.table);
    }
    let mut owned: Vec<Box<Table>> = Vec::new();
    for r in &refs_list {
        if let Some(d) = &r.derived {
            let (hdr, rows) = exec_select_data(db, d, parent)?;
            owned.push(Box::new(derived_table(&hdr, rows)));
        }
    }
    let mut di = 0usize;
    let mut trefs: Vec<(TableRef, &Table)> = Vec::new();
    for r in &refs_list {
        let tb: &Table = match &r.derived {
            Some(_) => {
                let t = &*owned[di];
                di += 1;
                t
            }
            None => db
                .tables
                .get(&r.name)
                .ok_or_else(|| SqErr::no_table(&r.name))?,
        };
        trefs.push(((*r).clone(), tb));
    }
    let mut ctx: JoinCtx = JoinCtx::build(trefs.iter().map(|(r, tb)| (r, *tb)).collect());
    for (si, js) in joins.iter().enumerate() {
        for c in &js.using {
            ctx.using_skip.insert((si + 1, c.clone()));
        }
    }

    // The synthetic structure (columns + idx, no rows) is the resolution
    // context for joins; a plain single-table FROM runs on the base table
    // itself, whose layout is identical.
    let synth: Table = ctx.structure();
    let t: Option<&Table> = if joins.is_empty() {
        ctx.refs.first().map(|r| r.1)
    } else {
        Some(&synth)
    };

    // The scope chain: this query first, then every enclosing scope.
    let cur_frame = match t {
        Some(tt) if joins.is_empty() => {
            let vis = from
                .as_ref()
                .map(|r| r.alias.clone().unwrap_or_else(|| r.name.clone()))
                .unwrap_or_default();
            Frame::of_table(tt, &[vis])
        }
        Some(tt) => frame_of_ctx(&ctx, tt),
        None => Frame::empty(),
    };
    let mut cx = Ecx {
        db,
        frames: {
            let mut f = Vec::with_capacity(parent.len() + 1);
            f.push(cur_frame);
            f.extend(parent.iter().cloned());
            f
        },
    };

    // Ticket 10, phase 1: every subquery of the statement is checked in
    // full (its own tables, names and nested subqueries) before any plain
    // name of this query is resolved — the reference reports subquery
    // errors ahead of ordinary column errors.
    for it in items {
        if let SelItem::E { expr, .. } = it {
            precheck_subs(expr, &cx)?;
        }
    }
    if let Some(w) = wexpr {
        precheck_subs(w, &cx)?;
    }
    for js in joins {
        if let Some(e) = &js.on {
            precheck_subs(e, &cx)?;
        }
    }
    if let Some(h) = having {
        precheck_subs(h, &cx)?;
    }
    for g in group {
        precheck_subs(g, &cx)?;
    }
    for spec in order {
        if let OrdKind::Expr(e) = &spec.kind {
            precheck_subs(e, &cx)?;
        }
    }
    if let Some(LimVal::Expr(e)) = &limit.limit {
        precheck_subs(e, &cx)?;
    }
    if let Some(LimVal::Expr(e)) = &limit.offset {
        precheck_subs(e, &cx)?;
    }

    // Build the projection (star expands across every joined table; `t.*`
    // expands one; other expressions are rewritten against the join layout
    // before resolution).
    let mut proj: Vec<Proj> = Vec::new();
    let mut alias_map: HashMap<String, usize> = HashMap::new();
    let mut sel_agg = false;
    // A star in the select list also feeds bare ORDER BY terms: they match
    // output headers (first occurrence), like the reference.
    let mut proj_star = false;
    for it in items {
        match it {
            SelItem::Star => {
                if ctx.refs.is_empty() {
                    return Err(SqErr::syn("no tables specified".to_string()));
                }
                proj_star = true;
                // Two sources may share a qualifier (a duplicated alias, or
                // `FROM t, t`): expanding a column both own is ambiguous.
                for (ref_i, (r, tb)) in ctx.refs.iter().enumerate() {
                    let q = r.alias.clone().unwrap_or_else(|| r.name.clone());
                    for (cn, _) in tb.cols.iter() {
                        if ctx.using_skip.contains(&(ref_i, cn.clone())) {
                            continue;
                        }
                        let clash = ctx.refs.iter().enumerate().any(|(k, (r2, tb2))| {
                            k != ref_i
                                && r2.alias.clone().unwrap_or_else(|| r2.name.clone()) == q
                                && tb2.idx.contains_key(cn)
                        });
                        if clash {
                            return Err(SqErr::new(
                                "ambiguous",
                                format!("ambiguous column name: main.{}.{}", q, cn),
                            ));
                        }
                    }
                }
                for ref_i in 0..ctx.refs.len() {
                    push_star(&mut proj, &ctx, ref_i);
                }
            }
            SelItem::E { expr, alias, raw } => {
                if let Expr::QStar(q) = expr {
                    proj_star = true;
                    let hits: Vec<usize> = ctx
                        .refs
                        .iter()
                        .enumerate()
                        .filter(|(_, (r, _))| r.matches(q))
                        .map(|(i, _)| i)
                        .collect();
                    match hits.len() {
                        0 => return Err(SqErr::no_table(q)),
                        1 => push_star(&mut proj, &ctx, hits[0]),
                        _ => {
                            return Err(SqErr::new(
                                "ambiguous",
                                format!("ambiguous table name: {}", q),
                            ))
                        }
                    }
                    continue;
                }
                // Name resolution happens up front, like the reference: an
                // unknown column is an error even on an empty table.
                let e2 = rewrite_join(expr, &ctx)?;
                resolve_expr(&e2, &cx, true)?;
                if has_agg(&e2) {
                    sel_agg = true;
                }
                let hdr = match alias {
                    Some(a) => {
                        alias_map
                            .entry(a.to_ascii_lowercase())
                            .or_insert(proj.len());
                        a.clone()
                    }
                    None => match expr {
                        Expr::Col(n) => n.clone(),
                        Expr::QCol(_, c) => c.clone(),
                        _ => raw.clone(),
                    },
                };
                proj.push(Proj { expr: e2, hdr });
            }
        }
    }
    let sel_mm = proj.iter().any(|p| has_minmax(&p.expr));

    let wexpr: Option<Expr> = match wexpr {
        Some(w) => Some(rewrite_join(w, &ctx)?),
        None => None,
    };
    if let Some(w) = &wexpr {
        resolve_expr(w, &cx, false)?;
    }

    // ON/USING resolve after WHERE and before HAVING/GROUP BY/ORDER BY,
    // like the reference. An ON that reaches tables further right is a
    // syntax error for a LEFT join (its padding would depend on rows not
    // yet joined); an inner join's such ON is a predicate over the
    // completed row and is applied after materialization instead.
    let mut on_res: Vec<Option<Expr>> = Vec::with_capacity(joins.len());
    let mut using_res: Vec<Option<Expr>> = Vec::with_capacity(joins.len());
    let mut deferred_on: Vec<Expr> = Vec::new();
    for (si, js) in joins.iter().enumerate() {
        let ref_i = si + 1;
        let mut on_here: Option<Expr> = None;
        if let Some(e) = &js.on {
            let e2 = rewrite_join(e, &ctx)?;
            resolve_expr(&e2, &cx, false)?;
            let mut seen: Vec<usize> = Vec::new();
            expr_ref_idxs(&e2, &ctx, &mut seen);
            if seen.iter().any(|&i| i > ref_i) {
                if js.kind == JoinKind::Left {
                    return Err(SqErr::syn(
                        "ON clause references tables to its right".to_string(),
                    ));
                }
                deferred_on.push(e2);
            } else {
                on_here = Some(e2);
            }
        }
        on_res.push(on_here);
        using_res.push(build_using(js, &ctx, ref_i)?);
    }

    let grouped = !group.is_empty();
    let agg_mode = grouped || sel_agg;

    // HAVING filters groups after aggregation. Without GROUP BY and
    // without an aggregate in the select list it is a syntax error —
    // checked before HAVING itself is resolved, like the reference.
    // HAVING is resolved before the GROUP BY keys are (the reference
    // reports a bad HAVING column over a bad GROUP BY key).
    let hsub: Option<Expr> = match having {
        Some(h) => {
            if !agg_mode {
                return Err(SqErr::syn(
                    "HAVING clause on a non-aggregate query".to_string(),
                ));
            }
            let h2 = subst_aliases(h, &alias_map, &proj);
            let h2 = rewrite_join(&h2, &ctx)?;
            resolve_expr(&h2, &cx, true)?;
            Some(h2)
        }
        None => None,
    };

    // GROUP BY keys. An integer literal is an ordinal into the select
    // list (out of range is a syntax error, like the reference's
    // "Nth GROUP BY term out of range"); aliases resolve to the aliased
    // expression; anything else stays as written. An aggregate in the key
    // is a syntax error.
    let mut gkeys: Vec<Expr> = Vec::with_capacity(group.len());
    for (ki, g) in group.iter().enumerate() {
        let e = match g {
            Expr::Lit(Lit::Int(i)) => {
                if *i < 1 || (*i as usize) > proj.len() {
                    return Err(SqErr::syn(format!(
                        "{} GROUP BY term out of range - should be between 1 and {}",
                        ord_ordinal(ki + 1),
                        proj.len()
                    )));
                }
                proj[*i as usize - 1].expr.clone()
            }
            other => subst_aliases(other, &alias_map, &proj),
        };
        let e = rewrite_join(&e, &ctx)?;
        resolve_expr(&e, &cx, false)?;
        gkeys.push(e);
    }

    // Resolve ORDER BY terms.
    let mut terms: Vec<(OrdKey, bool, Option<bool>)> = Vec::new();
    for spec in order {
        let key = match &spec.kind {
            OrdKind::Ordinal(l) => match l {
                Lit::Int(i) => {
                    if *i >= 1 && (*i as usize) <= proj.len() {
                        OrdKey::Item(*i as usize - 1)
                    } else {
                        return Err(SqErr::syn(format!(
                            "ORDER BY term out of range - should be between 1 and {}",
                            proj.len()
                        )));
                    }
                }
                Lit::Big { .. } => {
                    return Err(SqErr::syn(format!(
                        "ORDER BY term out of range - should be between 1 and {}",
                        proj.len()
                    )))
                }
                _ => {
                    return Err(SqErr::syn(format!(
                        "ORDER BY term out of range - should be between 1 and {}",
                        proj.len()
                    )))
                }
            },
            OrdKind::Word(w) => order_col_key(w, &alias_map, t)?,
            OrdKind::Const(l) => OrdKey::Const(lit_value(l)),
            OrdKind::Expr(e) => match e {
                // An output alias wins over a bare column, like the
                // reference — checked before the join ambiguity pass.
                Expr::Col(w) if alias_map.contains_key(w) => {
                    OrdKey::Item(alias_map[w])
                }
                // With a star in the select list a bare name matches an
                // output header (first occurrence), even when several
                // sources share the column name.
                Expr::Col(w)
                    if proj_star
                        && proj
                            .iter()
                            .position(|p| p.hdr.as_str() == w.as_str())
                            .is_some() =>
                {
                    OrdKey::Item(
                        proj.iter()
                            .position(|p| p.hdr.as_str() == w.as_str())
                            .unwrap(),
                    )
                }
                other => {
                    let s = subst_aliases(other, &alias_map, &proj);
                    let s = rewrite_join(&s, &ctx)?;
                    match s {
                        Expr::Col(w2) => order_col_key(&w2, &alias_map, t)?,
                        _ => {
                            // Aggregates in ORDER BY are legal on aggregate
                            // queries (and on a table-less SELECT, where the
                            // reference evaluates them over the implicit
                            // row); otherwise they are a misuse, like in
                            // WHERE.
                            resolve_expr(&s, &cx, agg_mode || cx.frames[0].tbl.is_none())?;
                            OrdKey::Expr(s)
                        }
                    }
                }
            },
        };
        terms.push((key, spec.asc, spec.nulls_first));
    }

    // Ticket 10, phase 3: subquery column counts are validated after every
    // plain name of the statement resolved (the reference checks them late
    // and position-dependently: `arity` in bare positions, `row value
    // misused` directly under a comparison).
    for it in items {
        if let SelItem::E { expr, .. } = it {
            check_subs_width(expr, &cx, SubCtx::Arity)?;
        }
    }
    if let Some(w) = &wexpr {
        check_subs_width(w, &cx, SubCtx::Arity)?;
    }
    for js in joins {
        if let Some(e) = &js.on {
            check_subs_width(e, &cx, SubCtx::Arity)?;
        }
    }
    if let Some(h) = having {
        check_subs_width(h, &cx, SubCtx::Arity)?;
    }
    for g in group {
        check_subs_width(g, &cx, SubCtx::Arity)?;
    }
    for spec in order {
        if let OrdKind::Expr(e) = &spec.kind {
            check_subs_width(e, &cx, SubCtx::Arity)?;
        }
    }
    if let Some(LimVal::Expr(e)) = &limit.limit {
        check_subs_width(e, &cx, SubCtx::Arity)?;
    }
    if let Some(LimVal::Expr(e)) = &limit.offset {
        check_subs_width(e, &cx, SubCtx::Arity)?;
    }

    // Materialize the join into one synthetic table (WHERE runs inside the
    // nested loop, so an over-large cross product never materializes); a
    // plain FROM needs no work. A fresh structure feeds materialize so the
    // resolution-time one `cx` still points at stays valid.
    let synth_owned: Option<Table> = if joins.is_empty() {
        None
    } else {
        Some(materialize(
            ctx.structure(),
            &ctx,
            &joins,
            &on_res,
            &using_res,
            wexpr.as_ref(),
            &deferred_on,
            &cx,
        )?)
    };
    let t: Option<&Table> = if joins.is_empty() {
        ctx.refs.first().map(|r| r.1)
    } else {
        Some(synth_owned.as_ref().expect("join synth"))
    };
    // Evaluation now runs against the filled table (identical layout).
    cx.frames[0].tbl = t;

    // Candidate source rows: every row of the table, or the single implicit
    // row of a table-less SELECT; WHERE drops rows before output. An index
    // may narrow the scan to a superset of the predicate's matches (the
    // WHERE clause still runs over every candidate, in physical order) or,
    // when it can produce the query's exact ORDER BY order, supply that
    // order directly.
    let base: usize = match t {
        Some(tt) => tt.rows.len(),
        None => 1,
    };
    let where_ok = wexpr.as_ref().map_or(true, eval_safe);
    let mut plan = ScanPlan::Full;
    if joins.is_empty() && !agg_mode && !distinct && where_ok {
        if let Some(tt) = t {
            if !tt.indexes.is_empty() {
                // ORDER BY fast path: one term reading exactly one
                // indexed column, in the direction the reference walks
                // its index (a descending term reverses everything,
                // ties included).
                let mut ordered: Option<(usize, usize, usize, bool, bool)> = None;
                if terms.len() == 1 {
                    let (key, asc, nf) = &terms[0];
                    let ocol = match key {
                        OrdKey::TableCol(j) => Some(*j),
                        OrdKey::Item(i) => match proj.get(*i).map(|p| &p.expr) {
                            Some(Expr::Col(n)) => tt.idx.get(n).copied(),
                            Some(Expr::QCol(_, n)) => tt.idx.get(n).copied(),
                            _ => None,
                        },
                        _ => None,
                    };
                    let dir_ok = (*asc && nf.map_or(true, |f| f))
                        || (!*asc && nf.map_or(true, |f| !f));
                    if let (Some(c), true) = (ocol, dir_ok) {
                        if let Some(ii) = tt
                            .indexes
                            .iter()
                            .position(|x| x.cols.len() == 1 && x.cols[0] == c)
                        {
                            let mut b: Vec<(u8, Value)> = Vec::new();
                            if let Some(w) = &wexpr {
                                collect_bnds(w, &tt.cols[c].0, &mut b);
                            }
                            let (lo, hi) = bnd_range(&tt.indexes[ii].entries, &b);
                            let desc = !*asc;
                            let nulls_first = if desc {
                                nf.unwrap_or(false)
                            } else {
                                nf.unwrap_or(true)
                            };
                            ordered = Some((ii, lo, hi, desc, nulls_first));
                        }
                    }
                }
                if let Some((ix, lo, hi, desc, nulls_first)) = ordered {
                    plan = ScanPlan::Ordered {
                        ix,
                        lo,
                        hi,
                        desc,
                        nulls_first,
                    };
                } else {
                    // Plain narrowing: the index whose bounds cut the
                    // most rows. A full-range cut is the classic scan.
                    let mut best: Option<(usize, usize, usize)> = None;
                    for (ii, ix) in tt.indexes.iter().enumerate() {
                        let mut b: Vec<(u8, Value)> = Vec::new();
                        if let Some(w) = &wexpr {
                            collect_bnds(w, &tt.cols[ix.cols[0]].0, &mut b);
                        }
                        if b.is_empty() {
                            continue;
                        }
                        let (lo, hi) = bnd_range(&ix.entries, &b);
                        if lo == 0 && hi == tt.indexes[ii].entries.len() {
                            continue;
                        }
                        if best.as_ref().map_or(true, |(_, _, s)| hi - lo < *s) {
                            best = Some((ii, lo, hi));
                        }
                    }
                    if let Some((ix, lo, hi)) = best {
                        plan = ScanPlan::Narrow { ix, lo, hi };
                    }
                }
            }
        }
    }

    // A literal LIMIT/OFFSET can be read up front (it cannot error) and,
    // when the final order is already the candidate order and no later
    // stage could reject a row, lets the scan stop once enough rows
    // survived — the projection of later rows cannot change an answer
    // that never reaches them.
    let mut early: Option<usize> = None;
    let proj_ok = proj.iter().all(|p| eval_safe(&p.expr));
    let order_final = match &plan {
        ScanPlan::Ordered { .. } => true,
        // Without ORDER BY the output is the candidate order, but a
        // rowid re-base below would reshuffle it — a prefix taken now
        // would not be the final prefix.
        ScanPlan::Narrow { .. } | ScanPlan::Full => {
            terms.is_empty()
                && t.map_or(true, |tt| tt.rowid.is_none() || tt.seq_scan)
        }
    };
    if proj_ok && !agg_mode && !distinct && order_final {
        let off_lit = match &limit.offset {
            None => Some(0i64),
            Some(LimVal::Expr(Expr::Lit(Lit::Int(o)))) => Some(*o),
            _ => None,
        };
        if let (Some(LimVal::Expr(Expr::Lit(Lit::Int(l)))), Some(offv)) =
            (&limit.limit, off_lit)
        {
            if *l >= 0 {
                early = Some(offv.max(0) as usize + *l as usize);
            }
        }
    }

    let mut keep: Vec<usize> = Vec::with_capacity(base);
    match &plan {
        ScanPlan::Full => {
            for r in 0..base {
                if let Some(n) = early {
                    if keep.len() >= n {
                        break;
                    }
                }
                if let Some(w) = &wexpr {
                    let v = eval(w, &cx, r)?;
                    if !is_true(&v) {
                        continue;
                    }
                }
                keep.push(r);
            }
        }
        ScanPlan::Narrow { ix, lo, hi } => {
            let tt = t.expect("narrowed scan of a table");
            let mut bits: Vec<u64> = vec![0u64; (base + 63) / 64];
            for e in &tt.indexes[*ix].entries[*lo..*hi] {
                bits[e.2 >> 6] |= 1u64 << (e.2 & 63);
            }
            for r in 0..base {
                if bits[r >> 6] & (1u64 << (r & 63)) == 0 {
                    continue;
                }
                if let Some(w) = &wexpr {
                    let v = eval(w, &cx, r)?;
                    if !is_true(&v) {
                        continue;
                    }
                }
                keep.push(r);
            }
        }
        ScanPlan::Ordered {
            ix,
            lo,
            hi,
            desc,
            nulls_first,
        } => {
            let tt = t.expect("ordered scan of a table");
            let entries = &tt.indexes[*ix].entries;
            let nn = entries.partition_point(|e| matches!(e.0, Value::Null));
            // NULLs sort as one group (rank 0: a prefix of the entries);
            // the term's nulls placement puts it first or last, and a
            // descending term walks everything backwards — value groups
            // reversed, and within a group the rowid order reversed too,
            // exactly like the reference's reversed index scan.
            let (ns, ne) = (*lo, (*hi).min(nn));
            let (xs, xe) = ((*lo).max(nn), *hi);
            let segs: [(usize, usize, bool); 2] = if !desc {
                if *nulls_first {
                    [(ns, ne, false), (xs, xe, false)]
                } else {
                    [(xs, xe, false), (ns, ne, false)]
                }
            } else if *nulls_first {
                [(ns, ne, true), (xs, xe, true)]
            } else {
                [(xs, xe, true), (ns, ne, true)]
            };
            'segs: for (s, e, rev) in segs {
                if s >= e {
                    continue;
                }
                if rev {
                    for pos in entries[s..e].iter().rev().map(|x| x.2) {
                        if let Some(n) = early {
                            if keep.len() >= n {
                                break 'segs;
                            }
                        }
                        if let Some(w) = &wexpr {
                            if !is_true(&eval(w, &cx, pos)?) {
                                continue;
                            }
                        }
                        keep.push(pos);
                    }
                } else {
                    for pos in entries[s..e].iter().map(|x| x.2) {
                        if let Some(n) = early {
                            if keep.len() >= n {
                                break 'segs;
                            }
                        }
                        if let Some(w) = &wexpr {
                            if !is_true(&eval(w, &cx, pos)?) {
                                continue;
                            }
                        }
                        keep.push(pos);
                    }
                }
            }
        }
    }

    // The reference scans a rowid table in rowid order; when explicit
    // rowids left physical order behind, re-base the survivors before
    // anything downstream (projection, DISTINCT, group representatives,
    // LIMIT without ORDER BY, sort ties) sees them. A subsequence of a
    // rowid-ordered table is still rowid-ordered, so the flag is enough.
    if !matches!(plan, ScanPlan::Ordered { .. }) {
        if let Some(tt) = t {
            if tt.rowid.is_some() && !tt.seq_scan {
                let rid = tt.rowid.unwrap();
                keep.sort_by(|&a, &b| match (&tt.rows[a][rid], &tt.rows[b][rid]) {
                    (Value::Int(x), Value::Int(y)) => x.cmp(y),
                    _ => Ordering::Equal,
                });
            }
        }
    }

    // Materialize projected rows: one per surviving source row on a plain
    // query, one per group on an aggregate query. `groups`/`gidx` map each
    // output row back to its group's source rows (aggregate queries only),
    // and `keep` holds each output row's representative source row —
    // NO_ROW when the group is empty.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut gidx: Vec<usize> = Vec::new();
    let mut prows: Vec<Vec<Value>> = Vec::with_capacity(keep.len());
    if !agg_mode {
        // A plain column projection resolves once against the current
        // frame (the same first lookup `eval_col` does), so the hot loop
        // reads the row directly instead of re-walking the expression.
        let pf: Vec<Option<usize>> = proj
            .iter()
            .map(|p| match &p.expr {
                Expr::Col(n) => cx.frames[0].tbl.and_then(|t| t.idx.get(n).copied()),
                _ => None,
            })
            .collect();
        let ftab = cx.frames[0].tbl;
        for &r in &keep {
            let mut row: Vec<Value> = Vec::with_capacity(proj.len());
            for (pi, p) in proj.iter().enumerate() {
                match (pf[pi], ftab) {
                    (Some(ci), Some(ft)) => row.push(ft.rows[r][ci].clone()),
                    _ => row.push(eval(&p.expr, &cx, r)?),
                }
            }
            prows.push(row);
        }
    } else {
        if grouped {
            // One group per distinct key value; NULL is one group, not one
            // per row. Groups are emitted sorted by key: the reference
            // sorts rows by the group keys before aggregating, so answers
            // with tied ORDER BY terms follow key order.
            let mut index: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
            for r in std::mem::take(&mut keep) {
                let mut key: Vec<Value> = Vec::with_capacity(gkeys.len());
                for g in &gkeys {
                    key.push(eval(g, &cx, r)?);
                }
                index.entry(key).or_default().push(r);
            }
            let mut packed: Vec<(Vec<Value>, Vec<usize>)> = index.into_iter().collect();
            packed.sort_by(|(ka, _), (kb, _)| cmp_key_tuple(ka, kb));
            groups = packed.into_iter().map(|(_, rs)| rs).collect();
        } else {
            // Scalar aggregate: exactly one group over everything that
            // survived WHERE — even when nothing did.
            groups.push(std::mem::take(&mut keep));
        }
        for (gi, rows) in groups.iter().enumerate() {
            // The representative row feeds the group's bare columns: the
            // last MIN/MAX winner, else the first row, like the reference.
            let rep: Option<usize> = if rows.is_empty() {
                None
            } else if sel_mm {
                let mut slot: Option<usize> = None;
                for p in &proj {
                    scan_rep(&p.expr, &cx, rows, &mut slot);
                }
                slot.or(Some(rows[0]))
            } else {
                Some(rows[0])
            };
            if let Some(h) = &hsub {
                if !is_true(&eval_group(h, &cx, rows, rep)?) {
                    continue;
                }
            }
            let mut row: Vec<Value> = Vec::with_capacity(proj.len());
            for p in &proj {
                row.push(eval_group(&p.expr, &cx, rows, rep)?);
            }
            prows.push(row);
            keep.push(rep.unwrap_or(NO_ROW));
            gidx.push(gi);
        }
    }

    // SELECT DISTINCT: keep the first copy of each projected row; NULLs are
    // equal to each other (Value::Null == Value::Null). Runs before ORDER BY
    // and LIMIT, matching the reference; the parallel `keep`/`gidx` stay in
    // sync so sort keys still point at the right row or group.
    if *distinct {
        let mut seen: HashSet<&Vec<Value>> = HashSet::with_capacity(prows.len());
        let mut keep2: Vec<usize> = Vec::with_capacity(keep.len());
        let mut prows2: Vec<Vec<Value>> = Vec::with_capacity(prows.len());
        let mut gidx2: Vec<usize> = Vec::with_capacity(gidx.len());
        for (i, row) in prows.iter().enumerate() {
            if seen.insert(row) {
                keep2.push(keep[i]);
                prows2.push(row.clone());
                if i < gidx.len() {
                    gidx2.push(gidx[i]);
                }
            }
        }
        keep = keep2;
        prows = prows2;
        gidx = gidx2;
    }

    // Evaluate ORDER BY expressions once per output row: in group context
    // on aggregate queries, in row context otherwise (a table-less SELECT
    // may sort by an aggregate over its implicit rows).
    let mut evals: Vec<Vec<Value>> = Vec::with_capacity(terms.len());
    for (key, _, _) in terms.iter() {
        match key {
            OrdKey::Expr(e) => {
                let mut vals: Vec<Value> = Vec::with_capacity(prows.len());
                for i in 0..prows.len() {
                    let v = if agg_mode {
                        let rep = match keep[i] {
                            NO_ROW => None,
                            r => Some(r),
                        };
                        eval_group(e, &cx, &groups[gidx[i]], rep)?
                    } else if t.is_some() {
                        eval(e, &cx, keep[i])?
                    } else {
                        let rep = keep.first().copied().filter(|&r| r != NO_ROW);
                        eval_group(e, &cx, &keep, rep)?
                    };
                    vals.push(v);
                }
                evals.push(vals);
            }
            _ => evals.push(Vec::new()),
        }
    }

    // Sort row indices (stable: ties keep insertion order, like the
    // reference). An index-ordered scan is already in its final order —
    // the reference's index order — so the sort is skipped outright.
    let mut ord: Vec<usize> = (0..prows.len()).collect();
    if !terms.is_empty() && !matches!(plan, ScanPlan::Ordered { .. }) {
        ord.sort_by(|&a, &b| {
            for (ti, (key, asc, nf)) in terms.iter().enumerate() {
                let (va, vb): (&Value, &Value) = match key {
                    OrdKey::Item(i) => (&prows[a][*i], &prows[b][*i]),
                    OrdKey::TableCol(j) => match t {
                        Some(tt) => {
                            // NO_ROW: the group was empty, its bare column
                            // reads as NULL.
                            let va = if keep[a] == NO_ROW {
                                &Value::Null
                            } else {
                                &tt.rows[keep[a]][*j]
                            };
                            let vb = if keep[b] == NO_ROW {
                                &Value::Null
                            } else {
                                &tt.rows[keep[b]][*j]
                            };
                            (va, vb)
                        }
                        None => (&Value::Null, &Value::Null),
                    },
                    OrdKey::Const(v) => (v, v),
                    // SELECT never produces the DML-only rowid key.
                    OrdKey::RowIdx => (&Value::Null, &Value::Null),
                    OrdKey::Expr(_) => (&evals[ti][a], &evals[ti][b]),
                };
                let c = cmp_term(va, vb, *asc, *nf);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }

    // Resolve LIMIT / OFFSET. The expression sees no table (a column
    // reference is `no_column`), and the value must be an integer.
    let mut lim: i64 = -1;
    let mut off: i64 = 0;
    if let Some(v) = &limit.limit {
        lim = lim_value(v, &cx)?;
    }
    if let Some(v) = &limit.offset {
        off = lim_value(v, &cx)?;
    }

    let total = ord.len();
    let s = (off.max(0) as usize).min(total);
    let e = if lim < 0 {
        total
    } else {
        s.saturating_add(lim as usize).min(total)
    };

    let hdrs: Vec<String> = proj.iter().map(|p| p.hdr.clone()).collect();
    let out_rows: Vec<Vec<Value>> = ord[s..e].iter().map(|&ri| prows[ri].clone()).collect();
    Ok((hdrs, out_rows))
}

/// The wire-format answer for a SELECT.
fn exec_select<'x>(db: &'x Db, st: &Stmt, parent: &[Frame<'x>]) -> R<String> {
    let (hdrs, rows) = exec_select_data(db, st, parent)?;
    let mut out = String::new();
    out.push_str(&format!("ROWS {}\n", hdrs.len()));
    for (i, h) in hdrs.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        out.push_str(h);
    }
    out.push('\n');
    for row in &rows {
        for (i, v) in row.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            push_val(&mut out, v);
        }
        out.push('\n');
    }
    out.push_str(&format!("END {}\n", rows.len()));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Server plumbing
// ---------------------------------------------------------------------------

struct Server {
    db: RwLock<Db>,
    /// The write gate: `true` while some connection holds exclusive write
    /// ownership — an autocommit statement for the duration of its own
    /// execution, a transaction from its first write until COMMIT,
    /// ROLLBACK or disconnect. Writers wait on `gcv`; readers never do
    /// (the reference lets readers straight past an open transaction).
    gate: Mutex<bool>,
    gcv: Condvar,
}

fn srv_read<T>(srv: &Server, f: impl FnOnce(&Db) -> T) -> T {
    match srv.db.read() {
        Ok(g) => f(&g),
        Err(e) => {
            let g = e.into_inner();
            f(&g)
        }
    }
}

fn srv_write<T>(srv: &Server, f: impl FnOnce(&mut Db) -> T) -> T {
    match srv.db.write() {
        Ok(g) => {
            let mut g = g;
            f(&mut g)
        }
        Err(e) => {
            let mut g = e.into_inner();
            f(&mut g)
        }
    }
}

fn trim_ws(s: &str) -> &str {
    s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r')
}

// ---------------------------------------------------------------------------
// Transactions (ticket 6): one per connection, deferred until the first
// write, private until COMMIT.
// ---------------------------------------------------------------------------

/// Blocking hold on the write gate; released on drop (panic- and
/// disconnect-safe).
struct GateGuard {
    srv: Arc<Server>,
}

impl GateGuard {
    fn acquire(srv: &Arc<Server>) -> GateGuard {
        let mut g = match srv.gate.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        while *g {
            g = match srv.gcv.wait(g) {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
        }
        *g = true;
        GateGuard { srv: Arc::clone(srv) }
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        let mut g = match self.srv.gate.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        *g = false;
        self.srv.gcv.notify_all();
    }
}

/// Per-connection transaction state. `active` follows BEGIN until COMMIT,
/// ROLLBACK or the connection going away; `db` is the private copy of the
/// store, created at the transaction's first write while the gate is held,
/// so reads before that point see the latest committed state exactly like
/// the reference's deferred transactions. `stmts` collects the statements
/// that succeeded, so COMMIT can persist them as one atomic log record.
struct ConnTxn {
    active: bool,
    db: Option<Db>,
    stmts: Vec<String>,
    guard: Option<GateGuard>,
}

impl ConnTxn {
    fn new() -> ConnTxn {
        ConnTxn {
            active: false,
            db: None,
            stmts: Vec::new(),
            guard: None,
        }
    }

    /// Abandon the transaction: the private copy is dropped and the write
    /// gate (if held) is released, waking every blocked writer.
    fn end(&mut self) {
        self.active = false;
        self.db = None;
        self.stmts.clear();
        self.guard = None;
    }
}

/// Process one (newline-free) request line into its response, or "" for none.
fn process(srv: &Arc<Server>, raw: &[u8], txn: &mut ConnTxn) -> String {
    if raw.contains(&0) {
        return "ERR syntax query contains a null character\n".to_string();
    }
    let s = match std::str::from_utf8(raw) {
        Ok(s) => s,
        Err(_) => return "ERR syntax invalid utf-8\n".to_string(),
    };

    // Trim whitespace and leading/trailing semicolons.
    let mut body = trim_ws(s);
    let mut had_semi = false;
    loop {
        if let Some(rest) = body.strip_prefix(';') {
            body = trim_ws(rest);
            had_semi = true;
            continue;
        }
        if body.ends_with(';') {
            body = trim_ws(&body[..body.len() - 1]);
            had_semi = true;
            continue;
        }
        break;
    }
    if body.is_empty() {
        // Whitespace-only lines get no answer; bare ";" answers OK 0.
        return if had_semi {
            "OK 0\n".to_string()
        } else {
            String::new()
        };
    }

    // Name resolution sees the transaction's own view of the store once it
    // has written, and the latest committed state until then.
    let parsed = if let Some(w) = txn.db.as_ref() {
        parse(body, &|n: &str| name_hit(&w.tables, n))
    } else {
        parse(body, &|n: &str| srv_read(srv, |db| name_hit(&db.tables, n)))
    };
    let stmt = match parsed {
        Ok(st) => st,
        Err(e) => return line_err(&e),
    };

    match &stmt {
        // --- transaction control (ticket 6) ---
        Stmt::TxnBegin => {
            if txn.active {
                return line_err(&SqErr::new(
                    "txn",
                    "cannot start a transaction within a transaction".to_string(),
                ));
            }
            txn.active = true;
            return "OK 0\n".to_string();
        }
        Stmt::TxnCommit => {
            if !txn.active {
                return line_err(&SqErr::new(
                    "txn",
                    "cannot commit - no transaction is active".to_string(),
                ));
            }
            // Nothing succeeded inside the transaction: nothing changed and
            // nothing is logged.
            if txn.stmts.is_empty() {
                txn.end();
                return "OK 0\n".to_string();
            }
            let res: R<()> = match txn.db.as_mut() {
                None => Ok(()),
                Some(working) => srv_write(srv, |db| {
                    // One framed, checksummed record holding every
                    // statement of the transaction: the fsync inside
                    // append happens before the answer, and a torn or
                    // missing record drops the whole transaction — it is
                    // never applied halfway.
                    let payload = txn.stmts.join("\n");
                    let k = txn.stmts.len();
                    {
                        let w = db
                            .wal
                            .as_mut()
                            .ok_or_else(|| SqErr::new("io", "log unavailable".to_string()))?;
                        w.append(KIND_STMT, payload.as_bytes())
                            .map_err(|e| SqErr::new("io", format!("log write failed: {}", e)))?;
                        if k > 1 {
                            w.stmts_since_snap += k as u64 - 1;
                        }
                    }
                    db.tables = std::mem::take(&mut working.tables);
                    // Same snapshot cadence as autocommit statements.
                    let due = db.wal.as_ref().map(|w| w.snapshot_due()).unwrap_or(false);
                    if due {
                        let blob = serialize_tables(&db.tables);
                        let w = db.wal.as_mut().expect("wal present");
                        w.append(KIND_SNAP, &blob)
                            .map_err(|e| SqErr::new("io", format!("log write failed: {}", e)))?;
                        w.reset_snapshot(blob.len() as u64);
                    }
                    Ok(())
                }),
            };
            return match res {
                Ok(()) => {
                    txn.end();
                    "OK 0\n".to_string()
                }
                Err(e) => line_err(&e),
            };
        }
        Stmt::TxnRollback => {
            if !txn.active {
                return line_err(&SqErr::new(
                    "txn",
                    "cannot rollback - no transaction is active".to_string(),
                ));
            }
            txn.end();
            return "OK 0\n".to_string();
        }
        Stmt::Select { .. } => {
            let res = match txn.db.as_ref() {
                Some(w) => exec_select(w, &stmt, &[]),
                None => srv_read(srv, |db| exec_select(db, &stmt, &[])),
            };
            return match res {
                Ok(s) => s,
                Err(e) => line_err(&e),
            };
        }
        _ => {}
    }

    // DML: inside a transaction it lands in the transaction's private copy
    // and is logged only at COMMIT; outside it is an autocommit statement
    // logged immediately. Either way the write gate serializes writers —
    // but a plain read-only transaction never takes it, matching the
    // reference's deferred locking.
    let res = if txn.active {
        if txn.db.is_none() {
            let g = GateGuard::acquire(srv);
            let copy = srv_read(srv, |db| Db {
                tables: db.tables.clone(),
                wal: None,
            });
            txn.db = Some(copy);
            txn.guard = Some(g);
        }
        let w = txn.db.as_mut().expect("working copy");
        let r = exec_dml(w, &stmt, false, body);
        if r.is_ok() {
            // Only statements that took effect are part of the commit.
            txn.stmts.push(body.to_string());
        }
        r
    } else {
        let g = GateGuard::acquire(srv);
        let res = srv_write(srv, |db| exec_dml(db, &stmt, true, body));
        drop(g);
        res
    };
    match res {
        Ok(s) => s,
        Err(e) => line_err(&e),
    }
}

fn safe_process(srv: &Arc<Server>, raw: &[u8], txn: &mut ConnTxn) -> String {
    match std::panic::catch_unwind(AssertUnwindSafe(|| process(srv, raw, txn))) {
        Ok(s) => s,
        Err(_) => "ERR syntax internal error\n".to_string(),
    }
}

fn handle(mut stream: TcpStream, srv: Arc<Server>) {
    let _ = stream.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    // The connection's transaction lives here: dropped (and its write gate
    // released) whenever the connection goes away, so a client that dies
    // mid-transaction rolls back and unblocks waiting writers, exactly like
    // the reference.
    let mut txn = ConnTxn::new();
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_LINE {
            return;
        }
        loop {
            let pos = match buf.iter().position(|&b| b == b'\n') {
                Some(p) => p,
                None => break,
            };
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let resp = safe_process(&srv, &line[..pos], &mut txn);
            if !resp.is_empty() && stream.write_all(resp.as_bytes()).is_err() {
                return;
            }
        }
    }
    // EOF with an unterminated line: the reference still answers it.
    if !buf.is_empty() {
        let resp = safe_process(&srv, &buf, &mut txn);
        if !resp.is_empty() {
            let _ = stream.write_all(resp.as_bytes());
        }
    }
}

// ---------------------------------------------------------------------------
// Startup: args, log replay
// ---------------------------------------------------------------------------

fn parse_args() -> (u16, String) {
    let mut port: Option<u16> = None;
    let mut data: Option<String> = None;
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--port" {
            port = it.next().and_then(|v| v.parse::<u16>().ok());
        } else if a == "--data" {
            data = it.next();
        } else if let Some(v) = a.strip_prefix("--port=") {
            port = v.parse::<u16>().ok();
        } else if let Some(v) = a.strip_prefix("--data=") {
            data = Some(v.to_string());
        }
    }
    let port = port
        .or_else(|| env::var("SDB_PORT").ok().and_then(|v| v.parse::<u16>().ok()))
        .unwrap_or(5580);
    let data = data
        .or_else(|| env::var("SDB_DATA_DIR").ok())
        .unwrap_or_else(|| "./data".to_string());
    (port, data)
}

struct ScanInfo {
    rewrite: bool,
    seq: u64,
}

/// Walk the framed file record by record, validating magic, header
/// checksum, record length and payload checksum. A trailing partial record
/// (a torn tail) makes the walk stop before that record is considered, so it
/// is simply absent. Then apply the newest snapshot record and every
/// statement record written after it.
fn scan_framed(file: &File, len: u64, db: &mut Db) -> ScanInfo {
    let mut off = 0u64;
    let mut clean = true; // reached EOF exactly on a record boundary
    let mut saw_stmt = false;
    let mut snap_count = 0u32;
    let mut seq = 0u64;
    let mut snap: Option<(u64, u32, u32)> = None; // payload offset, len, crc
    let mut stmts: Vec<(u64, u32, u32)> = Vec::new();

    while off < len {
        if len - off < REC_HDR as u64 {
            clean = false;
            break;
        }
        let mut h = [0u8; REC_HDR];
        if read_exact_at(file, &mut h, off).is_err() {
            clean = false;
            break;
        }
        if h[0..4] != REC_MAGIC {
            clean = false;
            break;
        }
        let hcrc = u32::from_le_bytes(h[24..28].try_into().unwrap());
        if crc32(&h[0..24]) != hcrc {
            clean = false;
            break;
        }
        let kind = h[4];
        if kind > KIND_SNAP {
            clean = false;
            break;
        }
        let plen = u32::from_le_bytes(h[16..20].try_into().unwrap());
        let total = rec_total(plen as usize) as u64;
        if off + total > len {
            // Torn trailing record: treat it as if it was never written.
            clean = false;
            break;
        }
        let pcrc = u32::from_le_bytes(h[20..24].try_into().unwrap());
        let s = u64::from_le_bytes(h[8..16].try_into().unwrap());
        let poff = off + REC_HDR as u64;
        if kind == KIND_SNAP {
            snap = Some((poff, plen, pcrc));
            snap_count += 1;
            stmts.clear(); // superseded by this snapshot
        } else {
            stmts.push((poff, plen, pcrc));
            saw_stmt = true;
        }
        if s > seq {
            seq = s;
        }
        off += total;
    }

    if let Some((po, pl, pc)) = snap {
        let mut p = vec![0u8; pl as usize];
        if read_exact_at(file, &mut p, po).is_ok() && crc32(&p) == pc {
            if let Ok(t) = deserialize_tables(&p) {
                db.tables = t;
            }
        }
    }
    for &(po, pl, pc) in &stmts {
        let mut p = vec![0u8; pl as usize];
        if read_exact_at(file, &mut p, po).is_err() || crc32(&p) != pc {
            break;
        }
        let s = match std::str::from_utf8(&p) {
            Ok(s) => s,
            Err(_) => break,
        };
        if trim_ws(s).is_empty() {
            continue;
        }
        // A record holds one statement, or a whole committed transaction
        // (its statements joined by newlines): they replay in order against
        // the same state they ran against, so they cannot half-apply.
        let mut bad = false;
        for line in s.split('\n') {
            if trim_ws(line).is_empty() {
                continue;
            }
            let st = match parse(line, &|n: &str| name_hit(&db.tables, n)) {
                Ok(st) => st,
                Err(_) => {
                    bad = true; // unusable record tail: stop replay here
                    break;
                }
            };
            if let Stmt::Select { .. } = st {
                continue;
            }
            if exec_dml(db, &st, false, line).is_err() {
                bad = true;
                break;
            }
        }
        if bad {
            break;
        }
    }

    // Anything but a pristine two-snapshot file gets rewritten below.
    let rewrite = !clean || saw_stmt || snap_count < 2;
    ScanInfo { rewrite, seq }
}

/// Replay a ticket-4 store: raw statement lines, one per line. A torn
/// trailing line (no newline) is dropped, as is everything after the first
/// statement that no longer replays.
fn replay_legacy(file: &mut File, db: &mut Db) {
    let mut bytes: Vec<u8> = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return;
    }
    let cut = match bytes.iter().rposition(|&b| b == b'\n') {
        Some(p) => p + 1,
        None => 0,
    };
    bytes.truncate(cut);
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let s = match std::str::from_utf8(line) {
            Ok(s) => s,
            Err(_) => break,
        };
        if trim_ws(s).is_empty() {
            continue;
        }
        let st = match parse(s, &|n: &str| name_hit(&db.tables, n)) {
            Ok(st) => st,
            Err(_) => break,
        };
        if let Stmt::Select { .. } = st {
            continue;
        }
        if exec_dml(db, &st, false, s).is_err() {
            break;
        }
    }
}

/// Rewrite the log as two identical snapshot records: the store then needs
/// only one (possibly damaged) tail to lose before the copy behind it still
/// holds everything, and the next recovery replays no statement history at
/// all. Written to a temp file, fsynced, then renamed atomically - a kill at
/// any point leaves the old file in place and startup redoes it.
fn compact(
    path: &Path,
    tmp: &Path,
    tables: &HashMap<String, Table>,
    seq: u64,
) -> std::io::Result<u64> {
    let blob = serialize_tables(tables);
    let end_seq;
    {
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)?;
        let mut w = Wal {
            file: f,
            seq,
            stmts_since_snap: 0,
            bytes_since_snap: 0,
        };
        w.append(KIND_SNAP, &blob)?;
        w.append(KIND_SNAP, &blob)?;
        w.file.sync_all()?;
        end_seq = w.seq;
    }
    std::fs::rename(tmp, path)?;
    if let Some(d) = path.parent() {
        if let Ok(df) = File::open(d) {
            let _ = df.sync_all();
        }
    }
    Ok(end_seq)
}

fn open_db(dir: &Path) -> Db {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("sdb.log");
    let tmp = dir.join("sdb.log.tmp");
    let _ = std::fs::remove_file(&tmp); // stale compaction temp, if any

    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("sdb: cannot open {}: {}", path.display(), e);
            std::process::exit(1);
        }
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    // Framed stores start with the record magic; ticket-4 stores are raw
    // statement lines and are replayed as such before being rewritten.
    let mut head = [0u8; 4];
    let framed = len == 0 || (read_exact_at(&file, &mut head, 0).is_ok() && head == REC_MAGIC);

    let mut db = Db {
        tables: HashMap::new(),
        wal: None,
    };
    let mut rewrite = true;
    let mut seq = 0u64;
    if framed {
        let info = scan_framed(&file, len, &mut db);
        rewrite = info.rewrite;
        seq = info.seq;
    } else {
        replay_legacy(&mut file, &mut db);
    }
    drop(file);

    if rewrite {
        seq = match compact(&path, &tmp, &db.tables, seq) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sdb: cannot write {}: {}", path.display(), e);
                std::process::exit(1);
            }
        };
    }

    let mut f = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("sdb: cannot reopen {}: {}", path.display(), e);
            std::process::exit(1);
        }
    };
    // Appends always start exactly after the last valid record (compaction
    // guarantees a clean boundary when recovery dropped a torn tail).
    let _ = f.seek(SeekFrom::End(0));
    db.wal = Some(Wal {
        file: f,
        seq,
        stmts_since_snap: 0,
        bytes_since_snap: 0,
    });
    db
}

fn main() {
    let (port, data) = parse_args();
    if let Err(e) = std::fs::create_dir_all(&data) {
        eprintln!("sdb: cannot create data dir {}: {}", data, e);
        std::process::exit(1);
    }
    let db = open_db(Path::new(&data));
    let srv = Arc::new(Server {
        db: RwLock::new(db),
        gate: Mutex::new(false),
        gcv: Condvar::new(),
    });
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sdb: cannot bind 0.0.0.0:{}: {}", port, e);
            std::process::exit(1);
        }
    };
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let srv2 = Arc::clone(&srv);
                std::thread::spawn(move || handle(stream, srv2));
            }
            Err(_) => continue,
        }
    }
}
