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
    /// `COUNT(x)` counts NULLs as values.
    AggregatesCountNulls,
    /// `GROUP BY` puts every NULL in its own group.
    GroupByNullsSeparate,
    /// `LEFT JOIN` behaves like `INNER JOIN` (unmatched left rows vanish).
    LeftJoinIsInner,
    /// `IN` skips NULL in the list instead of yielding NULL.
    InNullIgnored,
    /// No constraint is enforced: NOT NULL/UNIQUE stay, no FK check, no
    /// INTEGER PRIMARY KEY auto-assignment.
    ConstraintIgnored,
}

impl Fault {
    pub const ALL: [Fault; 31] = [
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
        Fault::AggregatesCountNulls,
        Fault::GroupByNullsSeparate,
        Fault::LeftJoinIsInner,
        Fault::ConstraintIgnored,
        Fault::InNullIgnored,
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
                    '.' => ".",
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

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Lit(V),
    Col(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    Func(String, Vec<Expr>),
    /// An aggregate call: select-list items and `HAVING` only.
    Agg(Agg),
    /// A scalar subquery: `(SELECT ...)`, one column, one row.
    SubScalar(Box<Sel>),
    /// `EXISTS (SELECT ...)`.
    Exists(Box<Sel>),
    /// `lhs IN (SELECT ...)`.
    InSub {
        lhs: Box<Expr>,
        sel: Box<Sel>,
    },
    /// `lhs IN (literal, ...)`.
    InList {
        lhs: Box<Expr>,
        list: Vec<Expr>,
    },
}

/// One aggregate call. `COUNT` is the only one that takes `*`; the others
/// take exactly one argument (the dialect has no two-argument min/max).
#[derive(Debug, Clone, PartialEq)]
enum Agg {
    CountStar,
    Count(Box<Expr>),
    Sum(Box<Expr>),
    Min(Box<Expr>),
    Max(Box<Expr>),
}

/// A select-list item: a plain expression or an aggregate.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Expr(Expr),
    Agg(Agg),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Int,
    Text,
}

#[derive(Debug, Clone, PartialEq)]
enum Items {
    Star,
    List(Vec<(Item, Option<String>)>),
}

/// Column constraints (stage 9), parallel to a table's `(name, ty)` list.
#[derive(Debug, Clone, Default, PartialEq)]
struct Cons {
    pk: bool,
    unique: bool,
    not_null: bool,
    default: Option<V>,
    /// `REFERENCES parent(column)` with restrict semantics.
    references: Option<(String, String)>,
}

/// One `FROM` item: the table and the name its references may be
/// qualified by (the alias, or the table name when there is none).
#[derive(Debug, Clone, PartialEq)]
struct FromItem {
    table: String,
    qualifier: String,
    /// How this item joins the previous one (`Comma` for the first item).
    join: JoinKind,
    on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinKind {
    Comma,
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
struct Sel {
    distinct: bool,
    items: Items,
    /// Empty for a `SELECT` without `FROM`.
    from: Vec<FromItem>,
    wh: Option<Expr>,
    /// `GROUP BY` expressions (empty for a scalar aggregate).
    group: Vec<Expr>,
    having: Option<Expr>,
    order: Vec<(String, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug)]
enum Stmt {
    Create(String, Vec<(String, Ty)>, Vec<Cons>),
    /// `CREATE [UNIQUE] INDEX name ON table (col)`; the flag is `UNIQUE`.
    CreateIndex(String, String, String, bool),
    DropIndex(String),
    /// `ALTER TABLE t ADD [COLUMN] [IF NOT EXISTS] name TYPE [DEFAULT lit]`.
    AlterAdd {
        table: String,
        if_not_exists: bool,
        name: String,
        ty: Ty,
        default: Option<V>,
    },
    /// `ALTER TABLE t DROP [COLUMN] [IF EXISTS] name`.
    AlterDrop {
        table: String,
        if_exists: bool,
        name: String,
    },
    /// `ALTER TABLE t RENAME COLUMN old TO new`.
    AlterRenameColumn {
        table: String,
        old: String,
        new: String,
    },
    /// `ALTER TABLE t RENAME TO new`.
    AlterRenameTable {
        old: String,
        new: String,
    },
    /// `INSERT INTO t [(cols)] SELECT ...`.
    InsertSelect(String, Option<Vec<String>>, Box<Sel>),
    Insert(String, Option<Vec<String>>, Vec<Vec<Expr>>),
    Select(Sel),
    Update(String, Vec<(String, Expr)>, Option<Expr>),
    Delete(String, Option<Expr>),
    Begin,
    Commit,
    Rollback,
}

const RESERVED: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "FROM", "WHERE", "ORDER", "BY", "LIMIT",
    "OFFSET", "DISTINCT", "VALUES", "SET", "INTO", "AS", "AND", "OR", "NOT", "IS", "ASC", "DESC",
    "TABLE", "JOIN", "INNER", "LEFT", "OUTER", "ON", "GROUP", "HAVING", "NULL", "UNIQUE", "INDEX",
    // `EXISTS` is deliberately not reserved: the expression parser
    // recognises it as the subquery operator (ticket 10).
    "DROP", "ALTER", "RENAME", "TO", "IF", "COLUMN", "ADD",
];

// ----- parser ----------------------------------------------------------------

struct P {
    toks: Vec<Tok>,
    pos: usize,
    fault: Option<Fault>,
    /// True while parsing a context where an aggregate call may appear
    /// (select items, `HAVING`). Elsewhere the aggregate names are ordinary
    /// identifiers and an aggregate use is a prepare-time misuse.
    allow_agg: bool,
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
        } else if self.eat_kw("DROP") {
            self.expect_kw("INDEX")?;
            Stmt::DropIndex(self.ident()?)
        } else if self.eat_kw("ALTER") {
            self.alter()?
        } else if self.eat_kw("BEGIN") {
            Stmt::Begin
        } else if self.eat_kw("COMMIT") {
            Stmt::Commit
        } else if self.eat_kw("ROLLBACK") {
            Stmt::Rollback
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

    /// `ALTER TABLE` (ticket 13): add/drop/rename columns and tables, with
    /// the `IF [NOT] EXISTS` forms.
    fn alter(&mut self) -> R<Stmt> {
        self.expect_kw("TABLE")?;
        let table = self.ident()?;
        if self.eat_kw("ADD") {
            self.eat_kw("COLUMN");
            let if_not_exists = if self.eat_kw("IF") {
                self.expect_kw("NOT")?;
                self.expect_kw("EXISTS")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            let ty = if self.eat_kw("INTEGER") {
                Ty::Int
            } else if self.eat_kw("TEXT") {
                Ty::Text
            } else {
                return Err(ErrClass::Syntax);
            };
            let mut default = None;
            if self.eat_kw("DEFAULT") {
                default = Some(self.literal()?);
            }
            return Ok(Stmt::AlterAdd {
                table,
                if_not_exists,
                name,
                ty,
                default,
            });
        }
        if self.eat_kw("DROP") {
            self.eat_kw("COLUMN");
            let if_exists = if self.eat_kw("IF") {
                self.expect_kw("EXISTS")?;
                true
            } else {
                false
            };
            return Ok(Stmt::AlterDrop {
                table,
                if_exists,
                name: self.ident()?,
            });
        }
        if self.eat_kw("RENAME") {
            if self.eat_kw("COLUMN") {
                let old = self.ident()?;
                self.expect_kw("TO")?;
                let new = self.ident()?;
                return Ok(Stmt::AlterRenameColumn { table, old, new });
            }
            self.expect_kw("TO")?;
            return Ok(Stmt::AlterRenameTable {
                old: table,
                new: self.ident()?,
            });
        }
        Err(ErrClass::Syntax)
    }

    fn create(&mut self) -> R<Stmt> {
        let unique = self.eat_kw("UNIQUE");
        if self.eat_kw("INDEX") {
            let name = self.ident()?;
            self.expect_kw("ON")?;
            let table = self.ident()?;
            self.expect_sym("(")?;
            let col = self.ident()?;
            self.expect_sym(")")?;
            return Ok(Stmt::CreateIndex(name, table, col, unique));
        }
        if unique {
            return Err(ErrClass::Syntax);
        }
        self.expect_kw("TABLE")?;
        let name = self.ident()?;
        self.expect_sym("(")?;
        let mut cols = Vec::new();
        let mut cons = Vec::new();
        loop {
            let c = self.ident()?;
            let ty = if self.eat_kw("INTEGER") {
                Ty::Int
            } else if self.eat_kw("TEXT") {
                Ty::Text
            } else {
                return Err(ErrClass::Syntax);
            };
            // Column constraints, in any order.
            let mut cc = Cons::default();
            loop {
                if self.eat_kw("PRIMARY") {
                    self.expect_kw("KEY")?;
                    cc.pk = true;
                } else if self.eat_kw("UNIQUE") {
                    cc.unique = true;
                } else if self.eat_kw("NOT") {
                    self.expect_kw("NULL")?;
                    cc.not_null = true;
                } else if self.eat_kw("DEFAULT") {
                    cc.default = Some(self.literal()?);
                } else if self.eat_kw("REFERENCES") {
                    let pt = self.ident()?;
                    self.expect_sym("(")?;
                    let pc = self.ident()?;
                    self.expect_sym(")")?;
                    cc.references = Some((pt, pc));
                } else {
                    break;
                }
            }
            cols.push((c, ty));
            cons.push(cc);
            if self.eat_sym(",") {
                continue;
            }
            self.expect_sym(")")?;
            break;
        }
        Ok(Stmt::Create(name, cols, cons))
    }

    /// A literal value (a possibly negative integer, a string, or `NULL`),
    /// for `DEFAULT`.
    fn literal(&mut self) -> R<V> {
        let neg = self.eat_sym("-");
        match self.peek().cloned() {
            Some(Tok::Int(n)) => {
                self.pos += 1;
                if neg {
                    Ok(V::Int(n.checked_neg().ok_or(ErrClass::Syntax)?))
                } else {
                    Ok(V::Int(n))
                }
            }
            Some(Tok::Str(s)) if !neg => {
                self.pos += 1;
                Ok(V::Text(s))
            }
            Some(Tok::Id(w)) if !neg && w.eq_ignore_ascii_case("NULL") => {
                self.pos += 1;
                Ok(V::Null)
            }
            _ => Err(ErrClass::Syntax),
        }
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
        if self.eat_kw("SELECT") {
            let sel = self.select()?;
            return Ok(Stmt::InsertSelect(table, cols, Box::new(sel)));
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
                self.allow_agg = true;
                let parsed = self.item();
                self.allow_agg = false;
                let item = parsed?;
                let alias = if self.eat_kw("AS") {
                    Some(self.ident()?)
                } else {
                    None
                };
                v.push((item, alias));
                if !self.eat_sym(",") {
                    break;
                }
            }
            Items::List(v)
        };
        let mut sel = Sel {
            distinct,
            items,
            from: Vec::new(),
            wh: None,
            group: Vec::new(),
            having: None,
            order: Vec::new(),
            limit: None,
            offset: None,
        };
        if self.eat_kw("FROM") {
            // Items: `t [AS] a`, separated by `,` or a join keyword; a
            // join keyword's `ON` predicate follows the item it joins.
            let mut pending: Option<JoinKind> = None;
            loop {
                let table = self.ident()?;
                let has_alias = self.eat_kw("AS")
                    || matches!(self.peek(), Some(Tok::Id(w))
                        if !RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r)));
                let qualifier = if has_alias {
                    self.ident()?
                } else {
                    table.clone()
                };
                let join = pending.take().unwrap_or(JoinKind::Comma);
                let mut item = FromItem {
                    table,
                    qualifier,
                    join,
                    on: None,
                };
                if join != JoinKind::Comma {
                    self.expect_kw("ON")?;
                    item.on = Some(self.expr()?);
                }
                sel.from.push(item);
                if self.eat_sym(",") {
                    pending = Some(JoinKind::Comma);
                    continue;
                }
                let kind = if self.eat_kw("INNER") {
                    self.expect_kw("JOIN")?;
                    Some(JoinKind::Inner)
                } else if self.eat_kw("LEFT") {
                    self.eat_kw("OUTER");
                    self.expect_kw("JOIN")?;
                    Some(JoinKind::Left)
                } else if self.eat_kw("JOIN") {
                    Some(JoinKind::Inner)
                } else {
                    None
                };
                let Some(kind) = kind else { break };
                pending = Some(kind);
            }
            if self.eat_kw("WHERE") {
                sel.wh = Some(self.expr()?);
            }
            if self.eat_kw("GROUP") {
                self.expect_kw("BY")?;
                loop {
                    // An aggregate here is a misuse: SQLite rejects it, and
                    // `allow_agg` stays false so the parse fails.
                    sel.group.push(self.expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            if self.eat_kw("HAVING") {
                self.allow_agg = true;
                let h = self.expr();
                self.allow_agg = false;
                sel.having = Some(h?);
            }
            if self.eat_kw("ORDER") {
                self.expect_kw("BY")?;
                loop {
                    let name = {
                        let first = self.ident()?;
                        if self.eat_sym(".") {
                            format!("{first}.{}", self.ident()?)
                        } else {
                            first
                        }
                    };
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

    /// A select-list item: the expression parser already recognizes an
    /// aggregate call (when `allow_agg`) and returns `Expr::Agg`.
    fn item(&mut self) -> R<Item> {
        Ok(match self.expr()? {
            Expr::Agg(a) => Item::Agg(a),
            other => Item::Expr(other),
        })
    }

    /// `COUNT(*)`, `COUNT(x)`, `SUM(x)`, `MIN(x)`, `MAX(x)`. The dialect's
    /// shapes only: `*` is COUNT's, everything else takes one argument.
    fn aggregate(&mut self) -> R<Agg> {
        let name = self.ident()?.to_ascii_lowercase();
        self.expect_sym("(")?;
        if self.eat_sym("*") {
            self.expect_sym(")")?;
            return if name == "count" {
                Ok(Agg::CountStar)
            } else {
                Err(ErrClass::Arity)
            };
        }
        if self.eat_sym(")") {
            return Err(ErrClass::Arity);
        }
        self.allow_agg = false;
        let arg = self.expr();
        self.allow_agg = true;
        let arg = arg?;
        if !self.eat_sym(")") {
            // A second argument or a stray token.
            return Err(ErrClass::Arity);
        }
        Ok(match name.as_str() {
            "count" => Agg::Count(Box::new(arg)),
            "sum" => Agg::Sum(Box::new(arg)),
            "min" => Agg::Min(Box::new(arg)),
            _ => Agg::Max(Box::new(arg)),
        })
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
            // `[NOT] IN (subquery | list)` is a comparison-level operator.
            let neg_in = self.is_kw("NOT")
                && matches!(self.toks.get(self.pos + 1), Some(Tok::Id(w)) if w.eq_ignore_ascii_case("IN"));
            if neg_in || self.is_kw("IN") {
                self.pos += if neg_in { 2 } else { 1 };
                self.expect_sym("(")?;
                let item = if matches!(self.peek(), Some(Tok::Id(w)) if w.eq_ignore_ascii_case("SELECT"))
                {
                    self.pos += 1;
                    let sel = self.select()?;
                    Expr::InSub {
                        lhs: Box::new(l.clone()),
                        sel: Box::new(sel),
                    }
                } else {
                    let mut list = vec![self.expr()?];
                    while self.eat_sym(",") {
                        list.push(self.expr()?);
                    }
                    Expr::InList {
                        lhs: Box::new(l.clone()),
                        list,
                    }
                };
                self.expect_sym(")")?;
                l = if neg_in {
                    Expr::Not(Box::new(item))
                } else {
                    item
                };
                continue;
            }
            if self.eat_sym("=") {
                let r = self.rel_level()?;
                l = Expr::Bin(Op::Eq, Box::new(l), Box::new(r));
            } else if self.eat_sym("<>") || self.eat_sym("!=") {
                let r = self.rel_level()?;
                l = Expr::Bin(Op::Ne, Box::new(l), Box::new(r));
            } else if self.is_kw("IS") {
                self.pos += 1;
                let neg = self.eat_kw("NOT");
                if !matches!(self.peek(), Some(Tok::Id(s)) if s.eq_ignore_ascii_case("NULL")) {
                    return Err(ErrClass::Syntax);
                }
                self.pos += 1;
                l = Expr::IsNull(Box::new(l), neg);
            } else {
                return Ok(l);
            }
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
                // `(SELECT ...)` is a scalar subquery.
                if matches!(self.peek(), Some(Tok::Id(w)) if w.eq_ignore_ascii_case("SELECT")) {
                    self.pos += 1;
                    let sel = self.select()?;
                    self.expect_sym(")")?;
                    return Ok(Expr::SubScalar(Box::new(sel)));
                }
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
                if s.eq_ignore_ascii_case("EXISTS")
                    && matches!(self.toks.get(self.pos + 1), Some(Tok::Sym("(")))
                {
                    self.pos += 1;
                    self.expect_sym("(")?;
                    self.expect_kw("SELECT")?;
                    let sel = self.select()?;
                    self.expect_sym(")")?;
                    return Ok(Expr::Exists(Box::new(sel)));
                }
                let agg_name = matches!(
                    s.to_ascii_lowercase().as_str(),
                    "count" | "sum" | "min" | "max"
                );
                if agg_name
                    && self.allow_agg
                    && matches!(self.toks.get(self.pos + 1), Some(Tok::Sym("(")))
                {
                    return self.aggregate().map(Expr::Agg);
                }
                self.pos += 1;
                if self.eat_sym(".") {
                    // A qualified reference: `a.x` (alias or table name).
                    let col = self.ident()?;
                    return Ok(Expr::Col(format!("{}.{col}", s.to_ascii_lowercase())));
                }
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

/// Column-name resolution for one statement: the row layout's bare names
/// plus how each `FROM` item's columns may be qualified. A bare name that
/// matches several columns is `ERR ambiguous`; a qualifier that names no
/// item, or a name missing from the item, is `ERR no_column`.
#[derive(Debug, Default)]
struct Names {
    bare: Vec<String>,
    by_bare: std::collections::BTreeMap<String, Vec<usize>>,
    /// `(qualifier, first column index, column count)` in `FROM` order.
    items: Vec<(String, usize, usize)>,
}

impl Names {
    fn of(bare: Vec<String>, items: Vec<(String, usize, usize)>) -> Names {
        let mut by_bare: std::collections::BTreeMap<String, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, n) in bare.iter().enumerate() {
            by_bare.entry(n.clone()).or_default().push(i);
        }
        Names {
            bare,
            by_bare,
            items,
        }
    }

    /// A single-table statement: the table name qualifies its columns.
    fn single(table: &str, cols: &[(String, Ty)]) -> Names {
        let bare: Vec<String> = cols.iter().map(|(c, _)| c.clone()).collect();
        let n = bare.len();
        Names::of(bare, vec![(table.to_string(), 0, n)])
    }

    /// No columns (an `INSERT`'s values, a `SELECT` without `FROM`).
    fn empty() -> Names {
        Names::default()
    }

    fn resolve(&self, name: &str) -> R<usize> {
        if let Some((q, n)) = name.split_once('.') {
            let hits: Vec<&(String, usize, usize)> =
                self.items.iter().filter(|(a, _, _)| a == q).collect();
            if hits.len() != 1 {
                return Err(ErrClass::NoColumn);
            }
            let (_, start, len) = *hits[0];
            for i in start..start + len {
                if self.bare.get(i).map(|s| s.as_str()) == Some(n) {
                    return Ok(i);
                }
            }
            return Err(ErrClass::NoColumn);
        }
        match self.by_bare.get(name).map(|v| v.as_slice()) {
            Some([i]) => Ok(*i),
            Some(_) => Err(ErrClass::Ambiguous),
            None => Err(ErrClass::NoColumn),
        }
    }
}

struct Ctx<'a> {
    fault: Option<Fault>,
    names: &'a Names,
    /// `None` while validating statically (before any row exists).
    row: Option<&'a [V]>,
    /// The aggregates computed for the current group (empty outside an
    /// aggregate query).
    aggs: &'a [(Agg, V)],
    /// The enclosing query's scope, for correlated references.
    outer: Option<&'a Ctx<'a>>,
}

impl<'a> Ctx<'a> {
    /// A column reference: this scope first, then the enclosing ones (a
    /// correlated reference reads the *outer* row).
    fn col(&self, name: &str) -> R<V> {
        match self.names.resolve(name) {
            Ok(i) => match self.row {
                Some(r) => r.get(i).cloned().ok_or(ErrClass::NoColumn),
                None => Ok(V::Null),
            },
            Err(ErrClass::NoColumn) => match self.outer {
                Some(o) => o.col(name),
                None => Err(ErrClass::NoColumn),
            },
            Err(e) => Err(e),
        }
    }

    /// Resolve without a row (static validation).
    fn check(&self, name: &str) -> R<()> {
        match self.names.resolve(name) {
            Ok(_) => Ok(()),
            Err(ErrClass::NoColumn) => match self.outer {
                Some(o) => o.check(name),
                None => Err(ErrClass::NoColumn),
            },
            Err(e) => Err(e),
        }
    }
}

/// `x IN (values)`: true on a match; NULL when `x` is NULL or the list
/// holds NULL and nothing matched; false otherwise.
fn membership(v: V, vals: &[V], fault: Option<Fault>) -> V {
    // `x IN (empty set)` is FALSE, even for a NULL `x` (the SQL rule).
    if vals.is_empty() {
        return V::Int(0);
    }
    if matches!(v, V::Null) {
        return V::Null;
    }
    let mut saw_null = false;
    for x in vals {
        if matches!(x, V::Null) {
            saw_null = true;
            continue;
        }
        if cmp_vals(&v, x) == Ordering::Equal {
            return V::Int(1);
        }
    }
    if saw_null && fault != Some(Fault::InNullIgnored) {
        V::Null
    } else {
        V::Int(0)
    }
}

impl Toy {
    /// Evaluate an expression. A method (not a free function): subqueries
    /// run against the tables.
    fn eval(&self, e: &Expr, cx: &Ctx) -> R<V> {
        let two = cx.fault == Some(Fault::TwoValuedLogic);
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Col(name) => cx.col(name)?,
            Expr::Neg(x) => match self.eval(x, cx)? {
                V::Null => V::Null,
                V::Int(i) => V::Int(i.checked_neg().ok_or(ErrClass::Limit)?),
                V::Text(_) => return Err(ErrClass::Type),
            },
            Expr::Not(x) => match self.eval(x, cx)? {
                V::Null if !two => V::Null,
                v => V::Int(i64::from(!truthy(&v))),
            },
            Expr::IsNull(x, neg) => {
                let n = matches!(self.eval(x, cx)?, V::Null);
                V::Int(i64::from(n != *neg))
            }
            Expr::Func(name, args) => self.func(name, args, cx)?,
            Expr::Agg(a) => cx
                .aggs
                .iter()
                .find(|(k, _)| k == a)
                .map(|(_, v)| v.clone())
                .ok_or(ErrClass::Syntax)?,
            Expr::SubScalar(sel) => self.subquery_value(sel, cx)?,
            Expr::Exists(sel) => {
                let (_, rows) = self.run_sel(sel, Some(cx))?;
                V::Int(i64::from(!rows.is_empty()))
            }
            Expr::InSub { lhs, sel } => {
                let v = self.eval(lhs, cx)?;
                let (headers, rows) = self.run_sel(sel, Some(cx))?;
                if headers.len() != 1 {
                    return Err(ErrClass::Arity);
                }
                let vals: Vec<V> = rows
                    .into_iter()
                    .filter_map(|r| r.into_iter().next())
                    .collect();
                membership(v, &vals, cx.fault)
            }
            Expr::InList { lhs, list } => {
                let v = self.eval(lhs, cx)?;
                let vals: Vec<V> = list.iter().map(|e| self.eval(e, cx)).collect::<R<_>>()?;
                membership(v, &vals, cx.fault)
            }
            Expr::Bin(op, l, r) => {
                let (a, b) = (self.eval(l, cx)?, self.eval(r, cx)?);
                Self::bin(*op, a, b, cx.fault)?
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
                            return Self::zero(fault);
                        }
                        let q = x.checked_div(y).ok_or_else(lim)?;
                        let floor = fault == Some(Fault::FloorDivision)
                            && x % y != 0
                            && ((x < 0) != (y < 0));
                        V::Int(if floor { q - 1 } else { q })
                    }
                    _ => {
                        if y == 0 {
                            return Self::zero(fault);
                        }
                        let r = x.checked_rem(y).ok_or_else(lim)?;
                        let flip =
                            fault == Some(Fault::ModuloSign) && r != 0 && ((r < 0) != (y < 0));
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

    fn func(&self, name: &str, args: &[Expr], cx: &Ctx) -> R<V> {
        let want = match name {
            "length" | "upper" | "lower" | "abs" => 1,
            "coalesce" => 2,
            _ => return Err(ErrClass::NoColumn),
        };
        if args.len() < want || (name != "coalesce" && args.len() != want) {
            return Err(ErrClass::Arity);
        }
        let vals: Vec<V> = args.iter().map(|a| self.eval(a, cx)).collect::<R<_>>()?;
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
    fn check_cols(&self, e: &Expr, cx: &Ctx) -> R<()> {
        match e {
            Expr::Lit(_) => Ok(()),
            Expr::Col(n) => cx.check(n),
            Expr::Neg(x) | Expr::Not(x) | Expr::IsNull(x, _) => self.check_cols(x, cx),
            Expr::Bin(_, l, r) => {
                self.check_cols(l, cx)?;
                self.check_cols(r, cx)
            }
            Expr::Func(name, args) => {
                if matches!(name.as_str(), "count" | "sum" | "min" | "max") {
                    // A parse context that does not allow aggregates (WHERE,
                    // GROUP BY, an insert, ...): SQLite rejects the statement.
                    return Err(ErrClass::Syntax);
                }
                if !matches!(
                    name.as_str(),
                    "length" | "upper" | "lower" | "abs" | "coalesce"
                ) {
                    return Err(ErrClass::NoColumn);
                }
                args.iter().try_for_each(|a| self.check_cols(a, cx))
            }
            Expr::Agg(a) => self.check_agg(a, cx),
            Expr::SubScalar(sel) => {
                self.validate_sel(sel, cx)?;
                self.check_sub_arity(sel)
            }
            Expr::Exists(sel) => self.validate_sel(sel, cx),
            Expr::InSub { lhs, sel } => {
                self.check_cols(lhs, cx)?;
                self.validate_sel(sel, cx)?;
                self.check_sub_arity(sel)
            }
            Expr::InList { lhs, list } => {
                self.check_cols(lhs, cx)?;
                list.iter().try_for_each(|e| self.check_cols(e, cx))
            }
        }
    }

    /// Some aggregate's argument columns.
    fn check_agg(&self, a: &Agg, cx: &Ctx) -> R<()> {
        match a {
            Agg::CountStar => Ok(()),
            Agg::Count(x) | Agg::Sum(x) | Agg::Min(x) | Agg::Max(x) => self.check_cols(x, cx),
        }
    }

    /// Static validation of a (sub)query: tables exist, every column
    /// reference resolves in this query or an enclosing one, and the
    /// select-list arity rules hold for a scalar or `IN` subquery.
    fn validate_sel(&self, sel: &Sel, cx: &Ctx) -> R<()> {
        let names = self.names_of(&sel.from)?;
        let inner = Ctx {
            fault: cx.fault,
            names: &names,
            row: None,
            aggs: &[],
            outer: Some(cx),
        };
        for it in &sel.from {
            if let Some(on) = &it.on {
                self.check_cols(on, &inner)?;
            }
        }
        if let Some(w) = &sel.wh {
            self.check_cols(w, &inner)?;
        }
        for g in &sel.group {
            self.check_cols(g, &inner)?;
        }
        if let Some(h) = &sel.having {
            self.check_cols(h, &inner)?;
        }
        if let Items::List(items) = &sel.items {
            for (item, _) in items {
                match item {
                    Item::Expr(e) => self.check_cols(e, &inner)?,
                    Item::Agg(a) => self.check_agg(a, &inner)?,
                }
            }
        }
        Ok(())
    }

    /// A scalar or `IN` subquery must produce exactly one column: SQLite
    /// rejects more at prepare time, whatever the rows.
    fn check_sub_arity(&self, sel: &Sel) -> R<()> {
        let n = match &sel.items {
            Items::Star if sel.from.is_empty() => return Err(ErrClass::Syntax),
            Items::Star => self.names_of(&sel.from)?.bare.len(),
            Items::List(items) => items.len(),
        };
        if n != 1 {
            return Err(ErrClass::Arity);
        }
        Ok(())
    }

    /// A scalar subquery's value: one column (else `ERR arity`), first row,
    /// NULL when empty.
    fn subquery_value(&self, sel: &Sel, cx: &Ctx) -> R<V> {
        let (headers, rows) = self.run_sel(sel, Some(cx))?;
        if headers.len() != 1 {
            return Err(ErrClass::Arity);
        }
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .unwrap_or(V::Null))
    }

    /// One aggregate over a group's rows. NULLs are skipped (`COUNT` counts
    /// non-NULLs, `SUM`/`MIN`/`MAX` ignore them) and `SUM` of nothing is NULL.
    fn eval_agg(&self, a: &Agg, cx: &Ctx, rows: &[&Vec<V>]) -> R<V> {
        let values = |expr: &Expr| -> R<Vec<V>> {
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let row_cx = Ctx {
                    fault: cx.fault,
                    names: cx.names,
                    row: Some(r),
                    aggs: &[],
                    outer: cx.outer,
                };
                out.push(self.eval(expr, &row_cx)?);
            }
            Ok(out)
        };
        match a {
            Agg::CountStar => Ok(V::Int(rows.len() as i64)),
            Agg::Count(x) => {
                let count_nulls = cx.fault == Some(Fault::AggregatesCountNulls);
                let n = values(x)?
                    .iter()
                    .filter(|v| count_nulls || !matches!(v, V::Null))
                    .count();
                Ok(V::Int(n as i64))
            }
            Agg::Sum(x) => {
                let vs = values(x)?;
                let mut acc: i64 = 0;
                let mut seen = false;
                for v in &vs {
                    match v {
                        V::Null => {}
                        V::Int(i) => {
                            seen = true;
                            acc = acc.checked_add(*i).ok_or(ErrClass::Limit)?;
                        }
                        V::Text(_) => return Err(ErrClass::Type),
                    }
                }
                Ok(if seen { V::Int(acc) } else { V::Null })
            }
            Agg::Min(x) | Agg::Max(x) => {
                let want_max = matches!(a, Agg::Max(_));
                let mut best: Option<V> = None;
                for v in values(x)? {
                    if matches!(v, V::Null) {
                        continue;
                    }
                    best = Some(match best {
                        None => v,
                        Some(b) => {
                            let o = cmp_vals(&v, &b);
                            if (want_max && o == Ordering::Greater)
                                || (!want_max && o == Ordering::Less)
                            {
                                v
                            } else {
                                b
                            }
                        }
                    });
                }
                Ok(best.unwrap_or(V::Null))
            }
        }
    }
}

// ----- engine ----------------------------------------------------------------------

fn collect_aggs(e: &Expr, out: &mut Vec<Agg>) {
    match e {
        Expr::Agg(a) => {
            if !out.contains(a) {
                out.push(a.clone());
            }
            match a {
                Agg::CountStar => {}
                Agg::Count(x) | Agg::Sum(x) | Agg::Min(x) | Agg::Max(x) => collect_aggs(x, out),
            }
        }
        Expr::Neg(x) | Expr::Not(x) | Expr::IsNull(x, _) => collect_aggs(x, out),
        Expr::Bin(_, l, r) => {
            collect_aggs(l, out);
            collect_aggs(r, out);
        }
        Expr::Func(_, args) => args.iter().for_each(|a| collect_aggs(a, out)),
        Expr::InList { lhs, list } => {
            collect_aggs(lhs, out);
            for e in list {
                collect_aggs(e, out);
            }
        }
        Expr::InSub { lhs, .. } => collect_aggs(lhs, out),
        Expr::Lit(_) | Expr::Col(_) | Expr::SubScalar(_) | Expr::Exists(_) => {}
    }
}

fn contains_agg(e: &Expr) -> bool {
    let mut v = Vec::new();
    collect_aggs(e, &mut v);
    !v.is_empty()
}

/// The index registry: name -> (table, column, unique).
type Indexes = BTreeMap<String, (String, String, bool)>;

#[derive(Clone)]
struct Tbl {
    cols: Vec<(String, Ty)>,
    cons: Vec<Cons>,
    rows: Vec<Vec<V>>,
}

pub struct Toy {
    tables: BTreeMap<String, Tbl>,
    /// Live indexes (ticket 11). An index changes no answer; only a
    /// UNIQUE one constrains.
    indexes: Indexes,
    /// Snapshot taken at `BEGIN`; `Some` means a transaction is open.
    txn: Option<(BTreeMap<String, Tbl>, Indexes)>,
    fault: Option<Fault>,
}

impl Toy {
    pub fn new(fault: Option<Fault>) -> Self {
        Toy {
            tables: BTreeMap::new(),
            indexes: BTreeMap::new(),
            txn: None,
            fault,
        }
    }

    /// The row a column-omitting `INSERT` starts from: each column's
    /// `DEFAULT` literal, or NULL.
    fn insert_defaults(&self, name: &str) -> R<Vec<V>> {
        let tbl = self.tables.get(name).ok_or(ErrClass::NoTable)?;
        Ok(tbl
            .cons
            .iter()
            .map(|c| c.default.clone().unwrap_or(V::Null))
            .collect())
    }

    /// `CREATE [UNIQUE] INDEX name ON table (col)` (ticket 11). An index is
    /// a named schema object: a duplicate name is `ERR exists`, an unknown
    /// table or column is `no_table`/`no_column`, and a UNIQUE index over
    /// already-duplicated values fails at creation like the column form.
    fn create_index(&mut self, name: &str, table: &str, col: &str, unique: bool) -> R<Response> {
        if self.indexes.contains_key(name) {
            return Err(ErrClass::Exists);
        }
        let tbl = self.tables.get(table).ok_or(ErrClass::NoTable)?;
        let ti = tbl
            .cols
            .iter()
            .position(|(n, _)| n == col)
            .ok_or(ErrClass::NoColumn)?;
        if unique && self.fault != Some(Fault::ConstraintIgnored) {
            let mut seen: Vec<&V> = Vec::new();
            for r in &tbl.rows {
                if let Some(v) = r.get(ti) {
                    if matches!(v, V::Null) {
                        continue;
                    }
                    if seen.iter().any(|s| cmp_vals(s, v) == Ordering::Equal) {
                        return Err(ErrClass::Constraint);
                    }
                    seen.push(v);
                }
            }
        }
        self.indexes.insert(
            name.to_string(),
            (table.to_string(), col.to_string(), unique),
        );
        Ok(Response::Ok(0))
    }

    /// Constraints for one `INSERT` row (stage 9): `INTEGER PRIMARY KEY`
    /// assigns max+1 (1 when empty) on NULL, a TEXT primary key rejects
    /// NULL, `NOT NULL`/`UNIQUE` reject violations (several NULLs are
    /// fine), and `REFERENCES` requires the parent row to exist.
    /// `built` holds the statement's earlier rows, which are visible to
    /// UNIQUE and to the auto-assignment.
    fn insert_row_checks(&self, name: &str, out: &mut [V], built: &[Vec<V>]) -> R<()> {
        let tbl = self.tables.get(name).ok_or(ErrClass::NoTable)?;
        let ignore = self.fault == Some(Fault::ConstraintIgnored);
        for (ti, (_, ty)) in tbl.cols.iter().enumerate() {
            let c = &tbl.cons[ti];
            if ignore {
                continue;
            }
            if c.pk && *ty == Ty::Int && matches!(out[ti], V::Null) {
                let maxv = tbl
                    .rows
                    .iter()
                    .chain(built.iter())
                    .filter_map(|r| match r.get(ti) {
                        Some(V::Int(i)) => Some(*i),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                if maxv == i64::MAX {
                    // SQLite would pick a *random* rowid here (its
                    // allocation overflows): outside the dialect, so a
                    // generator regression fails loudly instead of as a
                    // constraint mismatch.
                    return Err(ErrClass::Limit);
                }
                // SQLite assigns max+1 even when the max is negative
                // (`unwrap_or(0)` above gives 1 on an empty table).
                out[ti] = V::Int(maxv.saturating_add(1));
            }
            if matches!(out[ti], V::Null) {
                if c.not_null || (c.pk && *ty == Ty::Text) {
                    return Err(ErrClass::Constraint);
                }
                continue;
            }
            if (c.unique || c.pk)
                && tbl.rows.iter().chain(built.iter()).any(|r| {
                    r.get(ti)
                        .map(|v| cmp_vals(v, &out[ti]) == Ordering::Equal)
                        .unwrap_or(false)
                })
            {
                return Err(ErrClass::Constraint);
            }
        }
        if !ignore {
            for (table, col, unique) in self.indexes.values() {
                if !*unique || table != name {
                    continue;
                }
                let Some(ti) = tbl.cols.iter().position(|(n, _)| n == col) else {
                    continue;
                };
                let Some(v) = out.get(ti) else { continue };
                if matches!(v, V::Null) {
                    continue;
                }
                if tbl.rows.iter().chain(built.iter()).any(|r| {
                    r.get(ti)
                        .map(|rv| cmp_vals(rv, v) == Ordering::Equal)
                        .unwrap_or(false)
                }) {
                    return Err(ErrClass::Constraint);
                }
            }
        }
        Ok(())
    }

    /// Foreign keys are *immediate* constraints, but SQLite checks them at
    /// the end of the statement: a later row's type or UNIQUE error wins
    /// over an earlier row's missing parent. One pass over the statement's
    /// new rows.
    fn fk_checks(&self, name: &str, rows: &[&Vec<V>]) -> R<()> {
        let tbl = self.tables.get(name).ok_or(ErrClass::NoTable)?;
        if self.fault == Some(Fault::ConstraintIgnored) {
            return Ok(());
        }
        for (ti, c) in tbl.cons.iter().enumerate() {
            let Some((pt, pc)) = &c.references else {
                continue;
            };
            let parent = self.tables.get(pt).ok_or(ErrClass::NoColumn)?;
            let pi = parent
                .cols
                .iter()
                .position(|(n, _)| n == pc)
                .ok_or(ErrClass::NoColumn)?;
            for r in rows {
                let Some(v) = r.get(ti) else { continue };
                if matches!(v, V::Null) {
                    continue;
                }
                let found = parent.rows.iter().any(|pr| {
                    pr.get(pi)
                        .map(|pv| cmp_vals(pv, v) == Ordering::Equal)
                        .unwrap_or(false)
                });
                if !found {
                    return Err(ErrClass::Constraint);
                }
            }
        }
        Ok(())
    }

    /// Constraints for one `UPDATE` result row: NULL into `NOT NULL`/TEXT
    /// key and a duplicate `UNIQUE`/primary key (the row itself excluded).
    /// Foreign keys are checked at the statement end (`fk_checks`). `rows`
    /// is the table's state with the rows updated so far.
    fn update_row_checks(&self, name: &str, out: &[V], row_at: usize, rows: &[Vec<V>]) -> R<()> {
        let tbl = self.tables.get(name).ok_or(ErrClass::NoTable)?;
        if self.fault == Some(Fault::ConstraintIgnored) {
            return Ok(());
        }
        for (ti, (_, ty)) in tbl.cols.iter().enumerate() {
            let c = &tbl.cons[ti];
            if matches!(out[ti], V::Null) {
                if c.not_null || (c.pk && *ty == Ty::Text) {
                    return Err(ErrClass::Constraint);
                }
                continue;
            }
            if (c.unique || c.pk)
                && rows.iter().enumerate().any(|(ri, r)| {
                    ri != row_at
                        && r.get(ti)
                            .map(|v| cmp_vals(v, &out[ti]) == Ordering::Equal)
                            .unwrap_or(false)
                })
            {
                return Err(ErrClass::Constraint);
            }
        }
        for (table, col, unique) in self.indexes.values() {
            if !*unique || table != name {
                continue;
            }
            let Some(ti) = tbl.cols.iter().position(|(n, _)| n == col) else {
                continue;
            };
            let Some(v) = out.get(ti) else { continue };
            if matches!(v, V::Null) {
                continue;
            }
            if rows.iter().enumerate().any(|(ri, r)| {
                ri != row_at
                    && r.get(ti)
                        .map(|rv| cmp_vals(rv, v) == Ordering::Equal)
                        .unwrap_or(false)
            }) {
                return Err(ErrClass::Constraint);
            }
        }
        Ok(())
    }

    /// A `DELETE` that removes a row another table's `REFERENCES` points at
    /// is `ERR constraint` (restrict), and nothing is deleted.
    fn delete_fk_restrict(
        &self,
        name: &str,
        deleted: &[Vec<V>],
        my_cols: &[(String, Ty)],
    ) -> R<()> {
        if self.fault == Some(Fault::ConstraintIgnored) || deleted.is_empty() {
            return Ok(());
        }
        for (child_name, child) in &self.tables {
            if child_name == name {
                continue;
            }
            for (ti, c) in child.cons.iter().enumerate() {
                let Some((pt, pc)) = &c.references else {
                    continue;
                };
                if pt != name {
                    continue;
                }
                let Some(pi) = my_cols.iter().position(|(n, _)| n == pc) else {
                    continue;
                };
                let gone: Vec<&V> = deleted
                    .iter()
                    .filter_map(|r| r.get(pi))
                    .filter(|v| !matches!(v, V::Null))
                    .collect();
                for crow in &child.rows {
                    if let Some(cv) = crow.get(ti) {
                        if !matches!(cv, V::Null)
                            && gone.iter().any(|g| cmp_vals(g, cv) == Ordering::Equal)
                        {
                            return Err(ErrClass::Constraint);
                        }
                    }
                }
            }
        }
        Ok(())
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
            allow_agg: false,
        };
        match p.statement()? {
            Stmt::Create(name, cols, cons) => {
                if self.tables.contains_key(&name) {
                    return Err(ErrClass::Exists);
                }
                self.tables.insert(
                    name,
                    Tbl {
                        cols,
                        cons,
                        rows: Vec::new(),
                    },
                );
                Ok(Response::Ok(0))
            }
            Stmt::CreateIndex(name, table, col, unique) => {
                self.create_index(&name, &table, &col, unique)
            }
            Stmt::AlterAdd {
                table,
                if_not_exists,
                name,
                ty,
                default,
            } => {
                let Some(tbl) = self.tables.get_mut(&table) else {
                    return Err(ErrClass::NoTable);
                };
                if tbl.cols.iter().any(|(c, _)| c == &name) {
                    if if_not_exists {
                        return Ok(Response::Ok(0));
                    }
                    return Err(ErrClass::Exists);
                }
                tbl.cols.push((name, ty));
                tbl.cons.push(Cons {
                    default: default.clone(),
                    ..Cons::default()
                });
                let fill = default.unwrap_or(V::Null);
                for row in tbl.rows.iter_mut() {
                    row.push(fill.clone());
                }
                Ok(Response::Ok(0))
            }
            Stmt::AlterDrop {
                table,
                if_exists,
                name,
            } => {
                let Some(tbl) = self.tables.get_mut(&table) else {
                    return Err(ErrClass::NoTable);
                };
                let Some(idx) = tbl.cols.iter().position(|(c, _)| c == &name) else {
                    if if_exists {
                        return Ok(Response::Ok(0));
                    }
                    return Err(ErrClass::NoColumn);
                };
                // SQLite refuses PK/UNIQUE/indexed columns and the last
                // column of a table.
                let cons = &tbl.cons[idx];
                if cons.pk || cons.unique {
                    return Err(ErrClass::Constraint);
                }
                if self
                    .indexes
                    .values()
                    .any(|(t, c, _)| t == &table && c == &name)
                {
                    return Err(ErrClass::Constraint);
                }
                if tbl.cols.len() <= 1 {
                    return Err(ErrClass::Constraint);
                }
                tbl.cols.remove(idx);
                tbl.cons.remove(idx);
                for row in tbl.rows.iter_mut() {
                    row.remove(idx);
                }
                Ok(Response::Ok(0))
            }
            Stmt::AlterRenameColumn { table, old, new } => {
                let Some(tbl) = self.tables.get_mut(&table) else {
                    return Err(ErrClass::NoTable);
                };
                let Some(idx) = tbl.cols.iter().position(|(c, _)| c == &old) else {
                    return Err(ErrClass::NoColumn);
                };
                if tbl.cols.iter().any(|(c, _)| c == &new) {
                    return Err(ErrClass::Exists);
                }
                tbl.cols[idx].0 = new.clone();
                // SQLite follows the rename in indexes and foreign keys.
                for (_, (t, c, _)) in self.indexes.iter_mut() {
                    if t == &table && c == &old {
                        *c = new.clone();
                    }
                }
                for other in self.tables.values_mut() {
                    for cons in other.cons.iter_mut() {
                        if cons.references == Some((table.clone(), old.clone())) {
                            cons.references = Some((table.clone(), new.clone()));
                        }
                    }
                }
                Ok(Response::Ok(0))
            }
            Stmt::AlterRenameTable { old, new } => {
                if self.tables.contains_key(&new) || self.indexes.contains_key(&new) {
                    return Err(ErrClass::Exists);
                }
                let Some(tbl) = self.tables.remove(&old) else {
                    return Err(ErrClass::NoTable);
                };
                self.tables.insert(new.clone(), tbl);
                for (_, (t, _, _)) in self.indexes.iter_mut() {
                    if t == &old {
                        *t = new.clone();
                    }
                }
                for other in self.tables.values_mut() {
                    for cons in other.cons.iter_mut() {
                        if let Some((pt, _)) = &mut cons.references {
                            if pt == &old {
                                *pt = new.clone();
                            }
                        }
                    }
                }
                Ok(Response::Ok(0))
            }
            Stmt::DropIndex(name) => {
                if self.indexes.remove(&name).is_none() {
                    return Err(ErrClass::NoTable);
                }
                Ok(Response::Ok(0))
            }
            Stmt::Insert(name, cols, rows) => self.insert(&name, cols, rows),
            Stmt::Select(sel) => self.select(sel),
            Stmt::InsertSelect(name, cols, sel) => self.insert_select_exec(&name, cols, &sel),
            Stmt::Update(name, sets, wh) => self.update(&name, sets, wh),
            Stmt::Delete(name, wh) => self.delete(&name, wh),
            Stmt::Begin => {
                if self.txn.is_some() {
                    return Err(ErrClass::Txn);
                }
                self.txn = Some((self.tables.clone(), self.indexes.clone()));
                Ok(Response::Ok(0))
            }
            Stmt::Commit => {
                if self.txn.take().is_none() {
                    return Err(ErrClass::Txn);
                }
                Ok(Response::Ok(0))
            }
            Stmt::Rollback => {
                let Some((snapshot, indexes)) = self.txn.take() else {
                    return Err(ErrClass::Txn);
                };
                self.tables = snapshot;
                self.indexes = indexes;
                Ok(Response::Ok(0))
            }
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
        // Values may not reference columns; omitted columns start at their
        // DEFAULT literal (or NULL).
        let none = Names::empty();
        let ncx = Ctx {
            fault: self.fault,
            names: &none,
            row: None,
            aggs: &[],
            outer: None,
        };
        let defaults = self.insert_defaults(name)?;
        for row in &rows {
            let step = (|| -> R<Vec<V>> {
                if row.len() != targets.len() {
                    return Err(ErrClass::Arity);
                }
                let mut out = defaults.clone();
                for (e, &ti) in row.iter().zip(&targets) {
                    self.check_cols(e, &ncx)?;
                    let v = self.eval(e, &ncx)?;
                    out[ti] = self.coerce(v, schema[ti].1)?;
                }
                self.insert_row_checks(name, &mut out, &built)?;
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
        self.apply_insert(name, built, failure)
    }

    /// The shared tail of `INSERT` and `INSERT ... SELECT`: the
    /// statement-end FK checks, the dialect's atomicity rule, and the
    /// apply.
    fn apply_insert(
        &mut self,
        name: &str,
        built: Vec<Vec<V>>,
        mut failure: Option<ErrClass>,
    ) -> R<Response> {
        if failure.is_none() {
            if let Err(c) = self.fk_checks(name, &built.iter().collect::<Vec<_>>()) {
                failure = Some(c);
            }
        }
        let tbl = self.tables.get_mut(name).ok_or(ErrClass::NoTable)?;
        match failure {
            Some(c) => {
                // The dialect's statement atomicity: a failing statement
                // changes nothing, in autocommit and inside a transaction
                // alike (the oracle enforces it with a statement savepoint,
                // because plain SQLite 3.46 has a statement-journal quirk
                // inside transactions). The `InsertNotAtomic` mutant keeps
                // the earlier rows instead.
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

    /// `INSERT INTO t [(cols)] SELECT ...`: the select's rows go through
    /// the same per-row constraints as `VALUES`.
    fn insert_select_exec(
        &mut self,
        name: &str,
        cols: Option<Vec<String>>,
        sel: &Sel,
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
        // SQLite checks the SELECT's declared width at prepare time, so a
        // zero-row SELECT still fails with `arity` when the widths differ.
        // The row loop below only sees actual rows; check the headers first.
        let (headers, rows) = self.run_sel(sel, None)?;
        if headers.len() != targets.len() {
            return Err(ErrClass::Arity);
        }
        let defaults = self.insert_defaults(name)?;
        let mut built: Vec<Vec<V>> = Vec::new();
        let mut failure: Option<ErrClass> = None;
        for row in rows {
            let step = (|| -> R<Vec<V>> {
                if row.len() != targets.len() {
                    return Err(ErrClass::Arity);
                }
                let mut out = defaults.clone();
                for (v, &ti) in row.into_iter().zip(&targets) {
                    out[ti] = self.coerce(v, schema[ti].1)?;
                }
                self.insert_row_checks(name, &mut out, &built)?;
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
        self.apply_insert(name, built, failure)
    }

    fn keep(&self, wh: &Option<Expr>, cx: &Ctx) -> R<bool> {
        match wh {
            None => Ok(true),
            Some(e) => {
                let v = self.eval(e, cx)?;
                Ok(truthy(&v) || (self.fault == Some(Fault::WhereNullIsTrue) && v == V::Null))
            }
        }
    }

    /// Build the joined rows for `items`: one nested loop per item, `ON`
    /// evaluated on the row combined so far (a reference to a later item is
    /// `no_column`, which SQLite rejects too), and a NULL-padded row when a
    /// `LEFT JOIN` finds no match.
    /// The `Names` of a `FROM` list: table lookups and column ranges, no
    /// rows (also used for static subquery validation).
    fn names_of(&self, items: &[FromItem]) -> R<Names> {
        let mut bare: Vec<String> = Vec::new();
        let mut parts: Vec<(String, usize, usize)> = Vec::new();
        for it in items {
            let tbl = self.tables.get(&it.table).ok_or(ErrClass::NoTable)?;
            parts.push((it.qualifier.clone(), bare.len(), tbl.cols.len()));
            bare.extend(tbl.cols.iter().map(|(c, _)| c.clone()));
        }
        Ok(Names::of(bare, parts))
    }

    fn join_rows(&self, items: &[FromItem]) -> R<(Names, Vec<Vec<V>>)> {
        let names = self.names_of(items)?;
        let parts: Vec<(String, usize, usize)> = names.items.clone();
        let mut tables: Vec<&Tbl> = Vec::new();
        for it in items {
            let tbl = self.tables.get(&it.table).ok_or(ErrClass::NoTable)?;
            tables.push(tbl);
        }
        // `ON` predicates are validated up front (SQLite rejects a missing
        // column at prepare time, even when no pair would be tested).
        let vcx = Ctx {
            fault: self.fault,
            names: &names,
            row: None,
            aggs: &[],
            outer: None,
        };
        for it in items {
            if let Some(on) = &it.on {
                self.check_cols(on, &vcx)?;
            }
        }
        let mut rows: Vec<Vec<V>> = vec![Vec::new()];
        for (i, it) in items.iter().enumerate() {
            let (_, _, len) = parts[i];
            let tbl = tables[i];
            let mut next: Vec<Vec<V>> = Vec::new();
            for left in &rows {
                let mut matched = false;
                for r in &tbl.rows {
                    let mut combined = left.clone();
                    combined.extend(r.iter().cloned());
                    if let Some(on) = &it.on {
                        let cx = Ctx {
                            fault: self.fault,
                            names: &names,
                            row: Some(&combined),
                            aggs: &[],
                            outer: None,
                        };
                        if !truthy(&self.eval(on, &cx)?) {
                            continue;
                        }
                    }
                    matched = true;
                    next.push(combined);
                }
                if it.join == JoinKind::Left
                    && !matched
                    && self.fault != Some(Fault::LeftJoinIsInner)
                {
                    let mut combined = left.clone();
                    combined.extend(std::iter::repeat_n(V::Null, len));
                    next.push(combined);
                }
            }
            rows = next;
        }
        Ok((names, rows))
    }

    /// Run a query and return its headers and rows (the shared path for
    /// statements and subqueries; `outer` is the enclosing scope for
    /// correlated references). Static validation runs first, so a bad
    /// column errors even when the query would never evaluate it.
    fn run_sel(&self, sel: &Sel, outer: Option<&Ctx>) -> R<(Vec<String>, Vec<Vec<V>>)> {
        let (names, rows): (Names, Vec<Vec<V>>) = if sel.from.is_empty() {
            (Names::empty(), vec![Vec::new()])
        } else {
            self.join_rows(&sel.from)?
        };
        let vcx = Ctx {
            fault: self.fault,
            names: &names,
            row: None,
            aggs: &[],
            outer,
        };
        for it in &sel.from {
            if let Some(on) = &it.on {
                self.check_cols(on, &vcx)?;
            }
        }
        if let Some(w) = &sel.wh {
            self.check_cols(w, &vcx)?;
        }
        for g in &sel.group {
            self.check_cols(g, &vcx)?;
        }
        if let Some(h) = &sel.having {
            self.check_cols(h, &vcx)?;
        }
        // Output columns: (header, item).
        let mut outputs: Vec<(String, Item)> = Vec::new();
        match &sel.items {
            Items::Star => {
                if sel.from.is_empty() {
                    return Err(ErrClass::Syntax);
                }
                // The header is the bare column name (SQLite's `SELECT *`),
                // but each output resolves through its own item's
                // qualifier: a self-join duplicates the bare names, and a
                // bare reference to them is ambiguous.
                let mut v: Vec<(String, Item)> = Vec::new();
                for (q, start, len) in &names.items {
                    for i in *start..*start + *len {
                        let bare = names.bare[i].clone();
                        let item = Item::Expr(Expr::Col(format!("{q}.{bare}")));
                        v.push((bare, item));
                    }
                }
                if self.fault == Some(Fault::StarReversed) {
                    v.reverse();
                }
                outputs = v;
            }
            Items::List(items) => {
                for (item, alias) in items {
                    let header = match (alias, item) {
                        (Some(a), _) => a.clone(),
                        (None, Item::Expr(Expr::Col(c))) => c.clone(),
                        _ => "?".to_string(),
                    };
                    match item {
                        Item::Expr(e) => self.check_cols(e, &vcx)?,
                        Item::Agg(a) => self.check_agg(a, &vcx)?,
                    }
                    outputs.push((header, item.clone()));
                }
            }
        }
        let mut headers: Vec<String> = outputs.iter().map(|(h, _)| h.clone()).collect();
        if self.fault == Some(Fault::HeadersUppercased) {
            headers = headers.iter().map(|h| h.to_ascii_uppercase()).collect();
        }
        // Resolve ORDER BY against the output names, then against the
        // source columns (legal when the query selects them, as `SELECT *`
        // does — the generator's join queries order by qualified refs).
        let mut keys: Vec<(usize, bool)> = Vec::new();
        for (name, desc) in &sel.order {
            let i = if let Some(i) = outputs.iter().position(|(h, _)| h == name) {
                i
            } else {
                let idx = names.resolve(name)?;
                let bare = name.rsplit('.').next().unwrap_or(name);
                // `SELECT *` lists the source columns in order, so the
                // source index is the output index.
                match outputs.get(idx).map(|(_, item)| item) {
                    Some(Item::Expr(Expr::Col(c))) if c.rsplit('.').next().unwrap_or(c) == bare => {
                        idx
                    }
                    _ => return Err(ErrClass::NoColumn),
                }
            };
            keys.push((i, *desc));
        }
        let mut out_rows: Vec<Vec<V>> = Vec::new();
        // `LIMIT 0` returns nothing and evaluates no row (SQLite does the
        // same), so an expression that would error is not reached.
        if sel.limit == Some(0) && self.fault != Some(Fault::LimitPlusOne) {
            return Ok((headers, Vec::new()));
        }
        let aggregates = outputs.iter().any(|(_, i)| match i {
            Item::Agg(_) => true,
            Item::Expr(e) => contains_agg(e),
        }) || sel.having.as_ref().map(contains_agg).unwrap_or(false);
        if aggregates {
            // Group the filtered rows by the GROUP BY expressions; with no
            // GROUP BY the whole (possibly empty) result is one group, so a
            // scalar aggregate always yields exactly one row.
            let mut groups: Vec<(Vec<V>, Vec<&Vec<V>>)> = Vec::new();
            for row in &rows {
                let cx = Ctx {
                    fault: self.fault,
                    names: &names,
                    row: Some(row),
                    aggs: &[],
                    outer,
                };
                if !self.keep(&sel.wh, &cx)? {
                    continue;
                }
                let key: Vec<V> = sel
                    .group
                    .iter()
                    .map(|e| self.eval(e, &cx))
                    .collect::<R<_>>()?;
                let separate = self.fault == Some(Fault::GroupByNullsSeparate);
                let found = groups.iter_mut().find(|(k, _)| {
                    k.len() == key.len()
                        && k.iter().zip(&key).all(|(a, b)| {
                            if separate && (matches!(a, V::Null) || matches!(b, V::Null)) {
                                false
                            } else {
                                cmp_vals(a, b) == Ordering::Equal
                            }
                        })
                });
                match found {
                    Some((_, rs)) => rs.push(row),
                    None => groups.push((key, vec![row])),
                }
            }
            if sel.group.is_empty() && groups.is_empty() {
                groups.push((Vec::new(), Vec::new()));
            }
            let mut all_aggs: Vec<Agg> = Vec::new();
            for (_, item) in &outputs {
                match item {
                    Item::Agg(a) if !all_aggs.contains(a) => all_aggs.push(a.clone()),
                    Item::Agg(_) => {}
                    Item::Expr(e) => collect_aggs(e, &mut all_aggs),
                }
            }
            if let Some(h) = &sel.having {
                collect_aggs(h, &mut all_aggs);
            }
            let null_row: Vec<V> = names.bare.iter().map(|_| V::Null).collect();
            for (key, g_rows) in &groups {
                let mut aggs: Vec<(Agg, V)> = Vec::with_capacity(all_aggs.len());
                for a in &all_aggs {
                    let acx = Ctx {
                        fault: self.fault,
                        names: &names,
                        row: None,
                        aggs: &[],
                        outer,
                    };
                    aggs.push((a.clone(), self.eval_agg(a, &acx, g_rows)?));
                }
                let basis: &[V] = g_rows.first().map(|r| r.as_slice()).unwrap_or(&null_row);
                if let Some(h) = &sel.having {
                    let v = self.eval(
                        h,
                        &Ctx {
                            fault: self.fault,
                            names: &names,
                            row: Some(basis),
                            aggs: &aggs,
                            outer,
                        },
                    )?;
                    if !truthy(&v) {
                        continue;
                    }
                }
                let _ = key;
                let mut o = Vec::with_capacity(outputs.len());
                for (_, item) in &outputs {
                    let cx = Ctx {
                        fault: self.fault,
                        names: &names,
                        row: Some(basis),
                        aggs: &aggs,
                        outer,
                    };
                    match item {
                        Item::Agg(a) => o.push(
                            aggs.iter()
                                .find(|(k, _)| k == a)
                                .map(|(_, v)| v.clone())
                                .unwrap_or(V::Null),
                        ),
                        // An expression item in an aggregate query may
                        // itself contain aggregates (`COUNT(*) + 1`).
                        Item::Expr(e) => o.push(self.eval(e, &cx)?),
                    }
                }
                out_rows.push(o);
            }
        } else {
            for row in &rows {
                let cx = Ctx {
                    fault: self.fault,
                    names: &names,
                    row: Some(row),
                    aggs: &[],
                    outer,
                };
                if !self.keep(&sel.wh, &cx)? {
                    continue;
                }
                let mut o = Vec::with_capacity(outputs.len());
                for (_, item) in &outputs {
                    match item {
                        Item::Expr(e) => o.push(self.eval(e, &cx)?),
                        Item::Agg(_) => unreachable!("aggregate mode caught above"),
                    }
                }
                out_rows.push(o);
            }
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
        Ok((headers, rows_final))
    }

    /// The statement path: run the query, then render it on the wire.
    fn select(&self, sel: Sel) -> R<Response> {
        let (headers, rows_final) = self.run_sel(&sel, None)?;
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
        let names = Names::single(name, &tbl.cols);
        let cols = tbl.cols.clone();
        let rows = tbl.rows.clone();
        let mut targets = Vec::new();
        let ucx = Ctx {
            fault: self.fault,
            names: &names,
            row: None,
            aggs: &[],
            outer: None,
        };
        for (c, e) in &sets {
            let i = cols
                .iter()
                .position(|(n, _)| n == c)
                .ok_or(ErrClass::NoColumn)?;
            self.check_cols(e, &ucx)?;
            targets.push(i);
        }
        if let Some(w) = &wh {
            self.check_cols(w, &ucx)?;
        }
        let mut new_rows = rows.clone();
        let (mut matched, mut changed) = (0u64, 0u64);
        for (ri, row) in rows.iter().enumerate() {
            let rcx = Ctx {
                fault: self.fault,
                names: &names,
                row: Some(row),
                aggs: &[],
                outer: None,
            };
            if !self.keep(&wh, &rcx)? {
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
                let scx = Ctx {
                    fault: self.fault,
                    names: &names,
                    row: Some(basis),
                    aggs: &[],
                    outer: None,
                };
                let v = self.eval(e, &scx)?;
                updated[ti] = self.coerce(v, cols[ti].1)?;
            }
            self.update_row_checks(name, &updated, ri, &new_rows)?;
            if updated != *row {
                changed += 1;
            }
            new_rows[ri] = updated;
        }
        // Statement end: the FK checks see every new row.
        self.fk_checks(name, &new_rows.iter().collect::<Vec<_>>())?;
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
        let names = Names::single(name, &tbl.cols);
        let rows = tbl.rows.clone();
        let dcx = Ctx {
            fault: self.fault,
            names: &names,
            row: None,
            aggs: &[],
            outer: None,
        };
        if let Some(w) = &wh {
            self.check_cols(w, &dcx)?;
        }
        let mut kept = Vec::new();
        let mut deleted: Vec<Vec<V>> = Vec::new();
        for row in rows {
            let rcx = Ctx {
                fault: self.fault,
                names: &names,
                row: Some(&row),
                aggs: &[],
                outer: None,
            };
            if self.keep(&wh, &rcx)? {
                deleted.push(row);
            } else {
                kept.push(row);
            }
        }
        let mut removed = deleted.len() as u64;
        // A referenced parent cannot be deleted (restrict).
        self.delete_fk_restrict(name, &deleted, &tbl.cols)?;
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
        for last in [0u32, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
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
    /// Ticket 13's soak found this: SQLite checks an `INSERT ... SELECT`'s
    /// declared width at prepare time, so a zero-row SELECT whose width is
    /// wrong is still `arity`.
    #[test]
    fn insert_select_width_is_checked_before_rows() {
        let mut t = Toy::new(None);
        assert!(t.exec("CREATE TABLE src (a INTEGER)").is_ok());
        assert!(t.exec("CREATE TABLE dst (a INTEGER, b INTEGER)").is_ok());
        assert!(t.exec("CREATE TABLE empty (a INTEGER)").is_ok());
        let got = format!("{:?}", t.exec("INSERT INTO dst SELECT a, a, a FROM empty"));
        assert!(got.contains("arity"), "{got}");
    }

    #[test]
    #[ignore]
    fn clean_toy_soak() {
        // Through the newest covered stage: the soak must never fall
        // behind the generator again (ticket 10 shipped unsoaked).
        let p = Profile::through(crate::verifier::sql::driver::MAX_DIFFERENTIAL_STAGE);
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

    /// Joins: inner/comma shapes, `LEFT JOIN` padding unmatched rows with
    /// NULLs, qualified references (alias and table name), ambiguity for a
    /// bare name in two items, and `ON` predicates.
    #[test]
    fn joins_inner_left_and_ambiguity() {
        let mut t = Toy::new(None);
        assert!(matches!(
            t.exec("CREATE TABLE j1 (x INTEGER, y TEXT)"),
            Ok(Response::Ok(0))
        ));
        assert!(matches!(
            t.exec("CREATE TABLE j2 (x INTEGER, z TEXT)"),
            Ok(Response::Ok(0))
        ));
        assert!(matches!(
            t.exec("INSERT INTO j1 VALUES (1, 'a'), (2, 'b'), (3, 'c')"),
            Ok(Response::Ok(3))
        ));
        assert!(matches!(
            t.exec("INSERT INTO j2 VALUES (2, 'B'), (3, 'C'), (4, 'D')"),
            Ok(Response::Ok(3))
        ));
        let rows = |r: Result<Response, String>| match r {
            Ok(Response::Rows { rows, headers, .. }) => (headers, rows),
            other => panic!("expected rows: {other:?}"),
        };
        // INNER JOIN: the matching pairs only.
        let (h, r) = rows(
            t.exec("SELECT j1.x AS k, j2.z AS v FROM j1 INNER JOIN j2 ON j1.x = j2.x ORDER BY k"),
        );
        assert_eq!(h, vec!["k", "v"]);
        assert_eq!(r.len(), 2);
        // Comma join + WHERE, qualified by the table names.
        let (_, r) = rows(t.exec("SELECT j1.y AS a FROM j1, j2 WHERE j1.x = j2.x"));
        assert_eq!(r.len(), 2);
        // LEFT JOIN pads the missing right side with NULL.
        let (_, r) = rows(
            t.exec("SELECT j1.x AS k, j2.x AS m FROM j1 LEFT JOIN j2 ON j1.x = j2.x ORDER BY k"),
        );
        assert_eq!(r.len(), 3);
        assert!(
            r.iter().any(|row| row[1] == "\\N"),
            "LEFT JOIN must pad with NULL: {r:?}"
        );
        // A bare name present in both items is ambiguous.
        assert!(matches!(
            t.exec("SELECT x FROM j1, j2"),
            Ok(Response::Err { ref class, .. }) if class == "ambiguous"
        ));
        // An unknown qualifier, and a missing column, are no_column.
        assert!(matches!(
            t.exec("SELECT zz.x FROM j1, j2"),
            Ok(Response::Err { ref class, .. }) if class == "no_column"
        ));
        assert!(matches!(
            t.exec("SELECT j1.zz FROM j1, j2"),
            Ok(Response::Err { ref class, .. }) if class == "no_column"
        ));
        // The generated shape that the agreement gate found: an ON
        // predicate mixing a qualified and a unique bare name.
        let (_, r) = rows(t.exec(
            "SELECT j1.x = (j2.x % 48) AS e0 FROM j1 INNER JOIN j2 ON j1.x = j2.z ORDER BY e0",
        ));
        assert!(r.is_empty(), "a text/int join matches nothing: {r:?}");
    }

    /// Stage 9: an INTEGER PRIMARY KEY assigns max+1 (1 when empty) on
    /// NULL/omitted, a TEXT PRIMARY KEY rejects NULL, omitted columns read
    /// their DEFAULT, `NOT NULL`/`UNIQUE` reject violations (several NULLs
    /// are fine) and `REFERENCES` enforces restrict semantics — including
    /// on `DELETE` of a referenced parent.
    #[test]
    fn constraints_pk_defaults_unique_and_fk() {
        let mut t = Toy::new(None);
        let ok0 = |r: Result<Response, String>| matches!(r, Ok(Response::Ok(0)));
        let ok_n = |r: Result<Response, String>, n: u64| matches!(r, Ok(Response::Ok(m)) if m == n);
        let constraint = |r: Result<Response, String>| matches!(r, Ok(Response::Err { ref class, .. }) if class == "constraint");
        let rows = |r: Result<Response, String>| match r {
            Ok(Response::Rows { rows, .. }) => rows,
            other => panic!("expected rows: {other:?}"),
        };
        assert!(ok0(
            t.exec("CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT)")
        ));
        assert!(ok_n(t.exec("INSERT INTO p (name) VALUES ('a')"), 1));
        assert!(ok_n(t.exec("INSERT INTO p VALUES (NULL, 'b')"), 1));
        assert_eq!(
            rows(t.exec("SELECT id FROM p ORDER BY id")),
            vec![vec!["1"], vec!["2"]],
            "auto-assign is max+1, 1 when empty"
        );
        assert!(ok0(t.exec(
            "CREATE TABLE d (id INTEGER PRIMARY KEY, n INTEGER NOT NULL DEFAULT -5, u INTEGER UNIQUE)"
        )));
        assert!(ok_n(t.exec("INSERT INTO d (id) VALUES (NULL)"), 1));
        assert_eq!(rows(t.exec("SELECT n FROM d")), vec![vec!["-5"]], "DEFAULT");
        assert!(constraint(t.exec("INSERT INTO d (n) VALUES (NULL)")));
        assert!(ok_n(t.exec("INSERT INTO d (u) VALUES (7)"), 1));
        assert!(constraint(t.exec("INSERT INTO d (u) VALUES (7)")));
        assert!(ok_n(t.exec("INSERT INTO d (u) VALUES (NULL), (NULL)"), 2));
        assert!(ok0(t.exec(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id))"
        )));
        assert!(ok_n(t.exec("INSERT INTO c (pid) VALUES (1)"), 1));
        assert!(constraint(t.exec("INSERT INTO c (pid) VALUES (99)")));
        assert!(ok_n(t.exec("INSERT INTO c (pid) VALUES (NULL)"), 1));
        assert!(
            constraint(t.exec("DELETE FROM p WHERE id = 1")),
            "referenced parent"
        );
        assert!(
            ok_n(t.exec("DELETE FROM p WHERE id = 2"), 1),
            "unreferenced parent"
        );
        assert_eq!(rows(t.exec("SELECT id FROM p")), vec![vec!["1"]]);
        // A negative max key still gives max + 1 (SQLite verified): -39
        // then NULL is -38, not 1.
        assert!(ok0(
            t.exec("CREATE TABLE neg (k INTEGER PRIMARY KEY, v TEXT)")
        ));
        assert!(ok_n(t.exec("INSERT INTO neg VALUES (-39, 'a')"), 1));
        assert!(ok_n(t.exec("INSERT INTO neg (v) VALUES ('b')"), 1));
        assert_eq!(
            rows(t.exec("SELECT k FROM neg ORDER BY k")),
            vec![vec!["-39"], vec!["-38"]]
        );
        // A TEXT primary key is NOT NULL.
        assert!(ok0(
            t.exec("CREATE TABLE tk (k TEXT PRIMARY KEY, v INTEGER)")
        ));
        assert!(ok_n(t.exec("INSERT INTO tk VALUES ('a', 1)"), 1));
        assert!(constraint(t.exec("INSERT INTO tk VALUES (NULL, 2)")));
    }

    /// Stage 11: indexes are named schema objects; a UNIQUE one constrains
    /// like the column form, and a failed create registers nothing.
    #[test]
    fn indexes_are_schema_objects_and_unique_indexes_constrain() {
        let mut t = Toy::new(None);
        let ok0 = |r: Result<Response, String>| matches!(r, Ok(Response::Ok(0)));
        let ok_n = |r: Result<Response, String>, n: u64| matches!(r, Ok(Response::Ok(m)) if m == n);
        let constraint = |r: Result<Response, String>| matches!(r, Ok(Response::Err { ref class, .. }) if class == "constraint");
        let err = |r: Result<Response, String>, tok: &str| matches!(r, Ok(Response::Err { ref class, .. }) if class == tok);
        assert!(ok0(t.exec(
            "CREATE TABLE ix (k INTEGER PRIMARY KEY, v TEXT, w INTEGER)"
        )));
        assert!(ok_n(
            t.exec("INSERT INTO ix (k, v, w) VALUES (1, 'a', 5), (2, 'a', 6)"),
            2
        ));
        // A UNIQUE index over existing duplicates fails, and the failure
        // does not register the name (SQLite lets the next create use it).
        assert!(constraint(t.exec("CREATE UNIQUE INDEX ix_u ON ix (v)")));
        assert!(ok0(t.exec("CREATE UNIQUE INDEX ix_u ON ix (w)")));
        // The name is taken now.
        assert!(err(t.exec("CREATE INDEX ix_u ON ix (w)"), "exists"));
        // Unknown table and column.
        assert!(err(t.exec("CREATE INDEX ix_x ON zz (w)"), "no_table"));
        assert!(err(t.exec("CREATE INDEX ix_x ON ix (nope)"), "no_column"));
        // DROP: once works, twice is no such index.
        assert!(ok0(t.exec("DROP INDEX ix_u")));
        assert!(err(t.exec("DROP INDEX ix_u"), "no_table"));
        // The unique index constrains INSERT and UPDATE like the column
        // form (SQLite verified: both rows getting the same value fails).
        assert!(ok0(t.exec("CREATE UNIQUE INDEX ix_w ON ix (w)")));
        assert!(constraint(
            t.exec("INSERT INTO ix (k, v, w) VALUES (3, 'b', 5)")
        ));
        assert!(ok_n(
            t.exec("INSERT INTO ix (k, v, w) VALUES (3, 'b', 7)"),
            1
        ));
        assert!(constraint(t.exec("UPDATE ix SET w = 5 WHERE k IN (1, 2)")));
        assert!(constraint(t.exec("UPDATE ix SET w = 8 WHERE k IN (1, 2)")));
        assert!(ok_n(t.exec("UPDATE ix SET w = 9 WHERE k = 1"), 1));
        // A transaction rolls the index registry back too.
        assert!(ok0(t.exec("BEGIN")));
        assert!(ok0(t.exec("CREATE INDEX ix_t ON ix (v)")));
        assert!(ok0(t.exec("ROLLBACK")));
        assert!(err(t.exec("DROP INDEX ix_t"), "no_table"));
    }

    /// Stage 10: scalar subqueries (aggregate-wrapped), `[NOT] IN
    /// (subquery)` and `IN (list)` with the three-valued NULL rule,
    /// `[NOT] EXISTS`, correlated references, and `INSERT ... SELECT`.
    #[test]
    fn subqueries_scalar_in_exists_and_insert_select() {
        let mut t = Toy::new(None);
        let ok0 = |r: Result<Response, String>| matches!(r, Ok(Response::Ok(0)));
        let ok_n = |r: Result<Response, String>, n: u64| matches!(r, Ok(Response::Ok(m)) if m == n);
        let rows = |r: Result<Response, String>| match r {
            Ok(Response::Rows { rows, .. }) => rows,
            other => panic!("expected rows: {other:?}"),
        };
        assert!(ok0(t.exec(
            "CREATE TABLE p (id INTEGER PRIMARY KEY, v INTEGER, tag TEXT)"
        )));
        assert!(ok0(t.exec(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER, w INTEGER)"
        )));
        assert!(ok_n(
            t.exec("INSERT INTO p (id, v, tag) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, NULL, 'a')"),
            3
        ));
        assert!(ok_n(
            t.exec("INSERT INTO c (pid, w) VALUES (1, 100), (2, 200)"),
            2
        ));
        // A scalar subquery (aggregate-wrapped) in the select list.
        assert_eq!(
            rows(t.exec("SELECT v, (SELECT COUNT(*) FROM c) AS n FROM p ORDER BY v")),
            vec![vec![r"\N", "2"], vec!["10", "2"], vec!["20", "2"]]
        );
        // Correlated scalar subquery.
        assert_eq!(
            rows(t.exec(
                "SELECT id, (SELECT COUNT(*) FROM c WHERE c.pid = p.id) AS n FROM p ORDER BY id"
            )),
            vec![vec!["1", "1"], vec!["2", "1"], vec!["3", "0"]]
        );
        // IN (subquery), correlated.
        assert_eq!(
            rows(t.exec("SELECT id FROM p WHERE id IN (SELECT pid FROM c) ORDER BY id")),
            vec![vec!["1"], vec!["2"]]
        );
        assert_eq!(
            rows(t.exec("SELECT id FROM p WHERE id NOT IN (SELECT pid FROM c) ORDER BY id")),
            vec![vec!["3"]]
        );
        // IN (list) with NULL: no match + a NULL in the list is NULL.
        assert_eq!(
            rows(t.exec("SELECT id FROM p WHERE v IN (10, NULL) ORDER BY id")),
            vec![vec!["1"]]
        );
        assert_eq!(
            rows(t.exec("SELECT id FROM p WHERE v NOT IN (10, NULL) ORDER BY id")),
            Vec::<Vec<String>>::new()
        );
        assert_eq!(
            rows(t.exec("SELECT id FROM p WHERE v IS NULL ORDER BY id")),
            vec![vec!["3"]]
        );
        // EXISTS / NOT EXISTS, correlated.
        assert_eq!(
            rows(t.exec("SELECT id FROM p WHERE EXISTS (SELECT * FROM c WHERE c.pid = p.id)")),
            vec![vec!["1"], vec!["2"]]
        );
        assert_eq!(
            rows(t.exec("SELECT id FROM p WHERE NOT EXISTS (SELECT * FROM c WHERE c.pid = p.id)")),
            vec![vec!["3"]]
        );
        // INSERT ... SELECT.
        assert!(ok_n(
            t.exec("INSERT INTO c (pid, w) SELECT id, 7 FROM p WHERE id = 3"),
            1
        ));
        assert_eq!(rows(t.exec("SELECT COUNT(*) FROM c")), vec![vec!["3"]]);
        // Errors: a missing table/column in a subquery is `no_table` /
        // `no_column`, a two-column scalar subquery is `arity`.
        assert!(matches!(
            t.exec("SELECT (SELECT COUNT(*) FROM zz) FROM p"),
            Ok(Response::Err { ref class, .. }) if class == "no_table"
        ));
        assert!(matches!(
            t.exec("SELECT * FROM p WHERE id IN (SELECT zz FROM c)"),
            Ok(Response::Err { ref class, .. }) if class == "no_column"
        ));
        assert!(matches!(
            t.exec("SELECT (SELECT id, pid FROM c) FROM p"),
            Ok(Response::Err { ref class, .. }) if class == "arity"
        ));
    }

    /// Every planted bug is caught, and caught OFTEN: a blind spot in the
    /// generator shows up here as a surviving or barely-caught mutant. A
    /// model with such a bug would otherwise pass several verifications in a
    /// row by luck.
    #[test]
    fn every_mutant_is_caught_in_most_scripts() {
        // 60 scripts: the stage-9 profile dilutes any single feature, and a
        // bug that is only caught in a quarter of scripts needs the sample
        // to stay above the floor reliably.
        const SCRIPTS: u64 = 60;
        const MIN_RATE_PCT: u64 = 25;
        // Through stage 11: the aggregate, join, constraint, subquery and
        // index mutants only fire when the profile generates those shapes.
        let p = Profile::through(11);
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
