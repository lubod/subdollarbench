use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::ops::Bound;
use std::process;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomOrd};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ty {
    Integer,
    Text,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Value {
    Null,
    Int(i64),
    Text(String),
}

#[derive(Clone)]
struct Idx {
    name: String,
    col: usize,
    unique: bool,
    map: BTreeMap<Value, Vec<usize>>,
}

#[derive(Clone)]
struct Table {
    cols: Vec<(String, Ty)>,
    rows: Vec<Vec<Value>>,
    notnull: Vec<bool>,
    defaults: Vec<Value>,
    uniq: Vec<Vec<usize>>,
    keys: Vec<HashSet<Vec<Value>>>,
    fks: Vec<(usize, String, String)>,
    pk_auto: Option<usize>,
    pk_max: Option<i64>,
    idxs: Vec<Idx>,
}

impl Table {
    fn new(cols: Vec<(String, Ty)>) -> Table {
        let n = cols.len();
        Table {
            cols,
            rows: Vec::new(),
            notnull: vec![false; n],
            defaults: vec![Value::Null; n],
            uniq: Vec::new(),
            keys: Vec::new(),
            fks: Vec::new(),
            pk_auto: None,
            pk_max: None,
            idxs: Vec::new(),
        }
    }

    fn pos(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.0 == name)
    }
}

#[derive(Clone)]
enum Con {
    NotNull(String),
    Default(String, Value),
    Unique(Vec<String>),
    Pk(Vec<String>),
    Fk(String, String, String),
}

struct Db {
    tables: BTreeMap<String, Table>,
    log: Log,
}

struct Log {
    file: File,
    len: u64,
    dirty: bool,
}

struct Shared {
    db: Mutex<Db>,
    busy: Mutex<bool>,
    free: Condvar,
    ver: AtomicU64,
}

struct Txn {
    tables: BTreeMap<String, Table>,
    buf: Vec<String>,
    ver: u64,
    locked: bool,
}

struct Session {
    shared: Arc<Shared>,
    txn: Option<Txn>,
}

type Fail = (&'static str, String);

fn syn() -> Fail {
    ("syntax", "syntax error".to_string())
}

fn no_table() -> Fail {
    ("no_table", "no such table".to_string())
}

fn no_column(name: &str) -> Fail {
    ("no_column", format!("no such column: {}", name))
}

fn type_err() -> Fail {
    ("type", "wrong type for column".to_string())
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Int(String),
    Str(String),
    Op(&'static str),
}

fn lex(src: &str) -> Result<Vec<Tok>, Fail> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let two: Option<&'static str> = match (c, b.get(i + 1).copied()) {
            (b'<', Some(b'=')) => Some("<="),
            (b'>', Some(b'=')) => Some(">="),
            (b'<', Some(b'>')) => Some("<>"),
            (b'!', Some(b'=')) => Some("!="),
            (b'|', Some(b'|')) => Some("||"),
            _ => None,
        };
        let one: Option<&'static str> = match c {
            b'(' => Some("("),
            b')' => Some(")"),
            b',' => Some(","),
            b'*' => Some("*"),
            b'-' => Some("-"),
            b'+' => Some("+"),
            b'/' => Some("/"),
            b'%' => Some("%"),
            b';' => Some(";"),
            b'.' => Some("."),
            b'=' => Some("="),
            b'<' => Some("<"),
            b'>' => Some(">"),
            _ => None,
        };
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
        } else if let Some(op) = two {
            out.push(Tok::Op(op));
            i += 2;
        } else if let Some(op) = one {
            out.push(Tok::Op(op));
            i += 1;
        } else if c == b'\'' {
            let mut s: Vec<u8> = Vec::new();
            i += 1;
            loop {
                if i >= b.len() {
                    return Err(("syntax", "unterminated string".to_string()));
                }
                if b[i] == b'\'' {
                    if i + 1 < b.len() && b[i + 1] == b'\'' {
                        s.push(b'\'');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    s.push(b[i]);
                    i += 1;
                }
            }
            out.push(Tok::Str(String::from_utf8_lossy(&s).into_owned()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
                return Err(syn());
            }
            out.push(Tok::Int(src[start..i].to_string()));
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Word(src[start..i].to_string()));
        } else {
            return Err(syn());
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
    And,
    Or,
    Concat,
}

#[derive(Clone, Copy)]
enum Func {
    Length,
    Upper,
    Lower,
    Abs,
    Coalesce,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AggFn {
    Count,
    Sum,
    Min,
    Max,
}

#[derive(Clone)]
struct SubQ {
    stmt: Box<Stmt>,
    scope: Option<Rc<Vec<SrcCols>>>,
}

#[derive(Clone)]
enum Expr {
    Lit(Value),
    Overflow,
    Col(Option<String>, String, usize),
    Outer(usize, usize),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
    Agg(AggFn, Option<Box<Expr>>),
    Scalar(Box<SubQ>),
    Exists(Box<SubQ>),
    InSub(Box<Expr>, Box<SubQ>, bool),
    InList(Box<Expr>, Vec<Expr>, bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinKind {
    First,
    Comma,
    Inner,
    Left,
}

#[derive(Clone)]
struct FromItem {
    table: String,
    alias: Option<String>,
    kind: JoinKind,
    on: Option<Expr>,
}

#[derive(Clone)]
struct SrcCols {
    qual: String,
    cols: Vec<(String, Ty)>,
    off: usize,
}

struct Frame {
    cols: Rc<Vec<SrcCols>>,
    row: Vec<Value>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = RefCell::new(Vec::new());
}

#[derive(Clone)]
enum SelItem {
    Star,
    Item(Expr, String),
}

#[derive(Clone)]
enum Stmt {
    Begin,
    Commit,
    Rollback,
    Create {
        name: String,
        cols: Vec<(String, Ty)>,
        cons: Vec<Con>,
    },
    Insert {
        name: String,
        cols: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
        select: Option<Box<Stmt>>,
    },
    Select {
        from: Vec<FromItem>,
        distinct: bool,
        items: Vec<SelItem>,
        filter: Option<Expr>,
        group: Vec<Expr>,
        having: Option<Expr>,
        order: Vec<(Expr, bool)>,
        limit: Option<Expr>,
        offset: Option<Expr>,
    },
    Update {
        name: String,
        sets: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        name: String,
        filter: Option<Expr>,
    },
    CreateIndex {
        name: String,
        table: String,
        col: String,
        unique: bool,
    },
    DropIndex {
        name: String,
    },
    Alter {
        table: String,
        op: AlterOp,
    },
    Explain(Box<Stmt>),
    Stats,
    Role,
}

#[derive(Clone)]
enum AlterOp {
    Add {
        name: String,
        ty: Ty,
        def: Value,
        if_not: bool,
    },
    Drop {
        name: String,
        if_exists: bool,
    },
    RenCol {
        old: String,
        new: String,
    },
    RenTable {
        new: String,
    },
}

const RESERVED: &[&str] = &[
    "and", "or", "not", "is", "as", "from", "where", "select", "insert", "create", "into", "values",
    "table", "order", "group", "by", "limit", "having", "update", "set", "delete",
];

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn kw(&mut self, k: &str) -> bool {
        let hit = matches!(self.toks.get(self.pos), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(k));
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn peek_kw(&self, k: &str, off: usize) -> bool {
        matches!(self.toks.get(self.pos + off), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(k))
    }

    fn expect_kw(&mut self, k: &str) -> Result<(), Fail> {
        if self.kw(k) {
            Ok(())
        } else {
            Err(syn())
        }
    }

    fn op(&mut self, o: &str) -> bool {
        let hit = matches!(self.toks.get(self.pos), Some(Tok::Op(x)) if *x == o);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn expect_op(&mut self, o: &str) -> Result<(), Fail> {
        if self.op(o) {
            Ok(())
        } else {
            Err(syn())
        }
    }

    fn ident(&mut self) -> Result<String, Fail> {
        match self.toks.get(self.pos) {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            _ => Err(syn()),
        }
    }

    fn ident_list(&mut self) -> Result<Vec<String>, Fail> {
        self.expect_op("(")?;
        let mut v = Vec::new();
        loop {
            v.push(self.ident()?.to_ascii_lowercase());
            if self.op(",") {
                continue;
            }
            self.expect_op(")")?;
            break;
        }
        Ok(v)
    }

    fn ref_target(&mut self) -> Result<(String, String), Fail> {
        let t = self.ident()?.to_ascii_lowercase();
        let c = self.ident_list()?;
        if c.len() != 1 {
            return Err(syn());
        }
        Ok((t, c[0].clone()))
    }

    fn table_ref(&mut self, kind: JoinKind) -> Result<FromItem, Fail> {
        const NO_ALIAS: &[&str] = &["join", "inner", "left", "outer", "cross", "on", "offset"];
        let table = self.ident()?.to_ascii_lowercase();
        let alias = if self.kw("as") {
            Some(self.ident()?.to_ascii_lowercase())
        } else {
            let w = match self.toks.get(self.pos) {
                Some(Tok::Word(w)) => w.to_ascii_lowercase(),
                _ => String::new(),
            };
            if w.is_empty() || RESERVED.contains(&w.as_str()) || NO_ALIAS.contains(&w.as_str()) {
                None
            } else {
                self.pos += 1;
                Some(w)
            }
        };
        Ok(FromItem { table, alias, kind, on: None })
    }

    fn expr(&mut self) -> Result<Expr, Fail> {
        let mut l = self.and_expr()?;
        while self.kw("or") {
            let r = self.and_expr()?;
            l = Expr::Bin(BinOp::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr, Fail> {
        let mut l = self.not_expr()?;
        while self.kw("and") {
            let r = self.not_expr()?;
            l = Expr::Bin(BinOp::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not_expr(&mut self) -> Result<Expr, Fail> {
        if self.kw("not") {
            return Ok(Expr::Not(Box::new(self.not_expr()?)));
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr, Fail> {
        let mut l = self.rel_expr()?;
        loop {
            let neg = self.peek_kw("not", 0) && self.peek_kw("in", 1);
            if neg || self.peek_kw("in", 0) {
                self.pos += if neg { 2 } else { 1 };
                self.expect_op("(")?;
                l = if self.kw("select") {
                    let st = self.select_body()?;
                    self.expect_op(")")?;
                    Expr::InSub(Box::new(l), Box::new(SubQ { stmt: Box::new(st), scope: None }), neg)
                } else {
                    let mut list = Vec::new();
                    if !self.op(")") {
                        loop {
                            list.push(self.expr()?);
                            if self.op(",") {
                                continue;
                            }
                            self.expect_op(")")?;
                            break;
                        }
                    }
                    Expr::InList(Box::new(l), list, neg)
                };
                continue;
            }
            let op = if self.op("=") {
                BinOp::Eq
            } else if self.op("<>") || self.op("!=") {
                BinOp::Ne
            } else if self.kw("is") {
                if self.kw("not") {
                    BinOp::IsNot
                } else {
                    BinOp::Is
                }
            } else {
                break;
            };
            let r = self.rel_expr()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn level(
        &mut self,
        ops: &[(&str, BinOp)],
        lower: fn(&mut Parser) -> Result<Expr, Fail>,
    ) -> Result<Expr, Fail> {
        let mut l = lower(self)?;
        'outer: loop {
            for (tok, op) in ops {
                if self.op(tok) {
                    let r = lower(self)?;
                    l = Expr::Bin(*op, Box::new(l), Box::new(r));
                    continue 'outer;
                }
            }
            break;
        }
        Ok(l)
    }

    fn rel_expr(&mut self) -> Result<Expr, Fail> {
        self.level(
            &[("<", BinOp::Lt), ("<=", BinOp::Le), (">", BinOp::Gt), (">=", BinOp::Ge)],
            Parser::add_expr,
        )
    }

    fn add_expr(&mut self) -> Result<Expr, Fail> {
        self.level(&[("+", BinOp::Add), ("-", BinOp::Sub)], Parser::mul_expr)
    }

    fn mul_expr(&mut self) -> Result<Expr, Fail> {
        self.level(
            &[("*", BinOp::Mul), ("/", BinOp::Div), ("%", BinOp::Rem)],
            Parser::concat_expr,
        )
    }

    fn concat_expr(&mut self) -> Result<Expr, Fail> {
        self.level(&[("||", BinOp::Concat)], Parser::unary_expr)
    }

    fn unary_expr(&mut self) -> Result<Expr, Fail> {
        if self.op("-") {
            if let Some(Tok::Int(d)) = self.toks.get(self.pos).cloned() {
                self.pos += 1;
                return Ok(match int_lit(&d, true) {
                    Some(v) => Expr::Lit(Value::Int(v)),
                    None => Expr::Overflow,
                });
            }
            return Ok(Expr::Neg(Box::new(self.unary_expr()?)));
        }
        if self.op("+") {
            return self.unary_expr();
        }
        if self.kw("not") {
            return Ok(Expr::Not(Box::new(self.cmp_expr()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, Fail> {
        let tok = self.toks.get(self.pos).cloned().ok_or_else(syn)?;
        self.pos += 1;
        match tok {
            Tok::Op("(") => {
                if self.kw("select") {
                    let st = self.select_body()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Scalar(Box::new(SubQ { stmt: Box::new(st), scope: None })));
                }
                let e = self.expr()?;
                self.expect_op(")")?;
                Ok(e)
            }
            Tok::Int(d) => Ok(match int_lit(&d, false) {
                Some(v) => Expr::Lit(Value::Int(v)),
                None => Expr::Overflow,
            }),
            Tok::Str(s) => Ok(Expr::Lit(Value::Text(s))),
            Tok::Word(w) => {
                let low = w.to_ascii_lowercase();
                match low.as_str() {
                    "null" => return Ok(Expr::Lit(Value::Null)),
                    "true" => return Ok(Expr::Lit(Value::Int(1))),
                    "false" => return Ok(Expr::Lit(Value::Int(0))),
                    _ => {}
                }
                if RESERVED.contains(&low.as_str()) {
                    return Err(syn());
                }
                if low == "exists" && self.op("(") {
                    self.expect_kw("select")?;
                    let st = self.select_body()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Exists(Box::new(SubQ { stmt: Box::new(st), scope: None })));
                }
                if self.op("(") {
                    let agg = match low.as_str() {
                        "count" => Some(AggFn::Count),
                        "sum" => Some(AggFn::Sum),
                        "min" => Some(AggFn::Min),
                        "max" => Some(AggFn::Max),
                        _ => None,
                    };
                    if agg == Some(AggFn::Count) && self.op("*") {
                        self.expect_op(")")?;
                        return Ok(Expr::Agg(AggFn::Count, None));
                    }
                    if agg.is_some() && self.op("*") {
                        return Err(("arity", "wrong number of arguments".to_string()));
                    }
                    let f = match low.as_str() {
                        "length" => Some(Func::Length),
                        "upper" => Some(Func::Upper),
                        "lower" => Some(Func::Lower),
                        "abs" => Some(Func::Abs),
                        "coalesce" => Some(Func::Coalesce),
                        _ => None,
                    };
                    if agg.is_none() && f.is_none() {
                        return Err(syn());
                    }
                    let mut args = Vec::new();
                    if !self.op(")") {
                        loop {
                            args.push(self.expr()?);
                            if self.op(",") {
                                continue;
                            }
                            self.expect_op(")")?;
                            break;
                        }
                    }
                    if let Some(a) = agg {
                        if a == AggFn::Count && args.is_empty() {
                            return Ok(Expr::Agg(AggFn::Count, None));
                        }
                        if args.len() != 1 {
                            return Err(("arity", "wrong number of arguments".to_string()));
                        }
                        return Ok(Expr::Agg(a, Some(Box::new(args.remove(0)))));
                    }
                    let Some(f) = f else {
                        return Err(syn());
                    };
                    let ok = match f {
                        Func::Coalesce => args.len() >= 2,
                        _ => args.len() == 1,
                    };
                    if !ok {
                        return Err(("arity", "wrong number of arguments".to_string()));
                    }
                    Ok(Expr::Call(f, args))
                } else if self.op(".") {
                    let col = self.ident()?;
                    Ok(Expr::Col(Some(w), col, 0))
                } else {
                    Ok(Expr::Col(None, w, 0))
                }
            }
            _ => Err(syn()),
        }
    }

    fn select_body(&mut self) -> Result<Stmt, Fail> {
        let distinct = self.kw("distinct");
        let mut items = Vec::new();
        loop {
            if self.op("*") {
                items.push(SelItem::Star);
            } else {
                let e = self.expr()?;
                let header = if self.kw("as") {
                    self.ident()?
                } else if let Expr::Col(_, orig, _) = &e {
                    orig.clone()
                } else {
                    "?column?".to_string()
                };
                items.push(SelItem::Item(e, header));
            }
            if !self.op(",") {
                break;
            }
        }
        let mut from: Vec<FromItem> = Vec::new();
        if self.kw("from") {
            from.push(self.table_ref(JoinKind::First)?);
            loop {
                let kind = if self.op(",") {
                    JoinKind::Comma
                } else if self.kw("inner") {
                    self.expect_kw("join")?;
                    JoinKind::Inner
                } else if self.kw("left") {
                    self.kw("outer");
                    self.expect_kw("join")?;
                    JoinKind::Left
                } else if self.kw("cross") {
                    self.expect_kw("join")?;
                    JoinKind::Inner
                } else if self.kw("join") {
                    JoinKind::Inner
                } else {
                    break;
                };
                let mut f = self.table_ref(kind)?;
                if kind != JoinKind::Comma && self.kw("on") {
                    f.on = Some(self.expr()?);
                }
                from.push(f);
            }
        }
        let filter = if self.kw("where") { Some(self.expr()?) } else { None };
        let mut group = Vec::new();
        if self.kw("group") {
            self.expect_kw("by")?;
            loop {
                group.push(self.expr()?);
                if !self.op(",") {
                    break;
                }
            }
        }
        let having = if self.kw("having") { Some(self.expr()?) } else { None };
        let mut order = Vec::new();
        if self.kw("order") {
            self.expect_kw("by")?;
            loop {
                let e = self.expr()?;
                let desc = self.kw("desc");
                if !desc {
                    self.kw("asc");
                }
                order.push((e, desc));
                if !self.op(",") {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = None;
        if self.kw("limit") {
            let e = self.expr()?;
            if self.op(",") {
                offset = Some(e);
                limit = Some(self.expr()?);
            } else {
                limit = Some(e);
                if self.kw("offset") {
                    offset = Some(self.expr()?);
                }
            }
        }
        Ok(Stmt::Select {
            from,
            distinct,
            items,
            filter,
            group,
            having,
            order,
            limit,
            offset,
        })
    }
}

fn int_lit(d: &str, neg: bool) -> Option<i64> {
    let v: i128 = d.parse().ok()?;
    let v = if neg { -v } else { v };
    if v < i64::MIN as i128 || v > i64::MAX as i128 {
        None
    } else {
        Some(v as i64)
    }
}

fn parse(src: &str) -> Result<Stmt, Fail> {
    let mut toks = lex(src)?;
    if matches!(toks.last(), Some(Tok::Op(";"))) {
        toks.pop();
    }
    let mut p = Parser { toks, pos: 0 };
    let stmt = if p.peek_kw("create", 0) && (p.peek_kw("index", 1) || p.peek_kw("unique", 1)) {
        p.kw("create");
        let unique = p.kw("unique");
        p.expect_kw("index")?;
        let name = p.ident()?.to_ascii_lowercase();
        p.expect_kw("on")?;
        let table = p.ident()?.to_ascii_lowercase();
        p.expect_op("(")?;
        let col = p.ident()?.to_ascii_lowercase();
        p.expect_op(")")?;
        Stmt::CreateIndex { name, table, col, unique }
    } else if p.kw("drop") {
        p.expect_kw("index")?;
        let name = p.ident()?.to_ascii_lowercase();
        Stmt::DropIndex { name }
    } else if p.kw("create") {
        p.expect_kw("table")?;
        let name = p.ident()?.to_ascii_lowercase();
        p.expect_op("(")?;
        let mut cols: Vec<(String, Ty)> = Vec::new();
        let mut cons: Vec<Con> = Vec::new();
        let mut pk_seen = false;
        loop {
            if p.kw("primary") {
                p.expect_kw("key")?;
                if pk_seen {
                    return Err(syn());
                }
                pk_seen = true;
                cons.push(Con::Pk(p.ident_list()?));
            } else if p.kw("unique") {
                cons.push(Con::Unique(p.ident_list()?));
            } else if p.kw("foreign") {
                p.expect_kw("key")?;
                let c = p.ident_list()?;
                if c.len() != 1 {
                    return Err(syn());
                }
                p.expect_kw("references")?;
                let (t, pc) = p.ref_target()?;
                cons.push(Con::Fk(c[0].clone(), t, pc));
            } else {
                let cname = p.ident()?.to_ascii_lowercase();
                let ty = if p.kw("integer") {
                    Ty::Integer
                } else if p.kw("text") {
                    Ty::Text
                } else {
                    return Err(syn());
                };
                if cols.iter().any(|(n, _)| *n == cname) {
                    return Err(syn());
                }
                loop {
                    if p.kw("primary") {
                        p.expect_kw("key")?;
                        if pk_seen {
                            return Err(syn());
                        }
                        pk_seen = true;
                        cons.push(Con::Pk(vec![cname.clone()]));
                    } else if p.kw("not") {
                        p.expect_kw("null")?;
                        cons.push(Con::NotNull(cname.clone()));
                    } else if p.kw("null") {
                    } else if p.kw("unique") {
                        cons.push(Con::Unique(vec![cname.clone()]));
                    } else if p.kw("default") {
                        match p.unary_expr()? {
                            Expr::Lit(v) => cons.push(Con::Default(cname.clone(), v)),
                            _ => return Err(syn()),
                        }
                    } else if p.kw("references") {
                        let (t, pc) = p.ref_target()?;
                        cons.push(Con::Fk(cname.clone(), t, pc));
                    } else {
                        break;
                    }
                }
                cols.push((cname, ty));
            }
            if p.op(",") {
                continue;
            }
            p.expect_op(")")?;
            break;
        }
        Stmt::Create { name, cols, cons }
    } else if p.kw("insert") {
        p.expect_kw("into")?;
        let name = p.ident()?.to_ascii_lowercase();
        let cols = if p.op("(") {
            let mut v = Vec::new();
            loop {
                v.push(p.ident()?.to_ascii_lowercase());
                if p.op(",") {
                    continue;
                }
                p.expect_op(")")?;
                break;
            }
            Some(v)
        } else {
            None
        };
        if p.kw("select") {
            let st = p.select_body()?;
            Stmt::Insert { name, cols, rows: Vec::new(), select: Some(Box::new(st)) }
        } else {
            p.expect_kw("values")?;
            let mut rows = Vec::new();
            loop {
                p.expect_op("(")?;
                let mut row = Vec::new();
                loop {
                    row.push(p.expr()?);
                    if p.op(",") {
                        continue;
                    }
                    p.expect_op(")")?;
                    break;
                }
                rows.push(row);
                if p.op(",") {
                    continue;
                }
                break;
            }
            Stmt::Insert { name, cols, rows, select: None }
        }
    } else if p.kw("select") {
        p.select_body()?
    } else if p.kw("update") {
        let name = p.ident()?.to_ascii_lowercase();
        p.expect_kw("set")?;
        let mut sets = Vec::new();
        loop {
            let col = p.ident()?.to_ascii_lowercase();
            p.expect_op("=")?;
            sets.push((col, p.expr()?));
            if !p.op(",") {
                break;
            }
        }
        let filter = if p.kw("where") { Some(p.expr()?) } else { None };
        Stmt::Update { name, sets, filter }
    } else if p.kw("delete") {
        p.expect_kw("from")?;
        let name = p.ident()?.to_ascii_lowercase();
        let filter = if p.kw("where") { Some(p.expr()?) } else { None };
        Stmt::Delete { name, filter }
    } else if p.kw("alter") {
        p.expect_kw("table")?;
        let table = p.ident()?.to_ascii_lowercase();
        let op = if p.kw("add") {
            p.kw("column");
            let if_not = p.kw("if");
            if if_not {
                p.expect_kw("not")?;
                p.expect_kw("exists")?;
            }
            let name = p.ident()?.to_ascii_lowercase();
            let ty = if p.kw("integer") {
                Ty::Integer
            } else if p.kw("text") {
                Ty::Text
            } else {
                return Err(syn());
            };
            let mut def = Value::Null;
            while p.kw("default") {
                def = match p.unary_expr()? {
                    Expr::Lit(v) => v,
                    _ => return Err(syn()),
                };
            }
            AlterOp::Add { name, ty, def, if_not }
        } else if p.kw("drop") {
            p.kw("column");
            let if_exists = p.kw("if");
            if if_exists {
                p.expect_kw("exists")?;
            }
            let name = p.ident()?.to_ascii_lowercase();
            AlterOp::Drop { name, if_exists }
        } else if p.kw("rename") {
            if p.kw("to") {
                AlterOp::RenTable { new: p.ident()?.to_ascii_lowercase() }
            } else {
                p.kw("column");
                let old = p.ident()?.to_ascii_lowercase();
                p.expect_kw("to")?;
                let new = p.ident()?.to_ascii_lowercase();
                AlterOp::RenCol { old, new }
            }
        } else {
            return Err(syn());
        };
        Stmt::Alter { table, op }
    } else if p.kw("explain") {
        p.expect_kw("select")?;
        Stmt::Explain(Box::new(p.select_body()?))
    } else if p.kw("show") {
        if p.kw("role") {
            Stmt::Role
        } else {
            p.expect_kw("stats")?;
            Stmt::Stats
        }
    } else if p.kw("begin") {
        let _ = p.kw("deferred") || p.kw("immediate") || p.kw("exclusive");
        p.kw("transaction");
        Stmt::Begin
    } else if p.kw("commit") || p.kw("end") {
        p.kw("transaction");
        Stmt::Commit
    } else if p.kw("rollback") {
        p.kw("transaction");
        Stmt::Rollback
    } else {
        return Err(syn());
    };
    if p.pos != p.toks.len() {
        return Err(syn());
    }
    Ok(stmt)
}

fn resolve(cols: &[SrcCols], q: Option<&str>, name: &str) -> Result<usize, Fail> {
    let low = name.to_ascii_lowercase();
    let mut hits: Vec<usize> = Vec::new();
    match q {
        Some(q) => {
            let ql = q.to_ascii_lowercase();
            let mut of = cols.iter().filter(|s| s.qual == ql);
            let s = of.next().ok_or_else(|| no_column(&format!("{}.{}", q, name)))?;
            if of.next().is_some() {
                return Err(("ambiguous", format!("ambiguous table name: {}", q)));
            }
            hits.extend(s.cols.iter().position(|c| c.0 == low).map(|p| s.off + p));
        }
        None => {
            for s in cols {
                hits.extend(s.cols.iter().position(|c| c.0 == low).map(|p| s.off + p));
            }
        }
    }
    match hits.len() {
        0 => Err(no_column(name)),
        1 => Ok(hits[0]),
        _ => Err(("ambiguous", format!("ambiguous column name: {}", name))),
    }
}

fn outer_ref(q: Option<&str>, name: &str) -> Option<(usize, usize)> {
    let frames: Vec<Rc<Vec<SrcCols>>> =
        FRAMES.with(|f| f.borrow().iter().rev().map(|fr| fr.cols.clone()).collect());
    for (d, cols) in frames.iter().enumerate() {
        if let Ok(i) = resolve(cols.as_slice(), q, name) {
            return Some((d + 1, i));
        }
    }
    None
}

fn bind(e: &mut Expr, cols: &[SrcCols]) -> Result<(), Fail> {
    let rep: Option<(usize, usize)> = match e {
        Expr::Col(q, name, idx) => match resolve(cols, q.as_deref(), name.as_str()) {
            Ok(i) => {
                *idx = i;
                None
            }
            Err(err) => {
                if err.0 != "no_column" {
                    return Err(err);
                }
                match outer_ref(q.as_deref(), name.as_str()) {
                    Some(r) => Some(r),
                    None => return Err(err),
                }
            }
        },
        Expr::Neg(x) | Expr::Not(x) => {
            bind(x, cols)?;
            None
        }
        Expr::Bin(_, a, b) => {
            bind(a, cols)?;
            bind(b, cols)?;
            None
        }
        Expr::Call(_, args) => {
            for a in args {
                bind(a, cols)?;
            }
            None
        }
        Expr::Agg(_, Some(x)) => {
            bind(x, cols)?;
            None
        }
        Expr::Agg(_, None) | Expr::Lit(_) | Expr::Overflow | Expr::Outer(..) => None,
        Expr::Scalar(sq) | Expr::Exists(sq) => {
            sq.scope = Some(Rc::new(cols.to_vec()));
            None
        }
        Expr::InSub(x, sq, _) => {
            bind(x, cols)?;
            sq.scope = Some(Rc::new(cols.to_vec()));
            None
        }
        Expr::InList(x, list, _) => {
            bind(x, cols)?;
            for a in list {
                bind(a, cols)?;
            }
            None
        }
    };
    if let Some(r) = rep {
        *e = Expr::Outer(r.0, r.1);
    }
    Ok(())
}

fn to_num(v: &Value) -> i64 {
    match v {
        Value::Null => 0,
        Value::Int(i) => *i,
        Value::Text(s) => {
            let t = s.trim_start();
            let (neg, rest) = match t.as_bytes().first() {
                Some(b'-') => (true, &t[1..]),
                Some(b'+') => (false, &t[1..]),
                _ => (false, t),
            };
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let n: i64 = digits.parse().unwrap_or(0);
            if neg {
                n.wrapping_neg()
            } else {
                n
            }
        }
    }
}

fn to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Text(s) => s.clone(),
    }
}

fn truth(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        _ => Some(to_num(v) != 0),
    }
}

fn cmp_vals(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Int(_), _) => Ordering::Less,
        (_, Value::Int(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => cmp_vals(a, b),
    }
}

fn null_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => cmp_vals(a, b) == Ordering::Equal,
    }
}

fn bin(op: BinOp, l: Value, r: Value) -> Value {
    match op {
        BinOp::And => match (truth(&l), truth(&r)) {
            (Some(false), _) | (_, Some(false)) => Value::Int(0),
            (Some(true), Some(true)) => Value::Int(1),
            _ => Value::Null,
        },
        BinOp::Or => match (truth(&l), truth(&r)) {
            (Some(true), _) | (_, Some(true)) => Value::Int(1),
            (Some(false), Some(false)) => Value::Int(0),
            _ => Value::Null,
        },
        BinOp::Is => Value::Int(null_eq(&l, &r) as i64),
        BinOp::IsNot => Value::Int(!null_eq(&l, &r) as i64),
        BinOp::Concat => {
            if l == Value::Null || r == Value::Null {
                Value::Null
            } else {
                Value::Text(to_text(&l) + &to_text(&r))
            }
        }
        _ if l == Value::Null || r == Value::Null => Value::Null,
        BinOp::Eq => Value::Int((cmp_vals(&l, &r) == Ordering::Equal) as i64),
        BinOp::Ne => Value::Int((cmp_vals(&l, &r) != Ordering::Equal) as i64),
        BinOp::Lt => Value::Int((cmp_vals(&l, &r) == Ordering::Less) as i64),
        BinOp::Le => Value::Int((cmp_vals(&l, &r) != Ordering::Greater) as i64),
        BinOp::Gt => Value::Int((cmp_vals(&l, &r) == Ordering::Greater) as i64),
        BinOp::Ge => Value::Int((cmp_vals(&l, &r) != Ordering::Less) as i64),
        BinOp::Add => Value::Int(to_num(&l).wrapping_add(to_num(&r))),
        BinOp::Sub => Value::Int(to_num(&l).wrapping_sub(to_num(&r))),
        BinOp::Mul => Value::Int(to_num(&l).wrapping_mul(to_num(&r))),
        BinOp::Div => {
            let d = to_num(&r);
            if d == 0 {
                Value::Null
            } else {
                Value::Int(to_num(&l).wrapping_div(d))
            }
        }
        BinOp::Rem => {
            let d = to_num(&r);
            if d == 0 {
                Value::Null
            } else {
                Value::Int(to_num(&l).wrapping_rem(d))
            }
        }
    }
}

fn call(f: Func, vals: Vec<Value>) -> Value {
    match f {
        Func::Coalesce => vals
            .into_iter()
            .find(|v| *v != Value::Null)
            .unwrap_or(Value::Null),
        Func::Abs => match &vals[0] {
            Value::Null => Value::Null,
            v => Value::Int(to_num(v).wrapping_abs()),
        },
        Func::Length => match &vals[0] {
            Value::Null => Value::Null,
            v => Value::Int(to_text(v).chars().count() as i64),
        },
        Func::Upper => match &vals[0] {
            Value::Null => Value::Null,
            v => Value::Text(to_text(v).to_ascii_uppercase()),
        },
        Func::Lower => match &vals[0] {
            Value::Null => Value::Null,
            v => Value::Text(to_text(v).to_ascii_lowercase()),
        },
    }
}

fn has_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg(..) => true,
        Expr::Neg(x) | Expr::Not(x) => has_agg(x),
        Expr::Bin(_, a, b) => has_agg(a) || has_agg(b),
        Expr::Call(_, args) => args.iter().any(has_agg),
        Expr::InSub(x, _, _) => has_agg(x),
        Expr::InList(x, list, _) => has_agg(x) || list.iter().any(has_agg),
        _ => false,
    }
}

fn aggregate(
    tables: &BTreeMap<String, Table>,
    f: AggFn,
    arg: Option<&Expr>,
    g: &[&[Value]],
) -> Result<Value, Fail> {
    let arg = match arg {
        None => return Ok(Value::Int(g.len() as i64)),
        Some(a) => a,
    };
    if has_agg(arg) {
        return Err(syn());
    }
    let mut count = 0i64;
    let mut sum = 0i64;
    let mut best: Option<Value> = None;
    for r in g {
        let v = eval(tables, arg, r)?;
        if v == Value::Null {
            continue;
        }
        count += 1;
        match f {
            AggFn::Count => {}
            AggFn::Sum => sum = sum.wrapping_add(to_num(&v)),
            AggFn::Min | AggFn::Max => {
                let better = match &best {
                    None => true,
                    Some(b) => {
                        let o = cmp_vals(&v, b);
                        if f == AggFn::Min {
                            o == Ordering::Less
                        } else {
                            o == Ordering::Greater
                        }
                    }
                };
                if better {
                    best = Some(v);
                }
            }
        }
    }
    Ok(match f {
        AggFn::Count => Value::Int(count),
        AggFn::Sum => {
            if count == 0 {
                Value::Null
            } else {
                Value::Int(sum)
            }
        }
        AggFn::Min | AggFn::Max => best.unwrap_or(Value::Null),
    })
}

fn sub_rows(
    tables: &BTreeMap<String, Table>,
    sq: &SubQ,
    g: &[&[Value]],
    one_col: bool,
) -> Result<Vec<Vec<Value>>, Fail> {
    let scope: Rc<Vec<SrcCols>> = sq.scope.clone().unwrap_or_else(|| Rc::new(Vec::new()));
    let width = scope.last().map_or(0, |s| s.off + s.cols.len());
    let row: Vec<Value> = match g.first() {
        Some(r) => r.to_vec(),
        None => vec![Value::Null; width],
    };
    FRAMES.with(|f| f.borrow_mut().push(Frame { cols: scope, row }));
    let res = run_select(tables, (*sq.stmt).clone());
    FRAMES.with(|f| {
        f.borrow_mut().pop();
    });
    let (ncols, rows) = res?;
    if one_col && ncols != 1 {
        return Err(("arity", "sub-select must return one column".to_string()));
    }
    Ok(rows)
}

fn prep(tables: &BTreeMap<String, Table>, e: &Expr) -> Result<(), Fail> {
    match e {
        Expr::Scalar(sq) => {
            sub_rows(tables, sq, &[], true)?;
        }
        Expr::Exists(sq) => {
            sub_rows(tables, sq, &[], false)?;
        }
        Expr::InSub(x, sq, _) => {
            prep(tables, x)?;
            sub_rows(tables, sq, &[], true)?;
        }
        Expr::InList(x, list, _) => {
            prep(tables, x)?;
            for a in list {
                prep(tables, a)?;
            }
        }
        Expr::Neg(x) | Expr::Not(x) => prep(tables, x)?,
        Expr::Bin(_, a, b) => {
            prep(tables, a)?;
            prep(tables, b)?;
        }
        Expr::Call(_, args) => {
            for a in args {
                prep(tables, a)?;
            }
        }
        Expr::Agg(_, Some(x)) => prep(tables, x)?,
        _ => {}
    }
    Ok(())
}

fn in_set(v: &Value, set: Vec<Value>, neg: bool) -> Value {
    if set.is_empty() {
        return Value::Int(neg as i64);
    }
    if *v == Value::Null {
        return Value::Null;
    }
    let mut has_null = false;
    for s in &set {
        if *s == Value::Null {
            has_null = true;
        } else if cmp_vals(v, s) == Ordering::Equal {
            return Value::Int(!neg as i64);
        }
    }
    if has_null {
        Value::Null
    } else {
        Value::Int(neg as i64)
    }
}

fn eval(tables: &BTreeMap<String, Table>, e: &Expr, row: &[Value]) -> Result<Value, Fail> {
    eval_grp(tables, e, &[row])
}

fn eval_grp(tables: &BTreeMap<String, Table>, e: &Expr, g: &[&[Value]]) -> Result<Value, Fail> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Overflow => return Err(type_err()),
        Expr::Col(_, _, i) => match g.first() {
            Some(r) => r[*i].clone(),
            None => Value::Null,
        },
        Expr::Outer(d, i) => FRAMES.with(|f| {
            let fr = f.borrow();
            let v = fr[fr.len() - *d].row[*i].clone();
            v
        }),
        Expr::Agg(f, arg) => aggregate(tables, *f, arg.as_deref(), g)?,
        Expr::Neg(x) => match eval_grp(tables, x, g)? {
            Value::Null => Value::Null,
            v => Value::Int(to_num(&v).wrapping_neg()),
        },
        Expr::Not(x) => match truth(&eval_grp(tables, x, g)?) {
            None => Value::Null,
            Some(b) => Value::Int(!b as i64),
        },
        Expr::Bin(op, a, b) => {
            let l = eval_grp(tables, a, g)?;
            let r = eval_grp(tables, b, g)?;
            bin(*op, l, r)
        }
        Expr::Call(f, args) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval_grp(tables, a, g)?);
            }
            call(*f, vals)
        }
        Expr::Scalar(sq) => {
            let rows = sub_rows(tables, sq, g, true)?;
            rows.first().map_or(Value::Null, |r| r[0].clone())
        }
        Expr::Exists(sq) => {
            let rows = sub_rows(tables, sq, g, false)?;
            Value::Int((!rows.is_empty()) as i64)
        }
        Expr::InSub(x, sq, neg) => {
            let v = eval_grp(tables, x, g)?;
            let rows = sub_rows(tables, sq, g, true)?;
            in_set(&v, rows.iter().map(|r| r[0].clone()).collect(), *neg)
        }
        Expr::InList(x, list, neg) => {
            let v = eval_grp(tables, x, g)?;
            let mut vals = Vec::with_capacity(list.len());
            for a in list {
                vals.push(eval_grp(tables, a, g)?);
            }
            in_set(&v, vals, *neg)
        }
    })
}

fn coerce(v: Value, ty: Ty) -> Result<Value, Fail> {
    match (v, ty) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Int(i), Ty::Integer) => Ok(Value::Int(i)),
        (Value::Int(i), Ty::Text) => Ok(Value::Text(i.to_string())),
        (Value::Text(s), Ty::Text) => Ok(Value::Text(s)),
        (Value::Text(s), Ty::Integer) => s
            .trim()
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| type_err()),
    }
}

fn push_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("\\N"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Text(s) => {
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    c => out.push(c),
                }
            }
        }
    }
}

fn unescape(f: &str) -> String {
    let mut s = String::new();
    let mut it = f.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
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
        } else {
            s.push(c);
        }
    }
    s
}

impl Log {
    fn append(&mut self, lines: &[String]) -> Result<(), Fail> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut buf = format!("G\t{}\n", lines.len());
        for l in lines {
            buf.push_str(l);
            buf.push('\n');
        }
        self.write_raw(buf.as_bytes())
    }

    fn write_raw(&mut self, buf: &[u8]) -> Result<(), Fail> {
        if self.dirty {
            self.file.set_len(self.len).map_err(|e| ("io", e.to_string()))?;
            self.dirty = false;
        }
        match self.file.write_all(buf).and_then(|_| self.file.sync_data()) {
            Ok(()) => {
                self.len += buf.len() as u64;
                Ok(())
            }
            Err(e) => {
                self.dirty = self.file.set_len(self.len).is_err();
                let class = match e.raw_os_error() {
                    Some(28) | Some(122) => "full",
                    _ => "io",
                };
                Err((class, e.to_string()))
            }
        }
    }
}

fn lock(s: &Shared) -> MutexGuard<'_, Db> {
    s.db.lock().unwrap_or_else(|e| e.into_inner())
}

fn acquire(s: &Shared) -> Result<(), Fail> {
    let mut busy = s.busy.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = Instant::now() + Duration::from_secs(5);
    while *busy {
        let now = Instant::now();
        if now >= deadline {
            return Err(("busy", "database is locked".to_string()));
        }
        busy = s.free.wait_timeout(busy, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
    }
    *busy = true;
    Ok(())
}

fn release(s: &Shared) {
    *s.busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
    s.free.notify_all();
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.txn.as_ref().map_or(false, |t| t.locked) {
            release(&self.shared);
        }
    }
}

fn no_txn() -> Fail {
    ("txn", "no transaction is active".to_string())
}

fn begin(sess: &mut Session) -> Result<String, Fail> {
    if sess.txn.is_some() {
        return Err(("txn", "cannot start a transaction within a transaction".to_string()));
    }
    let (tables, ver) = {
        let g = lock(&sess.shared);
        (g.tables.clone(), sess.shared.ver.load(AtomOrd::SeqCst))
    };
    sess.txn = Some(Txn { tables, buf: Vec::new(), ver, locked: false });
    Ok("OK 0\n".to_string())
}

fn claim(sess: &mut Session) -> Result<(), Fail> {
    let shared = Arc::clone(&sess.shared);
    let seen = match sess.txn.as_ref() {
        Some(t) if !t.locked => t.ver,
        _ => return Ok(()),
    };
    let res = acquire(&shared).and_then(|_| {
        if shared.ver.load(AtomOrd::SeqCst) == seen {
            Ok(())
        } else {
            release(&shared);
            Err(("busy", "transaction conflict, rolled back".to_string()))
        }
    });
    match res {
        Ok(()) => {
            if let Some(t) = sess.txn.as_mut() {
                t.locked = true;
            }
            Ok(())
        }
        Err(e) => {
            sess.txn = None;
            Err(e)
        }
    }
}

fn commit(sess: &mut Session) -> Result<String, Fail> {
    let t = sess.txn.take().ok_or_else(no_txn)?;
    if !t.locked {
        return Ok("OK 0\n".to_string());
    }
    let mut end: Option<u64> = None;
    let res = {
        let mut g = lock(&sess.shared);
        let appended = g.log.append(&t.buf);
        if appended.is_ok() {
            g.tables = t.tables;
            sess.shared.ver.fetch_add(1, AtomOrd::SeqCst);
            if !t.buf.is_empty() {
                end = Some(g.log.len);
                publish(g.log.len);
            }
        }
        appended
    };
    release(&sess.shared);
    res?;
    if let Some(end) = end {
        await_sync(end);
    }
    Ok("OK 0\n".to_string())
}

fn rollback(sess: &mut Session) -> Result<String, Fail> {
    let t = sess.txn.take().ok_or_else(no_txn)?;
    if t.locked {
        release(&sess.shared);
    }
    Ok("OK 0\n".to_string())
}

fn dispatch(sess: &mut Session, st: Stmt) -> Result<String, Fail> {
    let write = !matches!(st, Stmt::Select { .. });
    if write && !is_primary() {
        return Err(("readonly", "replica is read-only".to_string()));
    }
    if write {
        claim(sess)?;
    }
    if let Some(t) = sess.txn.as_mut() {
        let Txn { tables, buf, .. } = t;
        let r = execute(tables, st, &mut |ls: &[String]| -> Result<(), Fail> {
            buf.extend_from_slice(ls);
            Ok(())
        });
        return r;
    }
    let shared = Arc::clone(&sess.shared);
    if write {
        acquire(&shared)?;
    }
    let mut end: Option<u64> = None;
    let res = {
        let mut g = lock(&shared);
        let db = &mut *g;
        let log = &mut db.log;
        let r = execute(&mut db.tables, st, &mut |ls: &[String]| -> Result<(), Fail> {
            if write && !ls.is_empty() {
                log.append(ls)?;
                end = Some(log.len);
                publish(log.len);
            }
            Ok(())
        });
        if write {
            shared.ver.fetch_add(1, AtomOrd::SeqCst);
        }
        r
    };
    if write {
        release(&shared);
    }
    if let (Ok(_), Some(end)) = (&res, end) {
        await_sync(end);
    }
    res
}

fn decode_row(vals: &[&str], cols: &[(String, Ty)]) -> Vec<Value> {
    vals.iter()
        .zip(cols.iter())
        .map(|(f, (_, ty))| {
            if *f == "\\N" {
                Value::Null
            } else if *ty == Ty::Integer {
                Value::Int(f.parse().unwrap_or(0))
            } else {
                Value::Text(unescape(f))
            }
        })
        .collect()
}

fn replay(tables: &mut BTreeMap<String, Table>, line: &str) {
    let fields: Vec<&str> = line.split('\t').collect();
    match fields[0] {
        "C" if fields.len() >= 2 && fields.len() % 2 == 0 => {
            let mut cols = Vec::new();
            for pair in fields[2..].chunks(2) {
                let ty = if pair[1] == "INTEGER" { Ty::Integer } else { Ty::Text };
                cols.push((pair[0].to_string(), ty));
            }
            tables.insert(fields[1].to_string(), Table::new(cols));
        }
        "I" if fields.len() >= 2 => {
            if let Some(t) = tables.get_mut(fields[1]) {
                let vals = &fields[2..];
                if vals.len() == t.cols.len() {
                    let row = decode_row(vals, &t.cols);
                    t.rows.push(row);
                }
            }
        }
        "K" if fields.len() >= 4 => {
            let Some(t) = tables.get_mut(fields[1]) else {
                return;
            };
            match fields[2] {
                "Q" | "P" => {
                    let g = idx_list(t, fields[3]);
                    if fields[2] == "P" {
                        for &i in &g {
                            t.notnull[i] = true;
                        }
                        if g.len() == 1 && t.cols[g[0]].1 == Ty::Integer {
                            t.pk_auto = Some(g[0]);
                        }
                    }
                    t.uniq.push(g);
                }
                "N" => {
                    if let Some(i) = t.pos(fields[3]) {
                        t.notnull[i] = true;
                    }
                }
                "V" if fields.len() >= 5 => {
                    if let Some(i) = t.pos(fields[3]) {
                        t.defaults[i] = decode_row(&[fields[4]], &t.cols[i..i + 1]).remove(0);
                    }
                }
                "F" if fields.len() >= 6 => {
                    if let Some(i) = t.pos(fields[3]) {
                        t.fks.push((i, fields[4].to_string(), fields[5].to_string()));
                    }
                }
                _ => {}
            }
        }
        "X" if fields.len() >= 5 => {
            if let Some(t) = tables.get_mut(fields[1]) {
                if let Some(ci) = t.pos(fields[3]) {
                    t.idxs.push(Idx {
                        name: fields[2].to_string(),
                        col: ci,
                        unique: fields[4] == "1",
                        map: BTreeMap::new(),
                    });
                }
            }
        }
        "Y" if fields.len() >= 3 => {
            if let Some(t) = tables.get_mut(fields[1]) {
                t.idxs.retain(|x| x.name != fields[2]);
            }
        }
        "U" if fields.len() >= 3 => {
            if let Some(t) = tables.get_mut(fields[1]) {
                let idx: usize = match fields[2].parse() {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let vals = &fields[3..];
                if vals.len() == t.cols.len() && idx < t.rows.len() {
                    t.rows[idx] = decode_row(vals, &t.cols);
                }
            }
        }
        "AA" if fields.len() >= 5 => {
            let ty = if fields[3] == "INTEGER" { Ty::Integer } else { Ty::Text };
            if let Some(t) = tables.get_mut(fields[1]) {
                let def = decode_row(&[fields[4]], &[(String::new(), ty)]).remove(0);
                t.cols.push((fields[2].to_string(), ty));
                t.notnull.push(false);
                t.defaults.push(def.clone());
                for r in t.rows.iter_mut() {
                    r.push(def.clone());
                }
            }
        }
        "AD" if fields.len() >= 3 => {
            if let Some(t) = tables.get_mut(fields[1]) {
                if let Some(ci) = t.pos(fields[2]) {
                    let shift = |x: &mut usize| {
                        if *x > ci {
                            *x -= 1;
                        }
                    };
                    t.cols.remove(ci);
                    t.notnull.remove(ci);
                    t.defaults.remove(ci);
                    for r in t.rows.iter_mut() {
                        r.remove(ci);
                    }
                    for g in t.uniq.iter_mut() {
                        g.iter_mut().for_each(&shift);
                    }
                    for ix in t.idxs.iter_mut() {
                        shift(&mut ix.col);
                    }
                    if let Some(p) = t.pk_auto.as_mut() {
                        shift(p);
                    }
                    t.fks.retain(|f| f.0 != ci);
                    for f in t.fks.iter_mut() {
                        shift(&mut f.0);
                    }
                }
            }
        }
        "AR" if fields.len() >= 4 => {
            if let Some(t) = tables.get_mut(fields[1]) {
                if let Some(ci) = t.pos(fields[2]) {
                    t.cols[ci].0 = fields[3].to_string();
                }
            }
            for t in tables.values_mut() {
                for f in t.fks.iter_mut() {
                    if f.1 == fields[1] && f.2 == fields[2] {
                        f.2 = fields[3].to_string();
                    }
                }
            }
        }
        "AT" if fields.len() >= 3 => {
            if let Some(t) = tables.remove(fields[1]) {
                tables.insert(fields[2].to_string(), t);
                for t in tables.values_mut() {
                    for f in t.fks.iter_mut() {
                        if f.1 == fields[1] {
                            f.1 = fields[2].to_string();
                        }
                    }
                }
            }
        }
        "D" if fields.len() >= 2 => {
            if let Some(t) = tables.get_mut(fields[1]) {
                let gone: HashSet<usize> =
                    fields[2..].iter().filter_map(|s| s.parse().ok()).collect();
                let mut i = 0;
                t.rows.retain(|_| {
                    let keep = !gone.contains(&i);
                    i += 1;
                    keep
                });
            }
        }
        _ => {}
    }
}

fn load(dir: &str) -> Result<Db, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = format!("{}/sdb.log", dir.trim_end_matches('/'));
    let existed = Path::new(&path).exists();
    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.to_string()),
    };
    let mut tables: BTreeMap<String, Table> = BTreeMap::new();
    let valid = replay_all(&mut tables, &data);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    if valid < data.len() {
        file.set_len(valid as u64).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    if !existed {
        File::open(dir).and_then(|d| d.sync_all()).map_err(|e| e.to_string())?;
    }
    for t in tables.values_mut() {
        reindex(t);
    }
    Ok(Db {
        tables,
        log: Log {
            file,
            len: valid as u64,
            dirty: false,
        },
    })
}

fn one_src(name: &str, cols: &[(String, Ty)]) -> Vec<SrcCols> {
    vec![SrcCols {
        qual: name.to_string(),
        cols: cols.to_vec(),
        off: 0,
    }]
}

fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Bin(BinOp::And, a, b) => {
            conjuncts(*a, out);
            conjuncts(*b, out);
        }
        e => out.push(e),
    }
}

fn span(e: &Expr, lo: usize, hi: usize) -> u8 {
    match e {
        Expr::Col(_, _, i) => {
            if *i < lo {
                1
            } else if *i < hi {
                2
            } else {
                4
            }
        }
        Expr::Neg(x) | Expr::Not(x) => span(x, lo, hi),
        Expr::Bin(_, a, b) => span(a, lo, hi) | span(b, lo, hi),
        Expr::Call(_, args) => args.iter().fold(0, |m, a| m | span(a, lo, hi)),
        Expr::Agg(_, Some(x)) => span(x, lo, hi),
        _ => 0,
    }
}

fn rebase(e: &Expr, base: usize) -> Expr {
    match e {
        Expr::Col(q, n, i) => Expr::Col(q.clone(), n.clone(), i - base),
        Expr::Neg(x) => Expr::Neg(Box::new(rebase(x, base))),
        Expr::Not(x) => Expr::Not(Box::new(rebase(x, base))),
        Expr::Bin(op, a, b) => Expr::Bin(*op, Box::new(rebase(a, base)), Box::new(rebase(b, base))),
        Expr::Call(f, args) => Expr::Call(*f, args.iter().map(|a| rebase(a, base)).collect()),
        other => other.clone(),
    }
}

fn join_rows(
    tables: &BTreeMap<String, Table>,
    from: &[FromItem],
    srcs: &[SrcCols],
) -> Result<Vec<Vec<Value>>, Fail> {
    let mut cur: Vec<Vec<Value>> = tables[&from[0].table].rows.clone();
    for j in 1..from.len() {
        let f = &from[j];
        let right = &tables[&f.table].rows;
        let lo = srcs[j].off;
        let hi = lo + srcs[j].cols.len();
        let total = srcs.last().map_or(0, |s| s.off + s.cols.len());
        let mut conj: Vec<Expr> = Vec::new();
        if let Some(on) = &f.on {
            let mut e = on.clone();
            bind(&mut e, srcs)?;
            if has_agg(&e) {
                return Err(syn());
            }
            prep(tables, &e)?;
            conjuncts(e, &mut conj);
        }
        let mut lkeys: Vec<Expr> = Vec::new();
        let mut rkeys: Vec<Expr> = Vec::new();
        let mut resid: Vec<Expr> = Vec::new();
        for c in conj {
            let mut pair = None;
            if let Expr::Bin(BinOp::Eq, a, b) = &c {
                let (sa, sb) = (span(a, lo, hi), span(b, lo, hi));
                if sa == 1 && sb == 2 {
                    pair = Some(((**a).clone(), rebase(b, lo)));
                } else if sa == 2 && sb == 1 {
                    pair = Some(((**b).clone(), rebase(a, lo)));
                }
            }
            match pair {
                Some((l, r)) => {
                    lkeys.push(l);
                    rkeys.push(r);
                }
                None => resid.push(c),
            }
        }
        let mut index: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
        if !rkeys.is_empty() {
            for (i, r) in right.iter().enumerate() {
                let mut k = Vec::with_capacity(rkeys.len());
                for e in &rkeys {
                    k.push(eval(tables, e, r)?);
                }
                if k.iter().any(|v| *v == Value::Null) {
                    continue;
                }
                index.entry(k).or_default().push(i);
            }
        }
        let all: Vec<usize> = if lkeys.is_empty() {
            (0..right.len()).collect()
        } else {
            Vec::new()
        };
        let empty: Vec<usize> = Vec::new();
        let mut out: Vec<Vec<Value>> = Vec::new();
        for l in &cur {
            let cand: &[usize] = if lkeys.is_empty() {
                &all
            } else {
                let mut k = Vec::with_capacity(lkeys.len());
                for e in &lkeys {
                    k.push(eval(tables, e, l)?);
                }
                if k.iter().any(|v| *v == Value::Null) {
                    &empty
                } else {
                    index.get(&k).map_or(&empty[..], |v| v.as_slice())
                }
            };
            let mut hit = false;
            for &ri in cand {
                let mut row = Vec::with_capacity(total);
                row.extend_from_slice(l);
                row.extend_from_slice(&right[ri]);
                row.resize(total, Value::Null);
                let mut ok = true;
                for c in &resid {
                    if truth(&eval(tables, c, &row)?) != Some(true) {
                        ok = false;
                        break;
                    }
                }
                row.truncate(hi);
                if ok {
                    hit = true;
                    out.push(row);
                }
            }
            if !hit && f.kind == JoinKind::Left {
                let mut row = l.clone();
                row.extend(std::iter::repeat(Value::Null).take(hi - lo));
                out.push(row);
            }
        }
        cur = out;
    }
    Ok(cur)
}

fn constraint(msg: String) -> Fail {
    ("constraint", msg)
}

fn groups(t: &Table) -> Vec<Vec<usize>> {
    let mut g = t.uniq.clone();
    g.extend(t.idxs.iter().filter(|x| x.unique).map(|x| vec![x.col]));
    g
}

fn build_map(rows: &[Vec<Value>], col: usize) -> BTreeMap<Value, Vec<usize>> {
    let mut m: BTreeMap<Value, Vec<usize>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        if r[col] != Value::Null {
            m.entry(r[col].clone()).or_default().push(i);
        }
    }
    m
}

fn tighter(cur: &mut Option<(Value, bool)>, v: &Value, inc: bool, lower: bool) {
    let better = match cur {
        None => true,
        Some((cv, ci)) => {
            let o = v.cmp(cv);
            let o = if lower { o } else { o.reverse() };
            o == Ordering::Greater || (o == Ordering::Equal && *ci && !inc)
        }
    };
    if better {
        *cur = Some((v.clone(), inc));
    }
}

fn scan_bounds(e: &Expr, col: usize, ty: Ty, lo: &mut Option<(Value, bool)>, hi: &mut Option<(Value, bool)>) {
    let Expr::Bin(op, a, b) = e else {
        return;
    };
    if matches!(op, BinOp::And) {
        scan_bounds(a, col, ty, lo, hi);
        scan_bounds(b, col, ty, lo, hi);
        return;
    }
    let (Expr::Col(_, _, i), Expr::Lit(v)) = (&**a, &**b) else {
        return;
    };
    if *i != col || !matches!((ty, v), (Ty::Integer, Value::Int(_)) | (Ty::Text, Value::Text(_))) {
        return;
    }
    match op {
        BinOp::Eq => {
            tighter(lo, v, true, true);
            tighter(hi, v, true, false);
        }
        BinOp::Ge => tighter(lo, v, true, true),
        BinOp::Gt => tighter(lo, v, false, true),
        BinOp::Le => tighter(hi, v, true, false),
        BinOp::Lt => tighter(hi, v, false, false),
        _ => {}
    }
}

fn index_rows(t: &Table, filter: Option<&Expr>) -> Option<Vec<Vec<Value>>> {
    let f = filter?;
    for ix in &t.idxs {
        let (mut lo, mut hi) = (None, None);
        scan_bounds(f, ix.col, t.cols[ix.col].1, &mut lo, &mut hi);
        if lo.is_none() && hi.is_none() {
            continue;
        }
        let lb = match &lo {
            None => Bound::Unbounded,
            Some((v, true)) => Bound::Included(v),
            Some((v, false)) => Bound::Excluded(v),
        };
        let hb = match &hi {
            None => Bound::Unbounded,
            Some((v, true)) => Bound::Included(v),
            Some((v, false)) => Bound::Excluded(v),
        };
        if let (Some((l, li)), Some((h, hin))) = (&lo, &hi) {
            let o = l.cmp(h);
            if o == Ordering::Greater || (o == Ordering::Equal && !(*li && *hin)) {
                return Some(Vec::new());
            }
        }
        let mut ps: Vec<usize> = Vec::new();
        for (_, v) in ix.map.range((lb, hb)) {
            ps.extend_from_slice(v);
            if ps.len() * 4 > t.rows.len() {
                return None;
            }
        }
        ps.sort_unstable();
        return Some(ps.into_iter().map(|p| t.rows[p].clone()).collect());
    }
    None
}

fn key_of(row: &[Value], g: &[usize]) -> Option<Vec<Value>> {
    let k: Vec<Value> = g.iter().map(|&i| row[i].clone()).collect();
    if k.contains(&Value::Null) {
        None
    } else {
        Some(k)
    }
}

fn idx_list(t: &Table, s: &str) -> Vec<usize> {
    s.split(',').filter_map(|n| t.pos(n)).collect()
}

fn con_rec(name: &str, con: &Con) -> String {
    match con {
        Con::NotNull(c) => format!("K\t{}\tN\t{}", name, c),
        Con::Default(c, v) => {
            let mut s = format!("K\t{}\tV\t{}\t", name, c);
            push_value(&mut s, v);
            s
        }
        Con::Unique(cs) => format!("K\t{}\tQ\t{}", name, cs.join(",")),
        Con::Pk(cs) => format!("K\t{}\tP\t{}", name, cs.join(",")),
        Con::Fk(c, p, pc) => format!("K\t{}\tF\t{}\t{}\t{}", name, c, p, pc),
    }
}

fn reindex(t: &mut Table) {
    let gs = groups(t);
    let keys: Vec<HashSet<Vec<Value>>> = gs
        .iter()
        .map(|g| t.rows.iter().filter_map(|r| key_of(r, g)).collect::<HashSet<Vec<Value>>>())
        .collect();
    t.keys = keys;
    let rows = &t.rows;
    for ix in t.idxs.iter_mut() {
        ix.map = build_map(rows, ix.col);
    }
    t.pk_max = t.pk_auto.and_then(|pi| {
        t.rows
            .iter()
            .filter_map(|r| match r[pi] {
                Value::Int(i) => Some(i),
                _ => None,
            })
            .max()
    });
}

fn check_null(name: &str, t: &Table, rows: &[Vec<Value>]) -> Result<(), Fail> {
    for (i, nn) in t.notnull.iter().enumerate() {
        if *nn && rows.iter().any(|r| r[i] == Value::Null) {
            return Err(constraint(format!("NOT NULL constraint failed: {}.{}", name, t.cols[i].0)));
        }
    }
    Ok(())
}

fn check_unique(name: &str, t: &Table, rows: &[Vec<Value>], old: bool) -> Result<(), Fail> {
    for (gi, g) in groups(t).iter().enumerate() {
        let mut seen: HashSet<Vec<Value>> = HashSet::new();
        for r in rows {
            if let Some(k) = key_of(r, g) {
                if (old && t.keys[gi].contains(&k)) || !seen.insert(k) {
                    return Err(constraint(format!("UNIQUE constraint failed: {}", name)));
                }
            }
        }
    }
    Ok(())
}

fn check_unique_seq(name: &str, t: &Table, changes: &[(usize, Vec<Value>)]) -> Result<(), Fail> {
    let mut ord: Vec<&(usize, Vec<Value>)> = changes.iter().collect();
    if let Some(pi) = t.pk_auto {
        ord.sort_by(|a, b| sort_cmp(&t.rows[a.0][pi], &t.rows[b.0][pi]));
    }
    for g in &groups(t) {
        let mut cur: HashMap<Vec<Value>, usize> = HashMap::new();
        for r in &t.rows {
            if let Some(k) = key_of(r, g) {
                *cur.entry(k).or_insert(0) += 1;
            }
        }
        for (i, nr) in &ord {
            if let Some(k) = key_of(&t.rows[*i], g) {
                let gone = match cur.get_mut(&k) {
                    Some(c) => {
                        *c -= 1;
                        *c == 0
                    }
                    None => false,
                };
                if gone {
                    cur.remove(&k);
                }
            }
            if let Some(k) = key_of(nr, g) {
                if cur.contains_key(&k) {
                    return Err(constraint(format!("UNIQUE constraint failed: {}", name)));
                }
                cur.insert(k, 1);
            }
        }
    }
    Ok(())
}

fn parent_has(
    tables: &BTreeMap<String, Table>,
    self_name: &str,
    self_vals: &HashSet<Value>,
    full: bool,
    p: &str,
    pc: &str,
    v: &Value,
) -> bool {
    if p == self_name {
        if self_vals.contains(v) {
            return true;
        }
        if full {
            return false;
        }
    }
    let Some(pt) = tables.get(p) else {
        return false;
    };
    let Some(pi) = pt.pos(pc) else {
        return false;
    };
    match groups(pt).iter().position(|g| g.len() == 1 && g[0] == pi) {
        Some(gi) => pt.keys[gi].contains(&vec![v.clone()]),
        None => pt.rows.iter().any(|r| r[pi] == *v),
    }
}

fn check_child(tables: &BTreeMap<String, Table>, name: &str, rows: &[Vec<Value>], full: bool) -> Result<(), Fail> {
    let t = &tables[name];
    for (ci, p, pc) in &t.fks {
        let self_vals: HashSet<Value> = match t.pos(pc) {
            Some(pi) if p.as_str() == name => rows.iter().map(|r| r[pi].clone()).filter(|v| *v != Value::Null).collect(),
            _ => HashSet::new(),
        };
        for r in rows {
            let v = &r[*ci];
            if *v != Value::Null && !parent_has(tables, name, &self_vals, full, p, pc, v) {
                return Err(constraint(format!("FOREIGN KEY constraint failed: {}", name)));
            }
        }
    }
    Ok(())
}

fn check_parents(tables: &BTreeMap<String, Table>, name: &str, rows: &[Vec<Value>]) -> Result<(), Fail> {
    let t = &tables[name];
    for (cn, ct) in tables {
        if cn.as_str() == name {
            continue;
        }
        for (cc, p, pc) in &ct.fks {
            if p.as_str() != name {
                continue;
            }
            let Some(pi) = t.pos(pc) else {
                continue;
            };
            let keep: HashSet<&Value> = rows.iter().map(|r| &r[pi]).collect();
            if ct.rows.iter().any(|r| r[*cc] != Value::Null && !keep.contains(&r[*cc])) {
                return Err(constraint(format!("FOREIGN KEY constraint failed: {}", cn)));
            }
        }
    }
    Ok(())
}

fn run_select(tables: &BTreeMap<String, Table>, st: Stmt) -> Result<(usize, Vec<Vec<Value>>), Fail> {
    match st {
        Stmt::Select {
            from,
            distinct,
            items,
            filter,
            group,
            having,
            order,
            limit,
            offset,
        } => {
            let (headers, rows) = select(tables, from, distinct, items, filter, group, having, order, limit, offset)?;
            Ok((headers.len(), rows))
        }
        _ => Err(syn()),
    }
}

fn select(
    tables: &BTreeMap<String, Table>,
    from: Vec<FromItem>,
    distinct: bool,
    items: Vec<SelItem>,
    filter: Option<Expr>,
    group: Vec<Expr>,
    having: Option<Expr>,
    order: Vec<(Expr, bool)>,
    limit: Option<Expr>,
    offset: Option<Expr>,
) -> Result<(Vec<String>, Vec<Vec<Value>>), Fail> {
    let no_from: Vec<Vec<Value>> = vec![Vec::new()];
    let mut srcs: Vec<SrcCols> = Vec::with_capacity(from.len());
    for f in &from {
        let t = tables.get(&f.table).ok_or_else(no_table)?;
        let qual = f.alias.clone().unwrap_or_else(|| f.table.clone());
        if srcs.iter().any(|s| s.qual == qual) {
            return Err(("ambiguous", format!("ambiguous table name: {}", qual)));
        }
        let off = srcs.last().map_or(0, |s| s.off + s.cols.len());
        srcs.push(SrcCols {
            qual,
            cols: t.cols.clone(),
            off,
        });
    }
    let cols: &[SrcCols] = &srcs;
    let mut plan: Vec<(String, Expr)> = Vec::new();
    for item in items {
        match item {
            SelItem::Star => {
                if from.is_empty() {
                    return Err(syn());
                }
                for s in cols {
                    for (i, (cn, _)) in s.cols.iter().enumerate() {
                        plan.push((
                            cn.clone(),
                            Expr::Col(Some(s.qual.clone()), cn.clone(), s.off + i),
                        ));
                    }
                }
            }
            SelItem::Item(mut e, h) => {
                bind(&mut e, cols)?;
                plan.push((h, e));
            }
        }
    }
    let mut filter = filter;
    if let Some(f) = filter.as_mut() {
        bind(f, cols)?;
        if has_agg(f) {
            return Err(syn());
        }
    }
    let mut group_keys: Vec<Expr> = Vec::with_capacity(group.len());
    for mut e in group {
        let alias = match &e {
            Expr::Col(None, n, _) if resolve(cols, None, n).is_err() => plan
                .iter()
                .find(|(h, _)| h.eq_ignore_ascii_case(n))
                .map(|(_, pe)| pe.clone()),
            _ => None,
        };
        let e = match alias {
            Some(a) => a,
            None => {
                bind(&mut e, cols)?;
                e
            }
        };
        if has_agg(&e) {
            return Err(syn());
        }
        group_keys.push(e);
    }
    let having = match having {
        Some(mut h) => {
            bind(&mut h, cols)?;
            Some(h)
        }
        None => None,
    };
    enum Key {
        Out(usize),
        Src(Expr),
    }
    let mut keys: Vec<(Key, bool)> = Vec::with_capacity(order.len());
    for (mut e, desc) in order {
        let key = match &e {
            Expr::Lit(Value::Int(k)) => {
                if *k < 1 || *k as usize > plan.len() {
                    return Err(("syntax", "ORDER BY term out of range".to_string()));
                }
                Key::Out(*k as usize - 1)
            }
            Expr::Col(None, n, _) => match plan.iter().position(|(h, pe)| h.eq_ignore_ascii_case(n) && !matches!(pe, Expr::Col(_, c, _) if c.eq_ignore_ascii_case(h))) {
                Some(i) => Key::Out(i),
                None => {
                    bind(&mut e, cols)?;
                    Key::Src(e)
                }
            },
            _ => {
                bind(&mut e, cols)?;
                Key::Src(e)
            }
        };
        keys.push((key, desc));
    }
    for (_, e) in &plan {
        prep(tables, e)?;
    }
    if let Some(f) = &filter {
        prep(tables, f)?;
    }
    for g in &group_keys {
        prep(tables, g)?;
    }
    if let Some(h) = &having {
        prep(tables, h)?;
    }
    for (k, _) in &keys {
        if let Key::Src(e) = k {
            prep(tables, e)?;
        }
    }
    let bound_int = |e: Option<Expr>| -> Result<Option<i64>, Fail> {
        match e {
            None => Ok(None),
            Some(mut e) => {
                bind(&mut e, &[])?;
                match eval(tables, &e, &[])? {
                    Value::Int(n) => Ok(Some(n)),
                    _ => Err(type_err()),
                }
            }
        }
    };
    let limit_n = bound_int(limit)?;
    let offset_n = bound_int(offset)?;
    let joined: Vec<Vec<Value>>;
    let narrowed: Vec<Vec<Value>>;
    let rows: &[Vec<Value>] = match from.len() {
        0 => &no_from,
        1 => {
            let t = &tables[&from[0].table];
            match index_rows(t, filter.as_ref()) {
                Some(v) => {
                    narrowed = v;
                    &narrowed
                }
                None => &t.rows,
            }
        }
        _ => {
            joined = join_rows(tables, &from, cols)?;
            &joined
        }
    };

    let mut produced: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    let aggregate_query = !group_keys.is_empty()
        || having.is_some()
        || plan.iter().any(|(_, e)| has_agg(e))
        || keys.iter().any(|(k, _)| matches!(k, Key::Src(e) if has_agg(e)));
    if aggregate_query {
        let mut groups: Vec<Vec<&[Value]>> = Vec::new();
        if group_keys.is_empty() {
            groups.push(Vec::new());
        }
        let mut index: HashMap<Vec<Value>, usize> = HashMap::new();
        for row in rows {
            if let Some(f) = &filter {
                if truth(&eval(tables, f, row)?) != Some(true) {
                    continue;
                }
            }
            if group_keys.is_empty() {
                groups[0].push(row.as_slice());
            } else {
                let mut key = Vec::with_capacity(group_keys.len());
                for e in &group_keys {
                    key.push(eval(tables, e, row)?);
                }
                let next = groups.len();
                let gi = *index.entry(key).or_insert(next);
                if gi == next {
                    groups.push(Vec::new());
                }
                groups[gi].push(row.as_slice());
            }
        }
        for g in &groups {
            if let Some(h) = &having {
                if truth(&eval_grp(tables, h, g)?) != Some(true) {
                    continue;
                }
            }
            let mut out_row = Vec::with_capacity(plan.len());
            for (_, e) in &plan {
                out_row.push(eval_grp(tables, e, g)?);
            }
            let mut sort_row = Vec::with_capacity(keys.len());
            for (k, _) in &keys {
                sort_row.push(match k {
                    Key::Out(i) => out_row[*i].clone(),
                    Key::Src(e) => eval_grp(tables, e, g)?,
                });
            }
            produced.push((out_row, sort_row));
        }
    } else {
        for row in rows {
            if let Some(f) = &filter {
                if truth(&eval(tables, f, row)?) != Some(true) {
                    continue;
                }
            }
            let mut out_row = Vec::with_capacity(plan.len());
            for (_, e) in &plan {
                out_row.push(eval(tables, e, row)?);
            }
            let mut sort_row = Vec::with_capacity(keys.len());
            for (k, _) in &keys {
                sort_row.push(match k {
                    Key::Out(i) => out_row[*i].clone(),
                    Key::Src(e) => eval(tables, e, row)?,
                });
            }
            produced.push((out_row, sort_row));
        }
    }
    if distinct {
        let mut seen: HashSet<Vec<Value>> = HashSet::new();
        produced.retain(|(o, _)| seen.insert(o.clone()));
    }
    if !keys.is_empty() {
        produced.sort_by(|a, b| {
            for ((_, desc), (x, y)) in keys.iter().zip(a.1.iter().zip(b.1.iter())) {
                let c = sort_cmp(x, y);
                let c = if *desc { c.reverse() } else { c };
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }
    let skip = offset_n.unwrap_or(0).max(0) as usize;
    let take = match limit_n {
        Some(n) if n >= 0 => n as usize,
        _ => usize::MAX,
    };
    let headers: Vec<String> = plan.iter().map(|(h, _)| h.clone()).collect();
    let out_rows: Vec<Vec<Value>> = produced
        .into_iter()
        .skip(skip)
        .take(take)
        .map(|(vals, _)| vals)
        .collect();
    Ok((headers, out_rows))
}

fn execute(
    tables: &mut BTreeMap<String, Table>,
    stmt: Stmt,
    emit: &mut dyn FnMut(&[String]) -> Result<(), Fail>,
) -> Result<String, Fail> {
    match stmt {
        Stmt::Create { name, cols, mut cons } => {
            if tables.contains_key(&name) || tables.values().any(|t| t.idxs.iter().any(|x| x.name == name)) {
                return Err(("exists", "table already exists".to_string()));
            }
            for con in cons.iter_mut() {
                let names: Vec<&String> = match &*con {
                    Con::NotNull(c) | Con::Default(c, _) | Con::Fk(c, _, _) => vec![c],
                    Con::Unique(cs) | Con::Pk(cs) => cs.iter().collect(),
                };
                for n in names {
                    if !cols.iter().any(|(cn, _)| cn == n) {
                        return Err(no_column(n));
                    }
                }
                if let Con::Default(c, v) = con {
                    let ty = cols.iter().find(|(cn, _)| cn.as_str() == c.as_str()).map_or(Ty::Text, |x| x.1);
                    *v = coerce(v.clone(), ty)?;
                }
            }
            for con in cons.iter() {
                if let Con::Fk(_, p, pc) = con {
                    let ok = if *p == name {
                        cols.iter().any(|(cn, _)| cn == pc)
                    } else {
                        tables.get(p.as_str()).map_or(true, |pt| pt.pos(pc).is_some())
                    };
                    if !ok {
                        return Err(no_column(pc));
                    }
                }
            }
            let mut rec = format!("C\t{}", name);
            for (c, t) in &cols {
                rec.push('\t');
                rec.push_str(c);
                rec.push('\t');
                rec.push_str(if *t == Ty::Integer { "INTEGER" } else { "TEXT" });
            }
            let mut lines = vec![rec];
            lines.extend(cons.iter().map(|c| con_rec(&name, c)));
            emit(&lines)?;
            for l in &lines {
                replay(tables, l);
            }
            if let Some(t) = tables.get_mut(&name) {
                reindex(t);
            }
            Ok("OK 0\n".to_string())
        }
        Stmt::Insert {
            name,
            cols,
            mut rows,
            select,
        } => {
            let mut sel_width: Option<usize> = None;
            if let Some(st) = select {
                let (n, out) = run_select(tables, *st)?;
                sel_width = Some(n);
                rows = out
                    .into_iter()
                    .map(|r| r.into_iter().map(Expr::Lit).collect())
                    .collect();
            }
            let converted = {
                let t = tables.get(&name).ok_or_else(no_table)?;
                let targets: Vec<usize> = match &cols {
                    None => (0..t.cols.len()).collect(),
                    Some(list) => {
                        let mut v = Vec::with_capacity(list.len());
                        for c in list {
                            let pos = t
                                .cols
                                .iter()
                                .position(|tc| tc.0 == *c)
                                .ok_or_else(|| no_column(c))?;
                            v.push(pos);
                        }
                        v
                    }
                };
                if sel_width.map_or(false, |n| n != targets.len()) || rows.iter().any(|r| r.len() != targets.len()) {
                    return Err(("arity", "wrong number of values".to_string()));
                }
                for row in rows.iter_mut() {
                    for e in row.iter_mut() {
                        bind(e, &[])?;
                        if has_agg(e) {
                            return Err(syn());
                        }
                    }
                }
                let mut out: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
                let mut cur = t.pk_max;
                for row in &rows {
                    let mut vals = t.defaults.clone();
                    let mut set = vec![false; t.cols.len()];
                    for (e, &ci) in row.iter().zip(targets.iter()) {
                        let v = eval(tables, e, &[])?;
                        if !set[ci] {
                            vals[ci] = coerce(v, t.cols[ci].1)?;
                            set[ci] = true;
                        }
                    }
                    if let Some(pi) = t.pk_auto {
                        match vals[pi].clone() {
                            Value::Null => {
                                let next = match cur {
                                    None => 1,
                                    Some(m) => m
                                        .checked_add(1)
                                        .ok_or_else(|| constraint("PRIMARY KEY overflow".to_string()))?,
                                };
                                vals[pi] = Value::Int(next);
                                cur = Some(next);
                            }
                            Value::Int(v) => cur = Some(cur.map_or(v, |m| m.max(v))),
                            _ => {}
                        }
                    }
                    out.push(vals);
                }
                out
            };
            let t = &tables[&name];
            check_null(&name, t, &converted)?;
            check_unique(&name, t, &converted, true)?;
            check_child(tables, &name, &converted, false)?;
            let lines: Vec<String> = converted
                .iter()
                .map(|v| {
                    let mut l = format!("I\t{}", name);
                    for x in v {
                        l.push('\t');
                        push_value(&mut l, x);
                    }
                    l
                })
                .collect();
            emit(&lines)?;
            let n = converted.len();
            let t = tables.get_mut(&name).ok_or_else(no_table)?;
            let gs = groups(t);
            let base = t.rows.len();
            for (j, r) in converted.iter().enumerate() {
                for (gi, g) in gs.iter().enumerate() {
                    if let Some(k) = key_of(r, g) {
                        t.keys[gi].insert(k);
                    }
                }
                for ix in t.idxs.iter_mut() {
                    if r[ix.col] != Value::Null {
                        ix.map.entry(r[ix.col].clone()).or_default().push(base + j);
                    }
                }
                if let Some(pi) = t.pk_auto {
                    if let Value::Int(v) = r[pi] {
                        t.pk_max = Some(t.pk_max.map_or(v, |m| m.max(v)));
                    }
                }
            }
            t.rows.extend(converted);
            Ok(format!("OK {}\n", n))
        }
        Stmt::Select {
            from,
            distinct,
            items,
            filter,
            group,
            having,
            order,
            limit,
            offset,
        } => {
            let (headers, rows) = select(tables, from, distinct, items, filter, group, having, order, limit, offset)?;
            let mut out = format!("ROWS {}\n", headers.len());
            out.push_str(&headers.join("\t"));
            out.push('\n');
            for vals in &rows {
                for (k, v) in vals.iter().enumerate() {
                    if k > 0 {
                        out.push('\t');
                    }
                    push_value(&mut out, v);
                }
                out.push('\n');
            }
            out.push_str(&format!("END {}\n", rows.len()));
            Ok(out)
        }
        Stmt::Update { name, sets, filter } => {
            let t = tables.get(&name).ok_or_else(no_table)?;
            let mut targets: Vec<usize> = Vec::with_capacity(sets.len());
            let mut exprs: Vec<Expr> = Vec::with_capacity(sets.len());
            for (c, mut e) in sets {
                let pos = t
                    .cols
                    .iter()
                    .position(|tc| tc.0 == c)
                    .ok_or_else(|| no_column(&c))?;
                bind(&mut e, &one_src(&name, &t.cols))?;
                if has_agg(&e) {
                    return Err(syn());
                }
                targets.push(pos);
                exprs.push(e);
            }
            let mut filter = filter;
            if let Some(f) = filter.as_mut() {
                bind(f, &one_src(&name, &t.cols))?;
                if has_agg(f) {
                    return Err(syn());
                }
            }
            for e in &exprs {
                prep(tables, e)?;
            }
            if let Some(f) = &filter {
                prep(tables, f)?;
            }
            let mut changes: Vec<(usize, Vec<Value>)> = Vec::new();
            for (i, row) in t.rows.iter().enumerate() {
                if let Some(f) = &filter {
                    if truth(&eval(tables, f, row)?) != Some(true) {
                        continue;
                    }
                }
                let mut new_row = row.clone();
                for (e, &ci) in exprs.iter().zip(targets.iter()) {
                    new_row[ci] = coerce(eval(tables, e, row)?, t.cols[ci].1)?;
                }
                changes.push((i, new_row));
            }
            if changes.is_empty() {
                return Ok("OK 0\n".to_string());
            }
            let mut new_rows = t.rows.clone();
            for (i, r) in &changes {
                new_rows[*i] = r.clone();
            }
            if let Some(pi) = t.pk_auto {
                if changes.iter().any(|(_, r)| r[pi] == Value::Null) {
                    return Err(type_err());
                }
            }
            check_null(&name, t, &new_rows)?;
            check_unique_seq(&name, t, &changes)?;
            check_child(tables, &name, &new_rows, true)?;
            check_parents(tables, &name, &new_rows)?;
            let lines: Vec<String> = changes
                .iter()
                .map(|(i, r)| {
                    let mut l = format!("U\t{}\t{}", name, i);
                    for x in r {
                        l.push('\t');
                        push_value(&mut l, x);
                    }
                    l
                })
                .collect();
            emit(&lines)?;
            let n = changes.len();
            let t = tables.get_mut(&name).ok_or_else(no_table)?;
            t.rows = new_rows;
            reindex(t);
            Ok(format!("OK {}\n", n))
        }
        Stmt::Delete { name, filter } => {
            let t = tables.get(&name).ok_or_else(no_table)?;
            let mut filter = filter;
            if let Some(f) = filter.as_mut() {
                bind(f, &one_src(&name, &t.cols))?;
                if has_agg(f) {
                    return Err(syn());
                }
            }
            if let Some(f) = &filter {
                prep(tables, f)?;
            }
            let mut dels: Vec<usize> = Vec::new();
            for (i, row) in t.rows.iter().enumerate() {
                let hit = match &filter {
                    None => true,
                    Some(f) => truth(&eval(tables, f, row)?) == Some(true),
                };
                if hit {
                    dels.push(i);
                }
            }
            let n = dels.len();
            if n == 0 {
                return Ok("OK 0\n".to_string());
            }
            let gone: HashSet<usize> = dels.iter().copied().collect();
            let new_rows: Vec<Vec<Value>> = t
                .rows
                .iter()
                .enumerate()
                .filter(|(i, _)| !gone.contains(i))
                .map(|(_, r)| r.clone())
                .collect();
            check_child(tables, &name, &new_rows, true)?;
            check_parents(tables, &name, &new_rows)?;
            let mut rec = format!("D\t{}", name);
            for i in &dels {
                rec.push('\t');
                rec.push_str(&i.to_string());
            }
            emit(&[rec])?;
            let t = tables.get_mut(&name).ok_or_else(no_table)?;
            t.rows = new_rows;
            reindex(t);
            Ok(format!("OK {}\n", n))
        }
        Stmt::CreateIndex { name, table, col, unique } => {
            if tables.contains_key(&name) || tables.values().any(|t| t.idxs.iter().any(|x| x.name == name)) {
                return Err(("exists", "index already exists".to_string()));
            }
            let t = tables.get(&table).ok_or_else(no_table)?;
            let ci = t.pos(&col).ok_or_else(|| no_column(&col))?;
            if unique && build_map(&t.rows, ci).values().any(|v| v.len() > 1) {
                return Err(constraint(format!("UNIQUE constraint failed: {}", table)));
            }
            let rec = format!("X\t{}\t{}\t{}\t{}", table, name, col, if unique { 1 } else { 0 });
            emit(&[rec.clone()])?;
            replay(tables, &rec);
            if let Some(t) = tables.get_mut(&table) {
                reindex(t);
            }
            Ok("OK 0\n".to_string())
        }
        Stmt::DropIndex { name } => {
            let owner = tables
                .iter()
                .find(|(_, t)| t.idxs.iter().any(|x| x.name == name))
                .map(|(k, _)| k.clone())
                .ok_or_else(no_table)?;
            let rec = format!("Y\t{}\t{}", owner, name);
            emit(&[rec.clone()])?;
            replay(tables, &rec);
            if let Some(t) = tables.get_mut(&owner) {
                reindex(t);
            }
            Ok("OK 0\n".to_string())
        }
        Stmt::Alter { table, op } => {
            let t = tables.get(&table).ok_or_else(no_table)?;
            let (rec, target) = match op {
                AlterOp::Add { name, ty, def, if_not } => {
                    if t.pos(&name).is_some() {
                        if if_not {
                            return Ok("OK 0\n".to_string());
                        }
                        return Err(("exists", format!("duplicate column name: {}", name)));
                    }
                    let def = coerce(def, ty).map_err(|_| ("syntax", "type mismatch on DEFAULT".to_string()))?;
                    let mut enc = String::new();
                    push_value(&mut enc, &def);
                    let tn = if ty == Ty::Integer { "INTEGER" } else { "TEXT" };
                    (format!("AA\t{}\t{}\t{}\t{}", table, name, tn, enc), table.clone())
                }
                AlterOp::Drop { name, if_exists } => {
                    let Some(ci) = t.pos(&name) else {
                        if if_exists {
                            return Ok("OK 0\n".to_string());
                        }
                        return Err(no_column(&name));
                    };
                    let referenced = tables
                        .values()
                        .any(|x| x.fks.iter().any(|f| f.1 == table && f.2 == name));
                    if t.cols.len() == 1
                        || t.pk_auto == Some(ci)
                        || t.uniq.iter().any(|g| g.contains(&ci))
                        || t.idxs.iter().any(|x| x.col == ci)
                        || referenced
                    {
                        return Err(constraint(format!("cannot drop column: {}", name)));
                    }
                    (format!("AD\t{}\t{}", table, name), table.clone())
                }
                AlterOp::RenCol { old, new } => {
                    t.pos(&old).ok_or_else(|| no_column(&old))?;
                    if old == new {
                        return Ok("OK 0\n".to_string());
                    }
                    if t.pos(&new).is_some() {
                        return Err(("exists", format!("duplicate column name: {}", new)));
                    }
                    (format!("AR\t{}\t{}\t{}", table, old, new), table.clone())
                }
                AlterOp::RenTable { new } => {
                    if tables.contains_key(&new) || tables.values().any(|x| x.idxs.iter().any(|i| i.name == new)) {
                        return Err(("exists", format!("there is already another table or index with this name: {}", new)));
                    }
                    (format!("AT\t{}\t{}", table, new), new.clone())
                }
            };
            emit(&[rec.clone()])?;
            replay(tables, &rec);
            if let Some(t) = tables.get_mut(&target) {
                reindex(t);
            }
            Ok("OK 0\n".to_string())
        }
        Stmt::Begin | Stmt::Commit | Stmt::Rollback | Stmt::Explain(_) | Stmt::Stats | Stmt::Role => Err(syn()),
    }
}

const ZERO: AtomicU64 = AtomicU64::new(0);
static STATS: [AtomicU64; 10] = [ZERO; 10];
const STAT_NAMES: [&str; 10] = [
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
];
const ST_ERRORS: usize = 7;
const ST_CONN_CURRENT: usize = 8;
const ST_CONN_TOTAL: usize = 9;

fn bump(i: usize) {
    STATS[i].fetch_add(1, AtomOrd::SeqCst);
}

struct ConnGuard;

impl Drop for ConnGuard {
    fn drop(&mut self) {
        STATS[ST_CONN_CURRENT].fetch_sub(1, AtomOrd::SeqCst);
    }
}

fn count_statement(line: &str) {
    if line.trim().is_empty() {
        return;
    }
    bump(0);
    let word = line
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = match word.as_str() {
        "select" => 1,
        "insert" => 2,
        "update" => 3,
        "delete" => 4,
        "commit" | "end" => 5,
        "rollback" => 6,
        _ => return,
    };
    bump(kind);
}

fn show_stats() -> String {
    let mut out = String::from("ROWS 2\nname\tvalue\n");
    for (i, name) in STAT_NAMES.iter().enumerate() {
        out.push_str(&format!("{}\t{}\n", name, STATS[i].load(AtomOrd::SeqCst)));
    }
    out.push_str(&format!("END {}\n", STAT_NAMES.len()));
    out
}

fn plan(tables: &BTreeMap<String, Table>, st: Stmt) -> Result<Vec<String>, Fail> {
    let Stmt::Select { from, items, filter, .. } = st else {
        return Err(syn());
    };
    if from.is_empty() {
        return Ok(vec!["CONSTANT ROW".to_string()]);
    }
    let mut srcs: Vec<SrcCols> = Vec::with_capacity(from.len());
    for f in &from {
        let t = tables.get(&f.table).ok_or_else(no_table)?;
        let qual = f.alias.clone().unwrap_or_else(|| f.table.clone());
        if srcs.iter().any(|s| s.qual == qual) {
            return Err(("ambiguous", format!("ambiguous table name: {}", qual)));
        }
        let off = srcs.last().map_or(0, |s| s.off + s.cols.len());
        srcs.push(SrcCols {
            qual,
            cols: t.cols.clone(),
            off,
        });
    }
    for item in &items {
        if let SelItem::Item(e, _) = item {
            let mut e = e.clone();
            bind(&mut e, &srcs)?;
        }
    }
    let filter = match filter {
        Some(mut f) => {
            bind(&mut f, &srcs)?;
            Some(f)
        }
        None => None,
    };
    let mut out = Vec::new();
    for f in &from {
        let t = &tables[&f.table];
        let mut used = None;
        if let (1, Some(pred)) = (from.len(), filter.as_ref()) {
            for ix in &t.idxs {
                let (mut lo, mut hi) = (None, None);
                scan_bounds(pred, ix.col, t.cols[ix.col].1, &mut lo, &mut hi);
                if lo.is_some() || hi.is_some() {
                    used = Some(ix.name.clone());
                    break;
                }
            }
        }
        match used {
            Some(ix) => out.push(format!("INDEX {}.{}", f.table, ix)),
            None => out.push(format!("SCAN {}", f.table)),
        }
    }
    Ok(out)
}

fn explain(sess: &Session, st: Stmt) -> Result<String, Fail> {
    let lines = {
        let g = lock(&sess.shared);
        let tables = match sess.txn.as_ref() {
            Some(t) => &t.tables,
            None => &g.tables,
        };
        plan(tables, st)?
    };
    let mut out = String::from("ROWS 1\ndetail\n");
    for l in &lines {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!("END {}\n", lines.len()));
    Ok(out)
}

fn run(sess: &mut Session, line: &str) -> String {
    count_statement(line);
    let res = parse(line).and_then(|st| match st {
        Stmt::Begin => begin(sess),
        Stmt::Commit => commit(sess),
        Stmt::Rollback => rollback(sess),
        Stmt::Explain(inner) => explain(sess, *inner),
        Stmt::Stats => Ok(show_stats()),
        Stmt::Role => Ok(show_role()),
        st => dispatch(sess, st),
    });
    let out = match res {
        Ok(s) => s,
        Err((c, m)) => format!("ERR {} {}\n", c, m),
    };
    if out.starts_with("ERR ") {
        bump(ST_ERRORS);
    }
    out
}

fn handle_conn(stream: TcpStream, db: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut buf: Vec<u8> = Vec::new();
    let mut sess = Session { shared: db, txn: None };
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let complete = buf.last() == Some(&b'\n');
        if complete {
            buf.pop();
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        let line = String::from_utf8_lossy(&buf);
        if line == "STATE" {
            if writer.write_all(state_line().as_bytes()).is_err() {
                break;
            }
            continue;
        }
        if let Some(from) = line.strip_prefix("REPLICATE ") {
            if let Ok(n) = from.trim().parse::<u64>() {
                serve_replica(writer, reader, n);
            }
            break;
        }
        let resp = run(&mut sess, &line);
        if writer.write_all(resp.as_bytes()).is_err() {
            break;
        }
        if !complete {
            break;
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut port: u16 = env::var("SDB_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(5780);
    let mut data = env::var("SDB_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let mut replicaof: Option<String> = None;
    let mut pg_port: Option<u16> = env::var("SDB_PG_PORT").ok().and_then(|s| s.parse().ok());
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--port" || a == "--data" {
            i += 1;
            let v = args.get(i).cloned().unwrap_or_default();
            if a == "--port" {
                port = v.parse().unwrap_or_else(|_| {
                    eprintln!("bad --port");
                    process::exit(2)
                });
            } else {
                data = v;
            }
        } else if a == "--replicaof" {
            i += 1;
            replicaof = args.get(i).cloned();
        } else if let Some(v) = a.strip_prefix("--replicaof=") {
            replicaof = Some(v.to_string());
        } else if a == "--pg-port" {
            i += 1;
            pg_port = args.get(i).and_then(|v| v.parse().ok());
        } else if let Some(v) = a.strip_prefix("--pg-port=") {
            pg_port = v.parse().ok();
        } else if let Some(v) = a.strip_prefix("--port=") {
            port = v.parse().unwrap_or_else(|_| {
                eprintln!("bad --port");
                process::exit(2)
            });
        } else if let Some(v) = a.strip_prefix("--data=") {
            data = v.to_string();
        }
        i += 1;
    }

    let db = match load(&data) {
        Ok(d) => Arc::new(Shared {
            db: Mutex::new(d),
            busy: Mutex::new(false),
            free: Condvar::new(),
            ver: AtomicU64::new(0),
        }),
        Err(e) => {
            eprintln!("cannot open data dir {}: {}", data, e);
            process::exit(1);
        }
    };
    repl::start(port, &data, replicaof, Arc::clone(&db));
    if let Some(p) = pg_port {
        pg::serve(p, Arc::clone(&db), env::var("SDB_SQL_PASSWORD").ok());
    }
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind port {}: {}", port, e);
            process::exit(1);
        }
    };
    for s in listener.incoming() {
        if let Ok(s) = s {
            bump(ST_CONN_TOTAL);
            STATS[ST_CONN_CURRENT].fetch_add(1, AtomOrd::SeqCst);
            let db = Arc::clone(&db);
            thread::spawn(move || {
                let _guard = ConnGuard;
                handle_conn(s, db)
            });
        }
    }
}

mod repl;
use repl::*;

mod pg;

#[cfg(test)]
mod tests;
