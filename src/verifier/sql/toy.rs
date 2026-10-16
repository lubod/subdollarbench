//! Test-only toy engines for the stage 0-3 dialect.
//!
//! Two jobs:
//! 1. **Independent reference.** [`Toy`] is a second implementation of the
//!    dialect written from the spec, not from SQLite. If it agrees with the
//!    SQLite oracle on every generated script, the spec, the generator and
//!    the oracle are consistent; a disagreement is a spec gap or an SQLite
//!    quirk the spec forgot, found before any model is blamed for it.
//! 2. **Mutants.** [`Fault`] plants one realistic bug at a time in the toy.
//!    The verifier must catch every one, or the generator has a blind spot.

use super::oracle::{ErrClass, Value as V};
use super::run::Candidate;
use super::wire::{encode_text, encode_value, Response};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// One planted bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fault {
    /// NULL sorts last ascending (first descending).
    NullsLast,
    /// `SET a = b, b = a` assigns sequentially (second sees the first).
    UpdateSeesNewValues,
    /// `/` floors instead of truncating toward zero.
    FloorDivision,
    /// `%` takes the divisor's sign.
    ModuloSign,
    /// Division or modulo by zero is an error instead of NULL.
    DivideByZeroErrors,
    /// `OR` binds tighter than `AND`.
    OrBindsTighterThanAnd,
    /// `a - b - c` groups to the right.
    MinusIsRightAssociative,
    /// Two-valued logic: comparisons with NULL are false, `NOT NULL` is true.
    TwoValuedLogic,
    /// `DISTINCT` keeps every NULL row.
    DistinctKeepsNulls,
    /// `OFFSET` is ignored.
    OffsetIgnored,
    /// `LIMIT n` returns up to n+1 rows.
    LimitPlusOne,
    /// `DELETE ... WHERE` deletes every row.
    DeleteIgnoresWhere,
    /// UPDATE counts only rows whose values changed.
    UpdateCountsChangedOnly,
    /// A failing multi-row INSERT keeps the rows before the bad one.
    InsertNotAtomic,
    /// Tabs and backslashes are not escaped on the wire.
    NoWireEscaping,
    /// `NULL || x` is `x`.
    ConcatNullAsEmpty,
    /// `SELECT *` lists columns in reverse order.
    StarReversed,
    /// Only upper-case keywords are recognized.
    CaseSensitiveKeywords,
    /// A tab is not whitespace.
    TabNotWhitespace,
    /// A trailing `;` is a syntax error.
    SemicolonRejected,
    /// A NULL predicate keeps the row.
    WhereNullIsTrue,
    /// A missing column is reported as a missing table.
    WrongErrorClass,
    /// TEXT is accepted into an INTEGER column.
    TypeNotEnforced,
    /// Result headers are upper-cased.
    HeadersUppercased,
    /// `length()` counts bytes plus one.
    LengthOffByOne,
    /// `coalesce` only looks at its first argument.
    CoalesceFirstOnly,
}

impl Fault {
    pub const ALL: [Fault; 26] = [
        Fault::NullsLast,
        Fault::UpdateSeesNewValues,
        Fault::FloorDivision,
        Fault::ModuloSign,
        Fault::DivideByZeroErrors,
        Fault::OrBindsTighterThanAnd,
        Fault::MinusIsRightAssociative,
        Fault::TwoValuedLogic,
        Fault::DistinctKeepsNulls,
        Fault::OffsetIgnored,
        Fault::LimitPlusOne,
        Fault::DeleteIgnoresWhere,
        Fault::UpdateCountsChangedOnly,
        Fault::InsertNotAtomic,
        Fault::NoWireEscaping,
        Fault::ConcatNullAsEmpty,
        Fault::StarReversed,
        Fault::CaseSensitiveKeywords,
        Fault::TabNotWhitespace,
        Fault::SemicolonRejected,
        Fault::WhereNullIsTrue,
        Fault::WrongErrorClass,
        Fault::TypeNotEnforced,
        Fault::HeadersUppercased,
        Fault::LengthOffByOne,
        Fault::CoalesceFirstOnly,
    ];
}

type R<T> = Result<T, ErrClass>;

// ----- lexer ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Id(String),
    Int(i64),
    Str(String),
    Sym(&'static str),
}

fn lex(s: &str, fault: Option<Fault>) -> R<Vec<Tok>> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' => i += 1,
            '\t' => {
                if fault == Some(Fault::TabNotWhitespace) {
                    return Err(ErrClass::Syntax);
                }
                i += 1;
            }
            '\'' => {
                let mut text = String::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None => return Err(ErrClass::Syntax),
                        Some('\'') if b.get(i + 1) == Some(&'\'') => {
                            text.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&ch) => {
                            text.push(ch);
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Str(text));
            }
            '0'..='9' => {
                let st = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let digits: String = b[st..i].iter().collect();
                out.push(Tok::Int(digits.parse().map_err(|_| ErrClass::Syntax)?));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let st = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_') {
                    i += 1;
                }
                out.push(Tok::Id(b[st..i].iter().collect()));
            }
            ';' => {
                if fault == Some(Fault::SemicolonRejected) {
                    return Err(ErrClass::Syntax);
                }
                out.push(Tok::Sym(";"));
                i += 1;
            }
            _ => {
                let two: String = b[i..(i + 2).min(b.len())].iter().collect();
                let sym = match two.as_str() {
                    "<>" => Some("<>"),
                    "!=" => Some("!="),
                    "<=" => Some("<="),
                    ">=" => Some(">="),
                    "||" => Some("||"),
                    _ => None,
                };
                if let Some(s2) = sym {
                    out.push(Tok::Sym(s2));
                    i += 2;
                    continue;
                }
                let one = match c {
                    '(' => "(",
                    ')' => ")",
                    ',' => ",",
                    '*' => "*",
                    '+' => "+",
                    '-' => "-",
                    '/' => "/",
                    '%' => "%",
                    '=' => "=",
                    '<' => "<",
                    '>' => ">",
                    _ => return Err(ErrClass::Syntax),
                };
                out.push(Tok::Sym(one));
                i += 1;
            }
        }
    }
    Ok(out)
}

// ----- AST -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Cat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone)]
enum Expr {
    Lit(V),
    Col(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    Func(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Int,
    Text,
}

#[derive(Debug)]
enum Items {
    Star,
    List(Vec<(Expr, Option<String>)>),
}

#[derive(Debug)]
struct Sel {
    distinct: bool,
    items: Items,
    from: Option<String>,
    wh: Option<Expr>,
    order: Vec<(String, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug)]
enum Stmt {
    Create(String, Vec<(String, Ty)>),
    Insert(String, Option<Vec<String>>, Vec<Vec<Expr>>),
    Select(Sel),
    Update(String, Vec<(String, Expr)>, Option<Expr>),
    Delete(String, Option<Expr>),
}

const RESERVED: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "FROM", "WHERE", "ORDER", "BY", "LIMIT",
    "OFFSET", "DISTINCT", "VALUES", "SET", "INTO", "AS", "AND", "OR", "NOT", "IS", "ASC", "DESC",
    "TABLE",
];

// ----- parser ----------------------------------------------------------------

struct P {
    toks: Vec<Tok>,
    pos: usize,
    fault: Option<Fault>,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn is_kw(&self, w: &str) -> bool {
        match self.peek() {
            Some(Tok::Id(s)) => {
                if self.fault == Some(Fault::CaseSensitiveKeywords) {
                    s == w
                } else {
                    s.eq_ignore_ascii_case(w)
                }
            }
            _ => false,
        }
    }

    fn eat_kw(&mut self, w: &str) -> bool {
        if self.is_kw(w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, w: &str) -> R<()> {
        if self.eat_kw(w) {
            Ok(())
        } else {
            Err(ErrClass::Syntax)
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if *x == s)
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: &str) -> R<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(ErrClass::Syntax)
        }
    }

    fn ident(&mut self) -> R<String> {
        match self.peek() {
            Some(Tok::Id(s)) if !RESERVED.iter().any(|r| s.eq_ignore_ascii_case(r)) => {
                let s = s.to_ascii_lowercase();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(ErrClass::Syntax),
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    // statements

    fn statement(&mut self) -> R<Stmt> {
        let st = if self.eat_kw("CREATE") {
            self.create()?
        } else if self.eat_kw("INSERT") {
            self.insert()?
        } else if self.eat_kw("SELECT") {
            Stmt::Select(self.select()?)
        } else if self.eat_kw("UPDATE") {
            self.update()?
        } else if self.eat_kw("DELETE") {
            self.delete()?
        } else {
            return Err(ErrClass::Syntax);
        };
        self.eat_sym(";");
        if self.at_end() {
            Ok(st)
        } else {
            Err(ErrClass::Syntax)
        }
    }

    fn create(&mut self) -> R<Stmt> {
        self.expect_kw("TABLE")?;
        let name = self.ident()?;
        self.expect_sym("(")?;
        let mut cols = Vec::new();
        loop {
            let c = self.ident()?;
            let ty = if self.eat_kw("INTEGER") {
                Ty::Int
            } else if self.eat_kw("TEXT") {
                Ty::Text
            } else {
                return Err(ErrClass::Syntax);
            };
            cols.push((c, ty));
            if self.eat_sym(",") {
                continue;
            }
            self.expect_sym(")")?;
            break;
        }
        Ok(Stmt::Create(name, cols))
    }

    fn insert(&mut self) -> R<Stmt> {
        self.expect_kw("INTO")?;
        let table = self.ident()?;
        let mut cols = None;
        if self.eat_sym("(") {
            let mut v = vec![self.ident()?];
            while self.eat_sym(",") {
                v.push(self.ident()?);
            }
            self.expect_sym(")")?;
            cols = Some(v);
        }
        self.expect_kw("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect_sym("(")?;
            let mut row = vec![self.expr()?];
            while self.eat_sym(",") {
                row.push(self.expr()?);
            }
            self.expect_sym(")")?;
            rows.push(row);
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(Stmt::Insert(table, cols, rows))
    }

    fn select(&mut self) -> R<Sel> {
        let distinct = self.eat_kw("DISTINCT");
        let items = if self.eat_sym("*") {
            Items::Star
        } else {
            let mut v = Vec::new();
            loop {
                let e = self.expr()?;
                let alias = if self.eat_kw("AS") {
                    Some(self.ident()?)
                } else {
                    None
                };
                v.push((e, alias));
                if !self.eat_sym(",") {
                    break;
                }
            }
            Items::List(v)
        };
        let mut sel = Sel {
            distinct,
            items,
            from: None,
            wh: None,
            order: Vec::new(),
            limit: None,
            offset: None,
        };
        if self.eat_kw("FROM") {
            sel.from = Some(self.ident()?);
            if self.eat_kw("WHERE") {
                sel.wh = Some(self.expr()?);
            }
            if self.eat_kw("ORDER") {
                self.expect_kw("BY")?;
                loop {
                    let name = self.ident()?;
                    let desc = if self.eat_kw("DESC") {
                        true
                    } else {
                        self.eat_kw("ASC");
                        false
                    };
                    sel.order.push((name, desc));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            if self.eat_kw("LIMIT") {
                sel.limit = Some(self.int_lit()?);
                if self.eat_kw("OFFSET") {
                    sel.offset = Some(self.int_lit()?);
                }
            }
        }
        Ok(sel)
    }

    fn int_lit(&mut self) -> R<i64> {
        match self.peek() {
            Some(Tok::Int(n)) => {
                let n = *n;
                self.pos += 1;
                Ok(n)
            }
            _ => Err(ErrClass::Syntax),
        }
    }

    fn update(&mut self) -> R<Stmt> {
        let table = self.ident()?;
        self.expect_kw("SET")?;
        let mut sets = Vec::new();
        loop {
            let c = self.ident()?;
            self.expect_sym("=")?;
            sets.push((c, self.expr()?));
            if !self.eat_sym(",") {
                break;
            }
        }
        let wh = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Update(table, sets, wh))
    }

    fn delete(&mut self) -> R<Stmt> {
        self.expect_kw("FROM")?;
        let table = self.ident()?;
        let wh = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Delete(table, wh))
    }

    // expressions: OR < AND < NOT < (= <> != IS) < relational < + - < * / %
    // < || < unary minus (docs/PROPOSAL-sdb-sql.md, operator precedence)

    fn expr(&mut self) -> R<Expr> {
        let swapped = self.fault == Some(Fault::OrBindsTighterThanAnd);
        let (lo, hi) = if swapped {
            ("AND", "OR")
        } else {
            ("OR", "AND")
        };
        let mut l = self.level_mid(hi)?;
        while self.eat_kw(lo) {
            let r = self.level_mid(hi)?;
            l = Expr::Bin(op_of(lo), Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn level_mid(&mut self, kw: &str) -> R<Expr> {
        let mut l = self.not_level()?;
        while self.eat_kw(kw) {
            let r = self.not_level()?;
            l = Expr::Bin(op_of(kw), Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not_level(&mut self) -> R<Expr> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.not_level()?)));
        }
        self.eq_level()
    }

    fn eq_level(&mut self) -> R<Expr> {
        let mut l = self.rel_level()?;
        loop {
            let op = if self.eat_sym("=") {
                Op::Eq
            } else if self.eat_sym("<>") || self.eat_sym("!=") {
                Op::Ne
            } else if self.is_kw("IS") {
                self.pos += 1;
                let neg = self.eat_kw("NOT");
                if !matches!(self.peek(), Some(Tok::Id(s)) if s.eq_ignore_ascii_case("NULL")) {
                    return Err(ErrClass::Syntax);
                }
                self.pos += 1;
                l = Expr::IsNull(Box::new(l), neg);
                continue;
            } else {
                return Ok(l);
            };
            let r = self.rel_level()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn rel_level(&mut self) -> R<Expr> {
        let mut l = self.add_level()?;
        loop {
            let op = if self.eat_sym("<=") {
                Op::Le
            } else if self.eat_sym(">=") {
                Op::Ge
            } else if self.eat_sym("<") {
                Op::Lt
            } else if self.eat_sym(">") {
                Op::Gt
            } else {
                return Ok(l);
            };
            let r = self.add_level()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn add_level(&mut self) -> R<Expr> {
        let mut l = self.mul_level()?;
        loop {
            if self.eat_sym("+") {
                let r = self.mul_level()?;
                l = Expr::Bin(Op::Add, Box::new(l), Box::new(r));
            } else if self.eat_sym("-") {
                let r = if self.fault == Some(Fault::MinusIsRightAssociative) {
                    self.add_level()?
                } else {
                    self.mul_level()?
                };
                l = Expr::Bin(Op::Sub, Box::new(l), Box::new(r));
            } else {
                return Ok(l);
            }
        }
    }

    fn mul_level(&mut self) -> R<Expr> {
        let mut l = self.cat_level()?;
        loop {
            let op = if self.eat_sym("*") {
                Op::Mul
            } else if self.eat_sym("/") {
                Op::Div
            } else if self.eat_sym("%") {
                Op::Mod
            } else {
                return Ok(l);
            };
            let r = self.cat_level()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn cat_level(&mut self) -> R<Expr> {
        let mut l = self.unary()?;
        while self.eat_sym("||") {
            let r = self.unary()?;
            l = Expr::Bin(Op::Cat, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary(&mut self) -> R<Expr> {
        if self.eat_sym("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> R<Expr> {
        match self.peek().cloned() {
            Some(Tok::Int(n)) => {
                self.pos += 1;
                Ok(Expr::Lit(V::Int(n)))
            }
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Expr::Lit(V::Text(s)))
            }
            Some(Tok::Sym("(")) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Some(Tok::Id(s)) => {
                if s.eq_ignore_ascii_case("NULL") {
                    self.pos += 1;
                    return Ok(Expr::Lit(V::Null));
                }
                if RESERVED.iter().any(|r| s.eq_ignore_ascii_case(r)) {
                    return Err(ErrClass::Syntax);
                }
                self.pos += 1;
                if self.eat_sym("(") {
                    let mut args = Vec::new();
                    if !self.is_sym(")") {
                        args.push(self.expr()?);
                        while self.eat_sym(",") {
                            args.push(self.expr()?);
                        }
                    }
                    self.expect_sym(")")?;
                    return Ok(Expr::Func(s.to_ascii_lowercase(), args));
                }
                Ok(Expr::Col(s.to_ascii_lowercase()))
            }
            _ => Err(ErrClass::Syntax),
        }
    }
}

fn op_of(kw: &str) -> Op {
    if kw == "AND" {
        Op::And
    } else {
        Op::Or
    }
}

// ----- evaluation --------------------------------------------------------------

fn rank(v: &V) -> u8 {
    match v {
        V::Null => 0,
        V::Int(_) => 1,
        V::Text(_) => 2,
    }
}

/// Total order used for sorting and comparison: NULL < integers < text.
fn cmp_vals(a: &V, b: &V) -> Ordering {
    match (a, b) {
        (V::Int(x), V::Int(y)) => x.cmp(y),
        (V::Text(x), V::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => rank(a).cmp(&rank(b)),
    }
}

fn truthy(v: &V) -> bool {
    matches!(v, V::Int(i) if *i != 0)
}

fn as_text(v: &V) -> String {
    match v {
        V::Text(t) => t.clone(),
        V::Int(i) => i.to_string(),
        V::Null => String::new(),
    }
}

struct Ctx<'a> {
    fault: Option<Fault>,
    cols: &'a [(String, Ty)],
    row: &'a [V],
}

fn eval(e: &Expr, cx: &Ctx) -> R<V> {
    let two = cx.fault == Some(Fault::TwoValuedLogic);
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Col(name) => {
            let i = cx
                .cols
                .iter()
                .position(|(c, _)| c == name)
                .ok_or(ErrClass::NoColumn)?;
            cx.row[i].clone()
        }
        Expr::Neg(x) => match eval(x, cx)? {
            V::Null => V::Null,
            V::Int(i) => V::Int(i.checked_neg().ok_or(ErrClass::Limit)?),
            V::Text(_) => return Err(ErrClass::Type),
        },
        Expr::Not(x) => match eval(x, cx)? {
            V::Null if !two => V::Null,
            v => V::Int(i64::from(!truthy(&v))),
        },
        Expr::IsNull(x, neg) => {
            let n = matches!(eval(x, cx)?, V::Null);
            V::Int(i64::from(n != *neg))
        }
        Expr::Func(name, args) => func(name, args, cx)?,
        Expr::Bin(op, l, r) => {
            let (a, b) = (eval(l, cx)?, eval(r, cx)?);
            bin(*op, a, b, cx.fault)?
        }
    })
}

fn bin(op: Op, a: V, b: V, fault: Option<Fault>) -> R<V> {
    let two = fault == Some(Fault::TwoValuedLogic);
    match op {
        Op::And => {
            let (ta, tb) = (truthy(&a), truthy(&b));
            if two {
                return Ok(V::Int(i64::from(ta && tb)));
            }
            let false_a = !matches!(a, V::Null) && !ta;
            let false_b = !matches!(b, V::Null) && !tb;
            Ok(if false_a || false_b {
                V::Int(0)
            } else if matches!(a, V::Null) || matches!(b, V::Null) {
                V::Null
            } else {
                V::Int(1)
            })
        }
        Op::Or => {
            let (ta, tb) = (truthy(&a), truthy(&b));
            if two {
                return Ok(V::Int(i64::from(ta || tb)));
            }
            Ok(if ta || tb {
                V::Int(1)
            } else if matches!(a, V::Null) || matches!(b, V::Null) {
                V::Null
            } else {
                V::Int(0)
            })
        }
        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            if matches!(a, V::Null) || matches!(b, V::Null) {
                return Ok(if two { V::Int(0) } else { V::Null });
            }
            let o = cmp_vals(&a, &b);
            let t = match op {
                Op::Eq => o == Ordering::Equal,
                Op::Ne => o != Ordering::Equal,
                Op::Lt => o == Ordering::Less,
                Op::Le => o != Ordering::Greater,
                Op::Gt => o == Ordering::Greater,
                _ => o != Ordering::Less,
            };
            Ok(V::Int(i64::from(t)))
        }
        Op::Cat => {
            let empty = fault == Some(Fault::ConcatNullAsEmpty);
            if !empty && (matches!(a, V::Null) || matches!(b, V::Null)) {
                return Ok(V::Null);
            }
            Ok(V::Text(format!("{}{}", as_text(&a), as_text(&b))))
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => {
            if matches!(a, V::Null) || matches!(b, V::Null) {
                return Ok(V::Null);
            }
            let (V::Int(x), V::Int(y)) = (a, b) else {
                return Err(ErrClass::Type);
            };
            let lim = || ErrClass::Limit;
            Ok(match op {
                Op::Add => V::Int(x.checked_add(y).ok_or_else(lim)?),
                Op::Sub => V::Int(x.checked_sub(y).ok_or_else(lim)?),
                Op::Mul => V::Int(x.checked_mul(y).ok_or_else(lim)?),
                Op::Div => {
                    if y == 0 {
                        return zero(fault);
                    }
                    let q = x.checked_div(y).ok_or_else(lim)?;
                    let floor =
                        fault == Some(Fault::FloorDivision) && x % y != 0 && ((x < 0) != (y < 0));
                    V::Int(if floor { q - 1 } else { q })
                }
                _ => {
                    if y == 0 {
                        return zero(fault);
                    }
                    let r = x.checked_rem(y).ok_or_else(lim)?;
                    let flip = fault == Some(Fault::ModuloSign) && r != 0 && ((r < 0) != (y < 0));
                    V::Int(if flip { r + y } else { r })
                }
            })
        }
    }
}

fn zero(fault: Option<Fault>) -> R<V> {
    if fault == Some(Fault::DivideByZeroErrors) {
        Err(ErrClass::Limit)
    } else {
        Ok(V::Null)
    }
}

fn func(name: &str, args: &[Expr], cx: &Ctx) -> R<V> {
    let want = match name {
        "length" | "upper" | "lower" | "abs" => 1,
        "coalesce" => 2,
        _ => return Err(ErrClass::NoColumn),
    };
    if args.len() < want || (name != "coalesce" && args.len() != want) {
        return Err(ErrClass::Arity);
    }
    let vals: Vec<V> = args.iter().map(|a| eval(a, cx)).collect::<R<_>>()?;
    Ok(match name {
        "length" => match &vals[0] {
            V::Null => V::Null,
            v => {
                let n = as_text(v).chars().count() as i64;
                V::Int(if cx.fault == Some(Fault::LengthOffByOne) {
                    n + 1
                } else {
                    n
                })
            }
        },
        "upper" => match &vals[0] {
            V::Null => V::Null,
            v => V::Text(as_text(v).to_ascii_uppercase()),
        },
        "lower" => match &vals[0] {
            V::Null => V::Null,
            v => V::Text(as_text(v).to_ascii_lowercase()),
        },
        "abs" => match &vals[0] {
            V::Null => V::Null,
            V::Int(i) => V::Int(i.checked_abs().ok_or(ErrClass::Limit)?),
            V::Text(_) => return Err(ErrClass::Type),
        },
        _ => {
            if cx.fault == Some(Fault::CoalesceFirstOnly) {
                vals[0].clone()
            } else {
                vals.iter()
                    .find(|v| !matches!(v, V::Null))
                    .cloned()
                    .unwrap_or(V::Null)
            }
        }
    })
}

/// Every column an expression names must exist (checked at "prepare" time,
/// so a missing column errors even when the table is empty).
fn check_cols(e: &Expr, cols: &[(String, Ty)]) -> R<()> {
    match e {
        Expr::Lit(_) => Ok(()),
        Expr::Col(n) => {
            if cols.iter().any(|(c, _)| c == n) {
                Ok(())
            } else {
                Err(ErrClass::NoColumn)
            }
        }
        Expr::Neg(x) | Expr::Not(x) | Expr::IsNull(x, _) => check_cols(x, cols),
        Expr::Bin(_, l, r) => {
            check_cols(l, cols)?;
            check_cols(r, cols)
        }
        Expr::Func(name, args) => {
            if !matches!(
                name.as_str(),
                "length" | "upper" | "lower" | "abs" | "coalesce"
            ) {
                return Err(ErrClass::NoColumn);
            }
            args.iter().try_for_each(|a| check_cols(a, cols))
        }
    }
}

// ----- engine ----------------------------------------------------------------------

struct Tbl {
    cols: Vec<(String, Ty)>,
    rows: Vec<Vec<V>>,
}

pub struct Toy {
    tables: BTreeMap<String, Tbl>,
    fault: Option<Fault>,
}

impl Toy {
    pub fn new(fault: Option<Fault>) -> Self {
        Toy {
            tables: BTreeMap::new(),
            fault,
        }
    }

    fn coerce(&self, v: V, ty: Ty) -> R<V> {
        Ok(match (v, ty) {
            (V::Null, _) => V::Null,
            (V::Int(i), Ty::Int) => V::Int(i),
            (V::Text(t), Ty::Text) => V::Text(t),
            (V::Int(i), Ty::Text) => V::Text(i.to_string()),
            (V::Text(t), Ty::Int) => {
                if self.fault == Some(Fault::TypeNotEnforced) {
                    V::Text(t)
                } else {
                    V::Int(t.parse::<i64>().map_err(|_| ErrClass::Type)?)
                }
            }
        })
    }

    fn run_inner(&mut self, sql: &str) -> R<Response> {
        let toks = lex(sql, self.fault)?;
        let mut p = P {
            toks,
            pos: 0,
            fault: self.fault,
        };
        match p.statement()? {
            Stmt::Create(name, cols) => {
                if self.tables.contains_key(&name) {
                    return Err(ErrClass::Exists);
                }
                self.tables.insert(
                    name,
                    Tbl {
                        cols,
                        rows: Vec::new(),
                    },
                );
                Ok(Response::Ok(0))
            }
            Stmt::Insert(name, cols, rows) => self.insert(&name, cols, rows),
            Stmt::Select(sel) => self.select(sel),
            Stmt::Update(name, sets, wh) => self.update(&name, sets, wh),
            Stmt::Delete(name, wh) => self.delete(&name, wh),
        }
    }

    fn insert(
        &mut self,
        name: &str,
        cols: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    ) -> R<Response> {
        let tbl = self.tables.get(name).ok_or(ErrClass::NoTable)?;
        let schema = tbl.cols.clone();
        let targets: Vec<usize> = match &cols {
            None => (0..schema.len()).collect(),
            Some(names) => names
                .iter()
                .map(|n| {
                    schema
                        .iter()
                        .position(|(c, _)| c == n)
                        .ok_or(ErrClass::NoColumn)
                })
                .collect::<R<_>>()?,
        };
        let mut built: Vec<Vec<V>> = Vec::new();
        let mut failure: Option<ErrClass> = None;
        for row in &rows {
            let step = (|| -> R<Vec<V>> {
                if row.len() != targets.len() {
                    return Err(ErrClass::Arity);
                }
                let mut out = vec![V::Null; schema.len()];
                for (e, &ti) in row.iter().zip(&targets) {
                    check_cols(e, &[])?;
                    let v = eval(
                        e,
                        &Ctx {
                            fault: self.fault,
                            cols: &[],
                            row: &[],
                        },
                    )?;
                    out[ti] = self.coerce(v, schema[ti].1)?;
                }
                Ok(out)
            })();
            match step {
                Ok(r) => built.push(r),
                Err(c) => {
                    failure = Some(c);
                    break;
                }
            }
        }
        let tbl = self.tables.get_mut(name).ok_or(ErrClass::NoTable)?;
        match failure {
            Some(c) => {
                if self.fault == Some(Fault::InsertNotAtomic) {
                    tbl.rows.extend(built);
                }
                Err(c)
            }
            None => {
                let n = built.len() as u64;
                tbl.rows.extend(built);
                Ok(Response::Ok(n))
            }
        }
    }

    fn keep(&self, wh: &Option<Expr>, cols: &[(String, Ty)], row: &[V]) -> R<bool> {
        match wh {
            None => Ok(true),
            Some(e) => {
                let v = eval(
                    e,
                    &Ctx {
                        fault: self.fault,
                        cols,
                        row,
                    },
                )?;
                Ok(truthy(&v) || (self.fault == Some(Fault::WhereNullIsTrue) && v == V::Null))
            }
        }
    }

    fn select(&self, sel: Sel) -> R<Response> {
        let (cols, rows): (Vec<(String, Ty)>, Vec<Vec<V>>) = match &sel.from {
            Some(t) => {
                let tbl = self.tables.get(t).ok_or(ErrClass::NoTable)?;
                (tbl.cols.clone(), tbl.rows.clone())
            }
            None => (Vec::new(), vec![Vec::new()]),
        };
        if let Some(w) = &sel.wh {
            check_cols(w, &cols)?;
        }
        // Output columns: (header, expression).
        let mut outputs: Vec<(String, Expr)> = Vec::new();
        match &sel.items {
            Items::Star => {
                if sel.from.is_none() {
                    return Err(ErrClass::Syntax);
                }
                let mut v: Vec<(String, Expr)> = cols
                    .iter()
                    .map(|(c, _)| (c.clone(), Expr::Col(c.clone())))
                    .collect();
                if self.fault == Some(Fault::StarReversed) {
                    v.reverse();
                }
                outputs = v;
            }
            Items::List(items) => {
                for (e, alias) in items {
                    check_cols(e, &cols)?;
                    let header = match (alias, e) {
                        (Some(a), _) => a.clone(),
                        (None, Expr::Col(c)) => c.clone(),
                        _ => "?".to_string(),
                    };
                    outputs.push((header, e.clone()));
                }
            }
        }
        let mut headers: Vec<String> = outputs.iter().map(|(h, _)| h.clone()).collect();
        if self.fault == Some(Fault::HeadersUppercased) {
            headers = headers.iter().map(|h| h.to_ascii_uppercase()).collect();
        }
        // Resolve ORDER BY against the output names.
        let mut keys: Vec<(usize, bool)> = Vec::new();
        for (name, desc) in &sel.order {
            let i = outputs
                .iter()
                .position(|(h, _)| h == name)
                .ok_or(ErrClass::NoColumn)?;
            keys.push((i, *desc));
        }
        let mut out_rows: Vec<Vec<V>> = Vec::new();
        for row in &rows {
            if !self.keep(&sel.wh, &cols, row)? {
                continue;
            }
            let cx = Ctx {
                fault: self.fault,
                cols: &cols,
                row,
            };
            let mut o = Vec::with_capacity(outputs.len());
            for (_, e) in &outputs {
                o.push(eval(e, &cx)?);
            }
            out_rows.push(o);
        }
        if sel.distinct {
            let nulls_distinct = self.fault == Some(Fault::DistinctKeepsNulls);
            let mut seen: Vec<Vec<V>> = Vec::new();
            out_rows.retain(|r| {
                if nulls_distinct && r.iter().any(|v| matches!(v, V::Null)) {
                    return true;
                }
                if seen.contains(r) {
                    false
                } else {
                    seen.push(r.clone());
                    true
                }
            });
        }
        if !keys.is_empty() {
            let nulls_last = self.fault == Some(Fault::NullsLast);
            out_rows.sort_by(|a, b| {
                for &(i, desc) in &keys {
                    let mut o = if nulls_last {
                        match (&a[i], &b[i]) {
                            (V::Null, V::Null) => Ordering::Equal,
                            (V::Null, _) => Ordering::Greater,
                            (_, V::Null) => Ordering::Less,
                            (x, y) => cmp_vals(x, y),
                        }
                    } else {
                        cmp_vals(&a[i], &b[i])
                    };
                    if desc {
                        o = o.reverse();
                    }
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            });
        }
        let offset = if self.fault == Some(Fault::OffsetIgnored) {
            0
        } else {
            sel.offset.unwrap_or(0) as usize
        };
        let mut rows_final: Vec<Vec<V>> = out_rows.into_iter().skip(offset).collect();
        if let Some(l) = sel.limit {
            let extra = i64::from(self.fault == Some(Fault::LimitPlusOne));
            rows_final.truncate((l + extra) as usize);
        }
        let raw = self.fault == Some(Fault::NoWireEscaping);
        let enc = |v: &V| -> String {
            match v {
                V::Text(t) if raw => t.clone(),
                V::Text(t) => encode_text(t),
                other => encode_value(other),
            }
        };
        Ok(Response::Rows {
            ncols: headers.len(),
            headers,
            end_count: rows_final.len() as u64,
            rows: rows_final
                .iter()
                .map(|r| r.iter().map(&enc).collect())
                .collect(),
        })
    }

    fn update(&mut self, name: &str, sets: Vec<(String, Expr)>, wh: Option<Expr>) -> R<Response> {
        let tbl = self.tables.get(name).ok_or(ErrClass::NoTable)?;
        let cols = tbl.cols.clone();
        let rows = tbl.rows.clone();
        let mut targets = Vec::new();
        for (c, e) in &sets {
            let i = cols
                .iter()
                .position(|(n, _)| n == c)
                .ok_or(ErrClass::NoColumn)?;
            check_cols(e, &cols)?;
            targets.push(i);
        }
        if let Some(w) = &wh {
            check_cols(w, &cols)?;
        }
        let mut new_rows = rows.clone();
        let (mut matched, mut changed) = (0u64, 0u64);
        for (ri, row) in rows.iter().enumerate() {
            if !self.keep(&wh, &cols, row)? {
                continue;
            }
            matched += 1;
            let mut updated = row.clone();
            for ((_, e), &ti) in sets.iter().zip(&targets) {
                // Old-row semantics: every right-hand side reads the
                // original row, unless the fault makes it read the
                // partially updated one.
                let basis: &[V] = if self.fault == Some(Fault::UpdateSeesNewValues) {
                    &updated
                } else {
                    row
                };
                let v = eval(
                    e,
                    &Ctx {
                        fault: self.fault,
                        cols: &cols,
                        row: basis,
                    },
                )?;
                updated[ti] = self.coerce(v, cols[ti].1)?;
            }
            if updated != *row {
                changed += 1;
            }
            new_rows[ri] = updated;
        }
        self.tables.get_mut(name).ok_or(ErrClass::NoTable)?.rows = new_rows;
        Ok(Response::Ok(
            if self.fault == Some(Fault::UpdateCountsChangedOnly) {
                changed
            } else {
                matched
            },
        ))
    }

    fn delete(&mut self, name: &str, wh: Option<Expr>) -> R<Response> {
        let tbl = self.tables.get(name).ok_or(ErrClass::NoTable)?;
        let cols = tbl.cols.clone();
        let rows = tbl.rows.clone();
        if let Some(w) = &wh {
            check_cols(w, &cols)?;
        }
        let mut kept = Vec::new();
        let mut removed = 0u64;
        for row in rows {
            if self.keep(&wh, &cols, &row)? {
                removed += 1;
            } else {
                kept.push(row);
            }
        }
        if self.fault == Some(Fault::DeleteIgnoresWhere) && wh.is_some() {
            let t = self.tables.get_mut(name).ok_or(ErrClass::NoTable)?;
            removed = t.rows.len() as u64;
            t.rows.clear();
            return Ok(Response::Ok(removed));
        }
        self.tables.get_mut(name).ok_or(ErrClass::NoTable)?.rows = kept;
        Ok(Response::Ok(removed))
    }
}

impl Candidate for Toy {
    fn exec(&mut self, sql: &str) -> Result<Response, String> {
        Ok(match self.run_inner(sql) {
            Ok(r) => r,
            Err(c) => {
                let c = if self.fault == Some(Fault::WrongErrorClass) && c == ErrClass::NoColumn {
                    ErrClass::NoTable
                } else {
                    c
                };
                Response::Err {
                    class: c.token().to_string(),
                    text: String::new(),
                }
            }
        })
    }
}

// ----- tests ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verifier::sql::generator::{build_script, Profile, Statement};
    use crate::verifier::sql::run::{run_script, Verdict};
    use crate::verifier::sql::shrink::{first_failure, shrink};

    const SEEDS: u64 = 120;
    const STEPS: usize = 150;

    fn all_statements(seed: u64, p: &Profile) -> Vec<Statement> {
        let s = build_script(seed, p, STEPS);
        s.setup
            .iter()
            .chain(&s.steps)
            .map(|c| c.stmt.clone())
            .collect()
    }

    /// The independent toy agrees with the SQLite oracle on every generated
    /// script. A failure here is a spec gap or an SQLite quirk the dialect
    /// does not capture, not a candidate bug.
    #[test]
    fn clean_toy_agrees_with_the_oracle_on_every_script() {
        for last in [0u32, 1, 2, 3] {
            let p = Profile::through(last);
            for seed in 0..SEEDS {
                let script = build_script(seed, &p, STEPS);
                let mut toy = Toy::new(None);
                match run_script(&mut toy, &script) {
                    Verdict::Pass { .. } => {}
                    Verdict::Fail(f) => panic!(
                        "stage {last} seed {seed}: statement {} {:?}\n  {:?}\n  expected {:?}\n  got {:?}",
                        f.index, f.stmt.sql, f.mismatch, f.expected, f.got
                    ),
                }
            }
        }
    }

    /// Long soak of the same agreement check (run on demand:
    /// `cargo test --lib clean_toy_soak -- --ignored`). Rare divergences,
    /// such as an overflow path the bounds miss, only show up at volume.
    #[test]
    #[ignore]
    fn clean_toy_soak() {
        let p = Profile::through(3);
        for seed in 1000..4000u64 {
            let script = build_script(seed, &p, 300);
            let mut toy = Toy::new(None);
            if let Verdict::Fail(f) = run_script(&mut toy, &script) {
                panic!(
                    "seed {seed}: statement {} {:?}\n  {:?}\n  expected {:?}\n  got {:?}",
                    f.index, f.stmt.sql, f.mismatch, f.expected, f.got
                );
            }
        }
    }

    /// Every planted bug is caught, and caught OFTEN: a blind spot in the
    /// generator shows up here as a surviving or barely-caught mutant. A
    /// model with such a bug would otherwise pass several verifications in a
    /// row by luck.
    #[test]
    fn every_mutant_is_caught_in_most_scripts() {
        const SCRIPTS: u64 = 40;
        const MIN_RATE_PCT: u64 = 25;
        let p = Profile::through(3);
        let mut weak = Vec::new();
        let mut report = Vec::new();
        for fault in Fault::ALL {
            let mut caught = 0u64;
            for seed in 0..SCRIPTS {
                let script = build_script(seed, &p, STEPS);
                let mut toy = Toy::new(Some(fault));
                if let Verdict::Fail(_) = run_script(&mut toy, &script) {
                    caught += 1;
                }
            }
            report.push(format!("{fault:?}: {caught}/{SCRIPTS}"));
            if caught * 100 < SCRIPTS * MIN_RATE_PCT {
                weak.push(format!("{fault:?} ({caught}/{SCRIPTS})"));
            }
        }
        assert!(
            weak.is_empty(),
            "mutants caught in under {MIN_RATE_PCT}% of scripts: {weak:?}\n{}",
            report.join("\n")
        );
    }

    fn make(fault: Fault) -> impl FnMut() -> Box<dyn Candidate> {
        move || Box::new(Toy::new(Some(fault)))
    }

    #[test]
    fn shrink_reduces_a_null_ordering_bug_to_a_tiny_repro() {
        let p = Profile::through(3);
        let mut best = None;
        for seed in 0..SEEDS {
            let stmts = all_statements(seed, &p);
            let mut mk = make(Fault::NullsLast);
            if first_failure(&stmts, &mut mk).is_some() {
                best = Some(stmts);
                break;
            }
        }
        let stmts = best.expect("NullsLast must fail some script");
        let mut mk = make(Fault::NullsLast);
        let repro = shrink(&stmts, &mut mk).expect("a failing script shrinks");
        assert!(
            repro.statements.len() <= 5,
            "repro has {} statements:\n{}",
            repro.statements.len(),
            repro.render()
        );
        assert!(repro.statements.len() < stmts.len());
        // The repro is self-contained and still fails.
        let mut mk = make(Fault::NullsLast);
        assert!(first_failure(&repro.statements, &mut mk).is_some());
        // ... and passes on a correct engine.
        let mut ok = || -> Box<dyn Candidate> { Box::new(Toy::new(None)) };
        assert!(first_failure(&repro.statements, &mut ok).is_none());
        let text = repro.render();
        assert!(
            text.contains("Expected:") && text.contains("Got:"),
            "{text}"
        );
    }

    #[test]
    fn shrink_keeps_what_the_swap_update_bug_needs() {
        let p = Profile::through(3);
        for seed in 0..400u64 {
            let stmts = all_statements(seed, &p);
            let mut mk = make(Fault::UpdateSeesNewValues);
            let Some(found) = first_failure(&stmts, &mut mk) else {
                continue;
            };
            let repro = shrink(&stmts, &mut mk).unwrap();
            assert_eq!(repro.kind, found.mismatch.kind);
            let sql: Vec<String> = repro
                .statements
                .iter()
                .map(|s| s.sql.to_ascii_lowercase())
                .collect();
            assert!(sql.iter().any(|s| s.contains("create table")), "{sql:?}");
            assert!(sql.iter().any(|s| s.contains("update")), "{sql:?}");
            assert!(repro.statements.len() <= 6, "{}", repro.render());
            return;
        }
        panic!("UpdateSeesNewValues never failed in 400 seeds");
    }

    #[test]
    fn shrink_of_a_passing_sequence_is_none() {
        let stmts = all_statements(3, &Profile::through(3));
        let mut ok = || -> Box<dyn Candidate> { Box::new(Toy::new(None)) };
        assert!(shrink(&stmts, &mut ok).is_none());
    }

    /// The gate on the oracle itself: an in-memory and a file-backed SQLite
    /// answer every generated statement identically (rows compared as a
    /// multiset unless the statement orders every output column), and no
    /// message falls outside the class table.
    #[test]
    fn oracle_memory_and_file_agree_on_100k_statements() {
        use crate::verifier::sql::generator::{Generator, Kind};
        use crate::verifier::sql::oracle::{Oracle, OracleOutcome};
        use crate::verifier::sql::schema;
        let dir = std::env::temp_dir().join(format!("sdb-sql-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = Profile::through(3);
        let (mut checked, mut seed) = (0usize, 0u64);
        while checked < 100_000 {
            let schema = schema::generate(seed, 4, 5);
            let mut gen = Generator::new(seed, schema, p.clone());
            let mem = Oracle::new().unwrap();
            let file = Oracle::new_file(&dir.join(format!("g{seed}.db"))).unwrap();
            let mut stmts: Vec<Statement> = gen
                .setup_statements()
                .into_iter()
                .map(|sql| Statement {
                    sql,
                    kind: Kind::Create,
                    ordered: false,
                })
                .collect();
            for _ in 0..400 {
                stmts.push(gen.next_statement());
            }
            for st in &stmts {
                let (a, b) = (mem.exec(&st.sql), file.exec(&st.sql));
                assert!(
                    !matches!(a, OracleOutcome::Unknown(_)),
                    "seed {seed}: unmapped message for {:?}: {a:?}",
                    st.sql
                );
                let same = match (&a, &b) {
                    (
                        OracleOutcome::Rows {
                            headers: ha,
                            rows: ra,
                        },
                        OracleOutcome::Rows {
                            headers: hb,
                            rows: rb,
                        },
                    ) if !st.ordered => {
                        let key = |r: &Vec<crate::verifier::sql::oracle::Value>| format!("{r:?}");
                        let mut x: Vec<String> = ra.iter().map(key).collect();
                        let mut y: Vec<String> = rb.iter().map(key).collect();
                        x.sort();
                        y.sort();
                        ha == hb && x == y
                    }
                    _ => a == b,
                };
                assert!(same, "seed {seed}: {:?}\n mem  {a:?}\n file {b:?}", st.sql);
                checked += 1;
            }
            seed += 1;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
