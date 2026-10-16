//! Seeded, type-directed statement generator for sdb-sql.
//!
//! One generator serves every differential stage: a [`Profile`] switches
//! grammar features on, stage by stage (and lite subsets later), so a stage's
//! suite only ever uses what the dialect has unlocked so far. The oracle
//! (`oracle.rs`) answers every statement; nothing here knows an expected
//! value.
//!
//! Design rules, all from docs/PROPOSAL-sdb-sql.md:
//! - **Typed.** An integer expression never meets a text comparison, and
//!   text is never inserted into an INTEGER column except by the explicit
//!   `type` error probe (using alphabetic text, never `'123'`).
//! - **Total order.** `ORDER BY` always lists every output column, so the
//!   expected order is unambiguous and compared exactly. `LIMIT`/`OFFSET`
//!   only ever follow an `ORDER BY`.
//! - **Aliased.** Every non-bare output column is aliased (`e0`, `e1`, ...),
//!   so header names are always well defined.
//! - **Randomized surface.** Keyword case, whitespace (space, double space,
//!   tab) and the trailing `;` vary; a candidate's tokenizer is part of the
//!   test.
//! - **No `END` text.** Text literals never start with `END` (the wire
//!   framing marker, see `wire.rs`).
//! - **Constraint-free until stage 9.** Primary keys, UNIQUE, NOT NULL and
//!   DEFAULT only appear when the profile enables `constraints`.

use super::oracle::{Oracle, OracleOutcome};
use super::schema::{self, Rng, Schema, Table, Ty};
use std::collections::HashMap;

/// Grammar features unlocked by passed stages. Cumulative via [`union`].
///
/// [`union`]: Profile::union
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    pub multi_row_insert: bool,
    pub column_list_insert: bool,
    pub where_clause: bool,
    pub expressions: bool,
    pub select_no_from: bool,
    pub order_limit: bool,
    pub distinct: bool,
    pub update: bool,
    pub delete: bool,
    /// `BEGIN`/`COMMIT`/`ROLLBACK`: transactions, kept balanced per script.
    pub txn: bool,
    /// `COUNT`/`SUM`/`MIN`/`MAX`, `GROUP BY`, `HAVING` (ticket 7).
    pub aggregates: bool,
    /// Joins: comma lists and `INNER JOIN` (ticket 8).
    pub joins: bool,
    /// `LEFT [OUTER] JOIN` (stage 8 full only; the lite variant keeps
    /// inner/comma joins).
    pub left_join: bool,
    pub constraints: bool,
}

impl Profile {
    /// What ticket `stage` adds to the differential grammar.
    pub fn stage(stage: u32) -> Profile {
        let mut p = Profile::default();
        match stage {
            0 => p.multi_row_insert = true,
            1 => {
                p.column_list_insert = true;
                p.where_clause = true;
                p.expressions = true;
                p.select_no_from = true;
            }
            2 => {
                p.order_limit = true;
                p.distinct = true;
            }
            3 => {
                p.update = true;
                p.delete = true;
            }
            6 => p.txn = true,
            7 => p.aggregates = true,
            8 => {
                p.joins = true;
                p.left_join = true;
            }
            9 => p.constraints = true,
            _ => {}
        }
        p
    }

    pub fn union(&self, o: &Profile) -> Profile {
        Profile {
            multi_row_insert: self.multi_row_insert || o.multi_row_insert,
            column_list_insert: self.column_list_insert || o.column_list_insert,
            where_clause: self.where_clause || o.where_clause,
            expressions: self.expressions || o.expressions,
            select_no_from: self.select_no_from || o.select_no_from,
            order_limit: self.order_limit || o.order_limit,
            distinct: self.distinct || o.distinct,
            update: self.update || o.update,
            delete: self.delete || o.delete,
            txn: self.txn || o.txn,
            aggregates: self.aggregates || o.aggregates,
            joins: self.joins || o.joins,
            left_join: self.left_join || o.left_join,
            constraints: self.constraints || o.constraints,
        }
    }

    /// The cumulative profile of a set of passed stages.
    pub fn for_passed(stages: &[u32]) -> Profile {
        stages
            .iter()
            .fold(Profile::default(), |acc, s| acc.union(&Profile::stage(*s)))
    }

    /// Apply a stage's lite flags: `join-inner-only` (stage 8's lite
    /// variant) drops `LEFT JOIN` and keeps comma/inner joins.
    pub fn narrow(&mut self, flags: &[String]) {
        if flags.iter().any(|f| f == "join-inner-only") {
            self.left_join = false;
        }
    }

    /// Stages `0..=last`.
    pub fn through(last: u32) -> Profile {
        let all: Vec<u32> = (0..=last).collect();
        Profile::for_passed(&all)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Create,
    Insert,
    Select,
    Update,
    Delete,
    /// `BEGIN`, `COMMIT` or `ROLLBACK`.
    Txn,
    /// A deliberate error (missing table/column, arity, type, syntax,
    /// duplicate create, atomicity, nested `BEGIN`, bare `COMMIT`).
    Probe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,
    pub kind: Kind,
    /// The result is totally ordered (every output column in `ORDER BY`),
    /// so it compares as a sequence; otherwise as a multiset.
    pub ordered: bool,
}

#[derive(Clone)]
struct ColInfo {
    name: String,
    ty: Ty,
    pk: bool,
    unique: bool,
    not_null: bool,
    default: bool,
    /// `REFERENCES parent(column)`, when the profile enables constraints.
    references: Option<(String, String)>,
}

/// Generated expression: SQL text, its precedence level, and a conservative
/// bound.
///
/// Precedence: 0 is an atom or function call (needs no parentheses), then
/// `||` is 1, `* / %` is 2, `+ -` is 3, and comparisons and boolean
/// operators are 5.
///
/// Bound: for an integer expression, its maximum magnitude; for a text
/// expression, its maximum length; booleans use the value one. Bounds are
/// what keeps the workload overflow-free: SQLite silently promotes an
/// overflowing intermediate to REAL (a comparison then hides it), the
/// dialect leaves overflow untested, and a REAL the oracle cannot see would
/// be graded as if it were defined behavior.
type Ex = (String, u8, u128);

/// Largest magnitude an integer expression may reach (no 64-bit overflow).
const INT_BOUND: u128 = i64::MAX as u128;
/// Longest text an expression or column may reach. Without a cap,
/// `txt = txt || txt` doubles every row's text with each UPDATE.
const TEXT_BOUND: u128 = 48;

pub struct Generator {
    rng: Rng,
    schema: Schema,
    profile: Profile,
    /// Monotone counter for values that must be unique (keys, UNIQUE).
    uniq: u64,
    /// Recently inserted literals per column name (SQL text), so predicates
    /// can be anchored on values that really exist.
    seen: HashMap<String, Vec<String>>,
    /// True while the generated stream is inside an open transaction.
    /// `build_script` closes it at the end so no script leaves one open.
    txn_open: bool,
    /// Per table: an upper bound on its row count (every generated INSERT
    /// counts; deletes never lower it). Only `SUM`'s overflow guard uses
    /// it, so being conservative is free.
    rows: HashMap<String, u128>,
    /// Per column: the largest magnitude (INTEGER) or length (TEXT) any
    /// value of it can have. Seeded from the schema's `DEFAULT`s (a column
    /// omitted from every explicit insert still holds its default) and
    /// bumped by every generated literal and assignment. Conservative:
    /// a generated statement counts even if the oracle later rejects it.
    bounds: HashMap<String, u128>,
}

const TEXT_ALPHABET: &[u8] = b"abcdefghXYZ0123456789_-";

impl Generator {
    pub fn new(seed: u64, schema: Schema, profile: Profile) -> Self {
        // A column that is never explicitly written still holds its
        // DEFAULT, so the bound has to start there or the overflow and
        // TEXT_BOUND reasoning is wrong from the first read.
        let mut bounds: HashMap<String, u128> = HashMap::new();
        for t in &schema.tables {
            for c in &t.columns {
                if let Some(d) = &c.default {
                    let b = match d {
                        schema::DefaultVal::Int(v) => u128::from(v.unsigned_abs()),
                        schema::DefaultVal::Text(s) => s.chars().count() as u128,
                    };
                    // Bounds are keyed by column name; keep the max so a
                    // name shared across tables never lowers a bound.
                    let e = bounds.entry(c.name.clone()).or_insert(0);
                    *e = (*e).max(b);
                }
            }
        }
        Generator {
            rng: Rng::new(seed ^ 0xD1CE_5EED_0000_0001),
            schema,
            profile,
            uniq: 0,
            seen: HashMap::new(),
            rows: HashMap::new(),
            bounds,
            txn_open: false,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// `CREATE TABLE` for every table, in dialect form.
    pub fn setup_statements(&self) -> Vec<String> {
        if self.profile.constraints {
            self.schema.create_statements()
        } else {
            self.schema.create_statements_plain()
        }
    }

    // ----- surface randomization -------------------------------------

    /// A keyword/function name in random case (mostly upper).
    fn kw(&mut self, w: &str) -> String {
        match self.rng.below(10) {
            0..=6 => w.to_ascii_uppercase(),
            7 => w.to_ascii_lowercase(),
            _ => w
                .chars()
                .map(|c| {
                    if self.rng.chance(50) {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect(),
        }
    }

    /// Whitespace between clauses.
    fn sp(&mut self) -> &'static str {
        match self.rng.below(10) {
            0..=7 => " ",
            8 => "  ",
            _ => "\t",
        }
    }

    fn join(&mut self, parts: Vec<String>) -> String {
        let mut out = String::new();
        for (i, p) in parts.into_iter().enumerate() {
            if i > 0 {
                out.push_str(self.sp());
            }
            out.push_str(&p);
        }
        out
    }

    /// Optional trailing semicolon.
    fn finish(&mut self, mut sql: String) -> String {
        if self.rng.chance(30) {
            sql.push(';');
        }
        sql
    }

    fn stmt(&mut self, sql: String, kind: Kind, ordered: bool) -> Statement {
        Statement {
            sql: self.finish(sql),
            kind,
            ordered,
        }
    }

    // ----- schema views ------------------------------------------------

    fn table_infos(&self, idx: usize) -> (String, Vec<ColInfo>) {
        let t: &Table = self.schema.table(idx);
        let cons = self.profile.constraints;
        let cols = t
            .columns
            .iter()
            .map(|c| ColInfo {
                name: c.name.clone(),
                ty: c.ty,
                pk: cons && c.primary_key,
                unique: cons && c.unique,
                not_null: cons && c.not_null,
                default: cons && c.default.is_some(),
                references: if cons { c.references.clone() } else { None },
            })
            .collect();
        (t.name.clone(), cols)
    }

    fn pick_table(&mut self) -> usize {
        self.rng.below(self.schema.tables.len() as u64) as usize
    }

    // ----- literals ------------------------------------------------------

    fn int_value(&mut self) -> i64 {
        match self.rng.below(10) {
            0..=5 => self.rng.range(-9, 9),
            6..=8 => self.rng.range(-100, 100),
            _ => self.rng.range(-100_000, 100_000),
        }
    }

    /// An integer literal safe as an expression operand (negatives wrapped).
    fn int_atom(&mut self) -> Ex {
        if self.rng.chance(6) {
            return ("NULL".to_string(), 0, 0);
        }
        let v = self.int_value();
        let text = if v < 0 {
            format!("({v})")
        } else {
            v.to_string()
        };
        (text, 0, u128::from(v.unsigned_abs()))
    }

    fn text_raw(&mut self) -> String {
        let len = self.rng.below(7) as usize;
        let mut s = String::new();
        for _ in 0..len {
            match self.rng.below(100) {
                0..=2 => s.push('\''),
                3..=4 => s.push('\\'),
                5 => s.push('\t'),
                6..=11 => s.push(' '),
                _ => s.push(
                    TEXT_ALPHABET[self.rng.below(TEXT_ALPHABET.len() as u64) as usize] as char,
                ),
            }
        }
        s
    }

    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    fn text_atom(&mut self) -> Ex {
        if self.rng.chance(6) {
            return ("NULL".to_string(), 0, 0);
        }
        let raw = self.text_raw();
        let len = raw.chars().count() as u128;
        (Self::quote(&raw), 0, len)
    }

    /// The bare name of a possibly qualified column reference (`a.x` ->
    /// `x`): bounds, anchors and `seen` are keyed by the schema name.
    fn bare_ref(col: &str) -> &str {
        col.rsplit('.').next().unwrap_or(col)
    }

    fn bound_of(&self, col: &str) -> u128 {
        self.bounds
            .get(col)
            .or_else(|| self.bounds.get(Self::bare_ref(col)))
            .copied()
            .unwrap_or(0)
    }

    fn bump(&mut self, col: &str, to: u128) {
        let b = self.bounds.entry(col.to_string()).or_insert(0);
        *b = (*b).max(to);
    }

    // ----- expressions ---------------------------------------------------

    /// Does `sql` reference one of `cols` as a whole identifier? Predicates
    /// and assignments that never read a column are constants: they filter
    /// everything or nothing and prove nothing about the candidate.
    fn mentions_col(sql: &str, cols: &[ColInfo]) -> bool {
        sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|tok| {
                cols.iter()
                    .any(|c| c.name == tok || Self::bare_ref(&c.name) == tok)
            })
    }

    /// Parenthesize a composite operand most of the time; the rest relies on
    /// standard SQL precedence, which a candidate's parser must get right.
    fn wrap(&mut self, e: Ex, pct: u64) -> String {
        if e.1 == 0 || !self.rng.chance(pct) {
            e.0
        } else {
            format!("({})", e.0)
        }
    }

    /// Parenthesize an arithmetic operand. A child may go bare only if the
    /// printed text still parses to the tree that was generated (the bound
    /// reasoning depends on it): it binds tighter than its parent, or it is
    /// the LEFT operand of the same level. A looser child, or a right
    /// operand of the same level, would re-associate (`a - (b - c)` printed
    /// bare is `(a - b) - c`), so it keeps its parentheses. Left
    /// associativity itself is exercised by the explicit chains.
    fn wrap_arith(&mut self, e: Ex, parent: u8, right: bool, pct: u64) -> String {
        let required = e.1 > parent || (e.1 == parent && right);
        if e.1 != 0 && (required || self.rng.chance(pct)) {
            format!("({})", e.0)
        } else {
            e.0
        }
    }

    fn int_leaf(&mut self, cols: &[ColInfo]) -> Ex {
        let ints: Vec<&ColInfo> = cols.iter().filter(|c| c.ty == Ty::Integer).collect();
        if !ints.is_empty() && self.rng.chance(60) {
            let c = ints[self.rng.below(ints.len() as u64) as usize];
            return (c.name.clone(), 0, self.bound_of(&c.name));
        }
        self.int_atom()
    }

    fn text_leaf(&mut self, cols: &[ColInfo]) -> Ex {
        let texts: Vec<&ColInfo> = cols.iter().filter(|c| c.ty == Ty::Text).collect();
        if !texts.is_empty() && self.rng.chance(60) {
            let c = texts[self.rng.below(texts.len() as u64) as usize];
            return (c.name.clone(), 0, self.bound_of(&c.name));
        }
        self.text_atom()
    }

    /// An integer expression whose magnitude provably fits in 64 bits.
    fn int_expr(&mut self, cols: &[ColInfo], depth: u32) -> Ex {
        if depth == 0 || self.rng.chance(35) {
            return self.int_leaf(cols);
        }
        let built: Ex = match self.rng.below(10) {
            // A bare chain of two operators from the same precedence level,
            // `a - b - c`, `a / b % c`: left associativity is part of the
            // dialect and only a chain with no parentheses can test it.
            8..=9 => {
                let level: &[&str] = if self.rng.chance(50) {
                    &["+", "-"]
                } else {
                    &["*", "/", "%"]
                };
                let (a, b, c) = (
                    self.int_leaf(cols),
                    self.int_leaf(cols),
                    self.int_leaf(cols),
                );
                let (o1, o2) = (*self.rng.pick(level), *self.rng.pick(level));
                let step = |op: &str, x: u128, y: u128| match op {
                    "+" | "-" => x.saturating_add(y),
                    "*" => x.saturating_mul(y),
                    "/" => x,
                    _ => x.min(y),
                };
                // The intermediate result must fit too: `%` shrinks the final
                // bound back down after a product that already overflowed.
                let mid = step(o1, a.2, b.2);
                let bound = if mid > INT_BOUND {
                    u128::MAX
                } else {
                    step(o2, mid, c.2)
                };
                let prec = if level[0] == "+" { 3 } else { 2 };
                (format!("{} {o1} {} {o2} {}", a.0, b.0, c.0), prec, bound)
            }
            0..=3 => {
                let l = self.int_expr(cols, depth - 1);
                let r = self.int_expr(cols, depth - 1);
                let op = *self.rng.pick(&["+", "-", "*", "/", "%"]);
                let bound = match op {
                    "+" | "-" => l.2.saturating_add(r.2),
                    "*" => l.2.saturating_mul(r.2),
                    "/" => l.2,
                    _ => l.2.min(r.2),
                };
                let prec = if op == "+" || op == "-" { 3 } else { 2 };
                let (l, r) = (
                    self.wrap_arith(l, prec, false, 60),
                    self.wrap_arith(r, prec, true, 60),
                );
                (format!("{l} {op} {r}"), prec, bound)
            }
            4 => {
                let e = self.int_expr(cols, depth - 1);
                (format!("-({})", e.0), 0, e.2)
            }
            5 => {
                let e = self.int_expr(cols, depth - 1);
                let f = self.kw("abs");
                (format!("{f}({})", e.0), 0, e.2)
            }
            6 => {
                let e = self.text_expr(cols, depth - 1);
                let f = self.kw("length");
                (format!("{f}({})", e.0), 0, e.2)
            }
            _ => {
                let a = self.int_expr(cols, depth - 1);
                let b = self.int_expr(cols, depth - 1);
                let f = self.kw("coalesce");
                (format!("{f}({}, {})", a.0, b.0), 0, a.2.max(b.2))
            }
        };
        if built.2 > INT_BOUND {
            self.int_leaf(cols)
        } else {
            built
        }
    }

    /// A text expression no longer than [`TEXT_BOUND`].
    fn text_expr(&mut self, cols: &[ColInfo], depth: u32) -> Ex {
        if depth == 0 || self.rng.chance(35) {
            return self.text_leaf(cols);
        }
        let built: Ex = match self.rng.below(5) {
            0..=1 => {
                let e = self.text_expr(cols, depth - 1);
                let name = if self.rng.chance(50) {
                    "upper"
                } else {
                    "lower"
                };
                let f = self.kw(name);
                (format!("{f}({})", e.0), 0, e.2)
            }
            2..=3 => {
                let l = self.text_expr(cols, depth - 1);
                let r = self.text_expr(cols, depth - 1);
                let bound = l.2 + r.2;
                let (l, r) = (self.wrap(l, 60), self.wrap(r, 60));
                (format!("{l} || {r}"), 1, bound)
            }
            _ => {
                let a = self.text_expr(cols, depth - 1);
                let b = self.text_expr(cols, depth - 1);
                let f = self.kw("coalesce");
                (format!("{f}({}, {})", a.0, b.0), 0, a.2.max(b.2))
            }
        };
        if built.2 > TEXT_BOUND {
            self.text_leaf(cols)
        } else {
            built
        }
    }

    fn comparison(&mut self, cols: &[ColInfo]) -> Ex {
        // Anchored: compare a column with a value that was really inserted,
        // so the predicate selects rows instead of (almost) never matching.
        let anchored: Vec<usize> = (0..cols.len())
            .filter(|&i| {
                self.seen
                    .get(Self::bare_ref(&cols[i].name))
                    .is_some_and(|v| !v.is_empty())
            })
            .collect();
        if !anchored.is_empty() && self.rng.chance(45) {
            let c = &cols[anchored[self.rng.below(anchored.len() as u64) as usize]];
            let key = Self::bare_ref(&c.name);
            let n = self.seen[key].len();
            let lit = self.seen[key][self.rng.below(n as u64) as usize].clone();
            let op = *self.rng.pick(&["=", "=", "<>", "<", "<=", ">", ">="]);
            return (format!("{} {op} {lit}", c.name), 5, 1);
        }
        let mut out = (String::new(), 5, 1);
        for _ in 0..4 {
            let op = *self.rng.pick(&["=", "<>", "!=", "<", "<=", ">", ">="]);
            let (l, r) = if self.rng.chance(60) {
                (self.int_expr(cols, 1), self.int_expr(cols, 1))
            } else {
                (self.text_expr(cols, 1), self.text_expr(cols, 1))
            };
            let (l, r) = (self.wrap(l, 60), self.wrap(r, 60));
            out = (format!("{l} {op} {r}"), 5, 1);
            if cols.is_empty() || Self::mentions_col(&out.0, cols) {
                break;
            }
        }
        out
    }

    fn bool_expr(&mut self, cols: &[ColInfo], depth: u32) -> Ex {
        if depth == 0 || self.rng.chance(50) {
            return if self.rng.chance(25) {
                let mut e = (String::new(), 0, 0);
                for _ in 0..4 {
                    e = if self.rng.chance(50) {
                        self.int_expr(cols, 1)
                    } else {
                        self.text_expr(cols, 1)
                    };
                    if cols.is_empty() || Self::mentions_col(&e.0, cols) {
                        break;
                    }
                }
                let e = self.wrap(e, 60);
                let is = self.kw("IS");
                let not = if self.rng.chance(50) {
                    format!("{} ", self.kw("NOT"))
                } else {
                    String::new()
                };
                let null = self.kw("NULL");
                (format!("{e} {is} {not}{null}"), 5, 1)
            } else {
                self.comparison(cols)
            };
        }
        match self.rng.below(5) {
            0..=2 => {
                let l = self.bool_expr(cols, depth - 1);
                let r = self.bool_expr(cols, depth - 1);
                let (l, r) = (self.wrap(l, 70), self.wrap(r, 70));
                let name = if self.rng.chance(50) { "AND" } else { "OR" };
                let op = self.kw(name);
                (format!("{l} {op} {r}"), 5, 1)
            }
            3 => {
                let b = self.bool_expr(cols, depth - 1);
                let not = self.kw("NOT");
                (format!("{not} ({})", b.0), 5, 1)
            }
            _ => self.comparison(cols),
        }
    }

    // ----- cells -----------------------------------------------------------

    /// One INSERT cell for column `c`; non-NULL literals are remembered.
    fn cell(&mut self, c: &ColInfo) -> String {
        let lit = self.cell_inner(c);
        // An INTEGER PRIMARY KEY that gets NULL (or is omitted) is
        // auto-assigned max+1, so the column's bound must allow one more
        // than any value it already holds.
        if c.pk && c.ty == Ty::Integer && lit == "NULL" && self.profile.constraints {
            let b = self.bound_of(&c.name);
            self.bump(&c.name, b.saturating_add(1));
        }
        match c.ty {
            Ty::Integer => {
                if let Ok(v) = lit.parse::<i64>() {
                    self.bump(&c.name, u128::from(v.unsigned_abs()));
                }
            }
            Ty::Text => {
                if lit != "NULL" {
                    self.bump(&c.name, lit.len() as u128);
                }
            }
        }
        if lit != "NULL" {
            let seen = self.seen.entry(c.name.clone()).or_default();
            if seen.len() >= 16 {
                seen.remove(0);
            }
            seen.push(lit.clone());
        }
        lit
    }

    fn cell_inner(&mut self, c: &ColInfo) -> String {
        if c.pk {
            return match c.ty {
                Ty::Integer => {
                    if self.rng.chance(70) {
                        "NULL".to_string()
                    } else {
                        self.uniq += 1;
                        (100_000 + self.uniq).to_string()
                    }
                }
                Ty::Text => {
                    self.uniq += 1;
                    format!("'k{}'", self.uniq)
                }
            };
        }
        if c.unique {
            if !c.not_null && self.rng.chance(5) {
                return "NULL".to_string();
            }
            self.uniq += 1;
            return match c.ty {
                Ty::Integer => (1_000_000 + self.uniq).to_string(),
                Ty::Text => format!("'u{}'", self.uniq),
            };
        }
        if let Some((_, pc)) = &c.references {
            // A foreign key: a parent value most of the time (anchored on
            // the parent's inserted values), NULL sometimes (an FK ignores
            // NULLs). A missing parent is a deliberate probe, never noise.
            let parent: Option<String> = self
                .seen
                .get(pc)
                .filter(|v| !v.is_empty())
                .map(|v| v[self.rng.below(v.len() as u64) as usize].clone());
            return match self.rng.below(100) {
                0..=54 => parent.unwrap_or_else(|| "NULL".to_string()),
                _ => "NULL".to_string(),
            };
        }
        if !c.not_null && self.rng.chance(10) {
            return "NULL".to_string();
        }
        match c.ty {
            Ty::Integer => {
                if self.rng.chance(1) {
                    i64::MAX.to_string()
                } else {
                    self.int_value().to_string()
                }
            }
            Ty::Text => {
                let raw = self.text_raw();
                Self::quote(&raw)
            }
        }
    }

    // ----- statements ------------------------------------------------------

    pub fn insert_into(&mut self, ti: usize) -> Statement {
        let (tname, cols) = self.table_infos(ti);
        let use_list = self.profile.column_list_insert && self.rng.chance(50);
        let rows_n = if self.profile.multi_row_insert {
            1 + self.rng.below(4) as usize
        } else {
            1
        };
        let mut chosen: Vec<ColInfo> = Vec::new();
        if use_list {
            for c in &cols {
                let must = (c.not_null && !c.default) || (c.pk && c.ty == Ty::Text);
                if must || self.rng.chance(65) {
                    chosen.push(c.clone());
                }
            }
            if chosen.is_empty() {
                chosen.push(cols[0].clone());
            }
            // Fisher-Yates: column order in the list is free.
            for i in (1..chosen.len()).rev() {
                let j = self.rng.below(i as u64 + 1) as usize;
                chosen.swap(i, j);
            }
        } else {
            chosen = cols.clone();
        }
        let mut rows = Vec::new();
        for _ in 0..rows_n {
            let cells: Vec<String> = chosen.iter().map(|c| self.cell(c)).collect();
            rows.push(format!("({})", cells.join(", ")));
        }
        // Every insert (seed or step, accepted or rejected) counts towards
        // the table's row bound: SUM's overflow guard stays conservative.
        let rows_bound = self.rows.entry(tname.clone()).or_insert(0);
        *rows_bound += rows.len() as u128;
        let mut parts = vec![self.kw("INSERT"), self.kw("INTO")];
        if use_list {
            let names: Vec<&str> = chosen.iter().map(|c| c.name.as_str()).collect();
            parts.push(format!("{} ({})", tname, names.join(", ")));
        } else {
            parts.push(tname);
        }
        parts.push(self.kw("VALUES"));
        parts.push(rows.join(", "));
        let sql = self.join(parts);
        self.stmt(sql, Kind::Insert, false)
    }

    pub fn select_stmt(&mut self) -> Statement {
        // Aggregate queries first: they have their own select-list rules.
        if self.profile.aggregates && self.rng.below(100) < 30 {
            return self.aggregate_stmt();
        }
        // Join queries (ticket 8), single-table SELECTs below.
        if self.profile.joins && self.rng.below(100) < 30 {
            return self.join_select();
        }
        // SELECT without FROM: pure expressions.
        if self.profile.select_no_from && self.rng.chance(8) {
            let n = 1 + self.rng.below(3) as usize;
            let mut items = Vec::new();
            for i in 0..n {
                let e = if self.rng.chance(70) {
                    self.int_expr(&[], 2)
                } else {
                    self.bool_expr(&[], 1)
                };
                let alias = self.kw("AS");
                items.push(format!("{} {alias} e{i}", e.0));
            }
            let parts = vec![self.kw("SELECT"), items.join(", ")];
            let sql = self.join(parts);
            return self.stmt(sql, Kind::Select, false);
        }

        let ti = self.pick_table();
        let (tname, cols) = self.table_infos(ti);

        // Output items as (sql, header).
        let mut items: Vec<(String, String)> = Vec::new();
        if self.profile.expressions && self.rng.chance(55) {
            let n = 1 + self.rng.below(3) as usize;
            let mut used_bare: Vec<usize> = Vec::new();
            for i in 0..n {
                if self.rng.chance(35) && used_bare.len() < cols.len() {
                    let mut k = self.rng.below(cols.len() as u64) as usize;
                    while used_bare.contains(&k) {
                        k = (k + 1) % cols.len();
                    }
                    used_bare.push(k);
                    items.push((cols[k].name.clone(), cols[k].name.clone()));
                } else {
                    let e = match self.rng.below(3) {
                        0 => self.text_expr(&cols, 2),
                        1 => self.bool_expr(&cols, 1),
                        _ => self.int_expr(&cols, 2),
                    };
                    let alias = self.kw("AS");
                    items.push((format!("{} {alias} e{i}", e.0), format!("e{i}")));
                }
            }
        } else if self.rng.chance(45) {
            for c in &cols {
                items.push((String::new(), c.name.clone()));
            }
        } else {
            let mut idx: Vec<usize> = (0..cols.len()).collect();
            for i in (1..idx.len()).rev() {
                let j = self.rng.below(i as u64 + 1) as usize;
                idx.swap(i, j);
            }
            let k = 1 + self.rng.below(cols.len() as u64) as usize;
            for &i in idx.iter().take(k) {
                items.push((cols[i].name.clone(), cols[i].name.clone()));
            }
        }
        let star = items.iter().all(|(s, _)| s.is_empty());
        let item_sql = if star {
            "*".to_string()
        } else {
            items
                .iter()
                .map(|(s, _)| s.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };

        let mut parts = vec![self.kw("SELECT")];
        if self.profile.distinct && self.rng.chance(25) {
            parts.push(self.kw("DISTINCT"));
        }
        parts.push(item_sql);
        parts.push(self.kw("FROM"));
        parts.push(tname);
        if self.profile.where_clause && self.rng.chance(55) {
            parts.push(self.kw("WHERE"));
            let w = self.bool_expr(&cols, 2);
            parts.push(w.0);
        }
        let mut ordered = false;
        if self.profile.order_limit && self.rng.chance(50) {
            let mut terms: Vec<String> = items.iter().map(|(_, h)| h.clone()).collect();
            for i in (1..terms.len()).rev() {
                let j = self.rng.below(i as u64 + 1) as usize;
                terms.swap(i, j);
            }
            let mut rendered = Vec::new();
            for t in terms {
                let dir = match self.rng.below(10) {
                    0..=2 => String::new(),
                    3..=6 => format!(" {}", self.kw("ASC")),
                    _ => format!(" {}", self.kw("DESC")),
                };
                rendered.push(format!("{t}{dir}"));
            }
            parts.push(format!("{} {}", self.kw("ORDER"), self.kw("BY")));
            parts.push(rendered.join(", "));
            ordered = true;
            if self.rng.chance(50) {
                parts.push(self.kw("LIMIT"));
                parts.push(self.rng.below(9).to_string());
                if self.rng.chance(40) {
                    parts.push(self.kw("OFFSET"));
                    parts.push(self.rng.below(6).to_string());
                }
            }
        }
        let sql = self.join(parts);
        self.stmt(sql, Kind::Select, ordered)
    }

    /// A comparison operator, for `HAVING` predicates.
    fn cmp_op(&mut self) -> String {
        ["=", "<>", "<", "<=", ">", ">="][self.rng.below(6) as usize].to_string()
    }

    /// One aggregate argument: a column, or a one-column expression whose
    /// bound is still exact (`abs`, `length`, `upper`/`lower`, a small
    /// shift or scale). `want_int` restricts to INTEGER arguments.
    fn agg_arg(&mut self, cols: &[ColInfo], want_int: bool) -> Option<(Ex, Ty)> {
        let cands: Vec<&ColInfo> = cols
            .iter()
            .filter(|c| !want_int || c.ty == Ty::Integer)
            .collect();
        if cands.is_empty() {
            return None;
        }
        let c = cands[self.rng.below(cands.len() as u64) as usize];
        let b = self.bound_of(&c.name);
        let e: (Ex, Ty) = match self.rng.below(10) {
            0 if c.ty == Ty::Integer => {
                // `ABS` over TEXT coerces through a REAL (SQLite), which is
                // outside the dialect: INTEGER arguments only.
                (
                    (format!("{}({})", self.kw("abs"), c.name), 0, b),
                    Ty::Integer,
                )
            }
            1 => {
                // LENGTH yields the character count: bounded by the text
                // length, or by 20 digits for an INTEGER.
                let lb = if c.ty == Ty::Text {
                    b.min(TEXT_BOUND)
                } else {
                    20
                };
                (
                    (format!("{}({})", self.kw("length"), c.name), 0, lb),
                    Ty::Integer,
                )
            }
            2..=3 if c.ty == Ty::Text => {
                let f = if self.rng.chance(50) {
                    "upper"
                } else {
                    "lower"
                };
                ((format!("{}({})", self.kw(f), c.name), 0, b), Ty::Text)
            }
            4..=5 if c.ty == Ty::Integer && b < INT_BOUND => {
                // Keep the shifted bound inside 64 bits: SQLite promotes an
                // overflow to REAL, which is outside the dialect.
                let room = (INT_BOUND - b).min(50);
                let k = 1 + self.rng.below(room as u64);
                (
                    (format!("({} + {k})", c.name), 0, b + k as u128),
                    Ty::Integer,
                )
            }
            6 if c.ty == Ty::Integer && b > 0 && b <= INT_BOUND / 9 => {
                let k = 1 + self.rng.below(9);
                (
                    (format!("({} * {k})", c.name), 0, b * k as u128),
                    Ty::Integer,
                )
            }
            _ => ((c.name.clone(), 0, b), c.ty),
        };
        Some(e)
    }

    /// One aggregate output item, with the type of its result.
    fn agg_item(&mut self, cols: &[ColInfo], rows_bound: u128) -> (String, Ty) {
        match self.rng.below(100) {
            0..=19 => (format!("{}(*)", self.kw("count")), Ty::Integer),
            20..=39 => {
                let arg = self.agg_arg(cols, false).map(|(e, _)| e.0);
                let arg = arg.unwrap_or_else(|| cols[0].name.clone());
                (format!("{}({})", self.kw("count"), arg), Ty::Integer)
            }
            40..=64 => {
                // SUM only over INTEGER arguments whose worst case (max
                // magnitude x row bound) still fits: overflow is untested,
                // and SQLite's REAL promotion would be ungradeable.
                if let Some(((e, _, b), _)) = self.agg_arg(cols, true) {
                    if b.saturating_mul(rows_bound) <= INT_BOUND {
                        return (format!("{}({})", self.kw("sum"), e), Ty::Integer);
                    }
                }
                (format!("{}(*)", self.kw("count")), Ty::Integer)
            }
            65..=81 => {
                let arg = self.agg_arg(cols, false);
                match arg {
                    Some(((e, _, _), ty)) => (format!("{}({})", self.kw("min"), e), ty),
                    None => (format!("{}(*)", self.kw("count")), Ty::Integer),
                }
            }
            _ => {
                let arg = self.agg_arg(cols, false);
                match arg {
                    Some(((e, _, _), ty)) => (format!("{}({})", self.kw("max"), e), ty),
                    None => (format!("{}(*)", self.kw("count")), Ty::Integer),
                }
            }
        }
    }

    /// The `GROUP BY` key: a column, or a one-column expression whose text
    /// is reused verbatim as a select item and in `ORDER BY` (SQLite
    /// matches expressions structurally; an alias would not group).
    fn group_key(&mut self, cols: &[ColInfo]) -> Ex {
        let c = &cols[self.rng.below(cols.len() as u64) as usize];
        let b = self.bound_of(&c.name);
        match self.rng.below(10) {
            0..=5 => (c.name.clone(), 0, b),
            6..=7 if c.ty == Ty::Text => {
                let f = if self.rng.chance(50) {
                    "upper"
                } else {
                    "lower"
                };
                (format!("{}({})", self.kw(f), c.name), 0, b)
            }
            8 if c.ty == Ty::Integer && b < INT_BOUND => {
                let room = (INT_BOUND - b).min(50);
                let k = 1 + self.rng.below(room as u64);
                (format!("({} + {k})", c.name), 0, b + k as u128)
            }
            _ => (format!("{}({})", self.kw("length"), c.name), 0, 20),
        }
    }

    /// A `HAVING` predicate over a group's aggregates or its key. All
    /// forms are deterministic; the three-valued logic of the comparison
    /// is part of the dialect.
    fn having_pred(&mut self, cols: &[ColInfo], key: &str, rows_bound: u128) -> String {
        let atom = |g: &mut Self| -> String {
            match g.rng.below(10) {
                0..=3 => {
                    let (sql, _) = g.agg_item(cols, rows_bound);
                    let cmp = g.cmp_op();
                    let k = g.rng.below(6);
                    format!("{sql} {cmp} {k}")
                }
                4..=5 => {
                    let f = if g.rng.chance(50) { "max" } else { "min" };
                    let arg = g
                        .agg_arg(cols, false)
                        .map(|(e, _)| e.0)
                        .unwrap_or_else(|| cols[0].name.clone());
                    format!("{}({arg}) IS NOT NULL", g.kw(f))
                }
                6..=7 => {
                    let neg = if g.rng.chance(50) { "NOT " } else { "" };
                    format!("{key} IS {neg}NULL")
                }
                _ => {
                    let cmp = g.cmp_op();
                    let k = g.rng.below(4);
                    format!("{}(*) {cmp} {k}", g.kw("count"))
                }
            }
        };
        let a = atom(self);
        if self.rng.chance(30) {
            let op = if self.rng.chance(50) { "AND" } else { "OR" };
            let op = self.kw(op);
            let b = atom(self);
            format!("{a} {op} {b}")
        } else {
            a
        }
    }

    /// An aggregate query (ticket 7): `COUNT`/`SUM`/`MIN`/`MAX`, optional
    /// `GROUP BY` and `HAVING`. Fairness and determinism rules:
    /// - the select list holds only aggregates and the group key, never a
    ///   bare column of a grouped query (the dialect leaves those untested);
    /// - the key text is repeated in `SELECT`, `GROUP BY` and `ORDER BY`,
    ///   so the grouped frame is a total order and compares as a sequence;
    /// - a scalar aggregate returns exactly one row even with no match
    ///   (`COUNT` 0, `SUM` NULL), which the empty-group case exercises.
    pub fn aggregate_stmt(&mut self) -> Statement {
        let ti = self.pick_table();
        let (tname, cols) = self.table_infos(ti);
        let rows_bound = self.rows.get(&tname).copied().unwrap_or(2);

        if self.rng.chance(35) {
            let n = 1 + self.rng.below(3) as usize;
            let mut items = Vec::new();
            for i in 0..n {
                let (sql, _) = self.agg_item(&cols, rows_bound);
                items.push(format!("{sql} {} c{i}", self.kw("AS")));
            }
            let mut parts = vec![self.kw("SELECT"), items.join(", "), self.kw("FROM"), tname];
            if self.profile.where_clause && self.rng.chance(45) {
                parts.push(self.kw("WHERE"));
                let w = self.bool_expr(&cols, 2);
                parts.push(w.0);
            }
            let sql = self.join(parts);
            return self.stmt(sql, Kind::Select, true);
        }

        let key = self.group_key(&cols);
        let n = 1 + self.rng.below(3) as usize;
        let mut items: Vec<String> = vec![format!("{} {} k", key.0, self.kw("AS"))];
        for i in 0..n {
            let (sql, _) = self.agg_item(&cols, rows_bound);
            items.push(format!("{sql} {} c{i}", self.kw("AS")));
        }
        let mut parts = vec![self.kw("SELECT"), items.join(", "), self.kw("FROM"), tname];
        if self.profile.where_clause && self.rng.chance(45) {
            parts.push(self.kw("WHERE"));
            let w = self.bool_expr(&cols, 2);
            parts.push(w.0);
        }
        let group = self.kw("GROUP");
        let by = self.kw("BY");
        parts.push(format!("{group} {by} {}", key.0));
        if self.rng.chance(55) {
            let having = self.kw("HAVING");
            let pred = self.having_pred(&cols, &key.0, rows_bound);
            parts.push(format!("{having} {pred}"));
        }
        let order = self.kw("ORDER");
        let by = self.kw("BY");
        let desc = if self.rng.chance(35) {
            format!(" {}", self.kw("DESC"))
        } else {
            String::new()
        };
        parts.push(format!("{order} {by} k{desc}"));
        if self.rng.chance(15) {
            parts.push(self.kw("LIMIT"));
            parts.push(self.rng.below(7).to_string());
        }
        let sql = self.join(parts);
        self.stmt(sql, Kind::Select, true)
    }

    /// The columns of a table by name (for join clauses).
    fn cols_of(&self, tname: &str) -> Vec<ColInfo> {
        let idx = self
            .schema
            .tables
            .iter()
            .position(|t| t.name == tname)
            .unwrap_or(0);
        self.table_infos(idx).1
    }

    /// Two or three `FROM` references: distinct tables, or the same table
    /// twice (a self-join, whose unqualified columns are then ambiguous —
    /// aliases keep qualified references legal).
    fn join_items(&mut self) -> Vec<(String, String)> {
        // Occasionally a single item: a plain query whose references are
        // qualified (`FROM t` with `t.x`), which the dialect resolves.
        if self.rng.chance(12) {
            let ti = self.pick_table();
            let t = self.table_infos(ti).0;
            return vec![(t.clone(), t)];
        }
        let n = if self.rng.chance(30) { 3 } else { 2 };
        if self.rng.chance(25) {
            let ti = self.pick_table();
            let t = self.table_infos(ti).0;
            let mut out = vec![(t.clone(), "a".to_string()), (t, "b".to_string())];
            if n == 3 {
                let ti3 = self.pick_table();
                let t3 = self.table_infos(ti3).0;
                out.push((t3, "c".to_string()));
            }
            return out;
        }
        let mut idx: Vec<usize> = (0..self.schema.tables.len()).collect();
        for i in (1..idx.len()).rev() {
            let j = self.rng.below(i as u64 + 1) as usize;
            idx.swap(i, j);
        }
        let mut out = Vec::new();
        for (k, &i) in idx.iter().take(n).enumerate() {
            let t = self.table_infos(i).0;
            out.push((t, ["a", "b", "c"][k].to_string()));
        }
        out
    }

    /// The joined column list as expression inputs: each column is a
    /// qualified reference (`a.x`), or — when its bare name is unique
    /// across the join — sometimes a bare one (the dialect resolves those).
    fn join_cols(&mut self, items: &[(String, String)]) -> Vec<ColInfo> {
        let mut out: Vec<ColInfo> = Vec::new();
        for (t, alias) in items {
            for c in self.cols_of(t) {
                out.push(ColInfo {
                    name: format!("{alias}.{}", c.name),
                    ..c
                });
            }
        }
        // Bare references only where they cannot be ambiguous.
        let mut counts: HashMap<String, usize> = HashMap::new();
        for c in &out {
            *counts
                .entry(Self::bare_ref(&c.name).to_string())
                .or_insert(0) += 1;
        }
        for c in out.iter_mut() {
            let bare = Self::bare_ref(&c.name).to_string();
            if counts[&bare] == 1 && self.rng.chance(35) {
                c.name = bare;
            }
        }
        out
    }

    /// A link predicate between an earlier reference and a new one, over
    /// type-compatible columns (`a.x = b.y`). `None` when no pair shares a
    /// type: the caller then builds a cross product (comma style).
    fn link_pred(&mut self, left: &[ColInfo], right: &[ColInfo]) -> Option<String> {
        let ty = if self.rng.chance(50) {
            Ty::Integer
        } else {
            Ty::Text
        };
        let l: Vec<&ColInfo> = left.iter().filter(|c| c.ty == ty).collect();
        let r: Vec<&ColInfo> = right.iter().filter(|c| c.ty == ty).collect();
        if l.is_empty() || r.is_empty() {
            return None;
        }
        let a = l[self.rng.below(l.len() as u64) as usize];
        let b = r[self.rng.below(r.len() as u64) as usize];
        let op = *self
            .rng
            .pick(&["=", "=", "=", "=", "<", "<=", ">", ">=", "<>"]);
        Some(format!("{} {op} {}", a.name, b.name))
    }

    /// A join query (ticket 8): 2-3 `FROM` items, comma / `INNER JOIN` /
    /// `LEFT JOIN`, an `ON` link predicate per joined item, qualified
    /// column references and `SELECT *` over the whole join in FROM order.
    /// Every output column is in `ORDER BY`, so the frame is a sequence.
    pub fn join_select(&mut self) -> Statement {
        let items = self.join_items();
        let cols = self.join_cols(&items);

        // Style: the lite variant has no LEFT.
        enum Style {
            Comma,
            Inner,
            Left,
        }
        let style = if !self.profile.left_join {
            if self.rng.chance(50) {
                Style::Comma
            } else {
                Style::Inner
            }
        } else {
            match self.rng.below(100) {
                0..=34 => Style::Comma,
                35..=69 => Style::Inner,
                _ => Style::Left,
            }
        };

        // Per item: its column range in `cols`.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut at = 0usize;
        for (t, _) in &items {
            let n = self.cols_of(t).len();
            ranges.push((at, n));
            at += n;
        }

        let from_kw = self.kw("FROM");
        let mut from = format!("{from_kw} {} {}", items[0].0, items[0].1);
        let mut where_conds: Vec<String> = Vec::new();
        for i in 1..items.len() {
            let (start, len) = ranges[i];
            let right: Vec<ColInfo> = cols[start..start + len].to_vec();
            let left: Vec<ColInfo> = cols[..start].to_vec();
            let pred = self.link_pred(&left, &right);
            match (&style, pred) {
                (Style::Comma, Some(p)) => where_conds.push(p),
                (Style::Comma, None) => {}
                (Style::Inner, Some(p)) => {
                    let join = self.kw("INNER");
                    from.push_str(&format!(
                        " {join} {} {} {} {} {}",
                        self.kw("JOIN"),
                        items[i].0,
                        items[i].1,
                        self.kw("ON"),
                        p
                    ));
                }
                (Style::Left, Some(p)) => {
                    let left_kw = self.kw("LEFT");
                    let outer = if self.rng.chance(30) {
                        format!(" {}", self.kw("OUTER"))
                    } else {
                        String::new()
                    };
                    from.push_str(&format!(
                        " {left_kw}{outer} {} {} {} {} {}",
                        self.kw("JOIN"),
                        items[i].0,
                        items[i].1,
                        self.kw("ON"),
                        p
                    ));
                }
                // No type-compatible pair: a cross product needs no ON, so
                // fall back to the comma form for this item.
                (Style::Inner | Style::Left, None) => {
                    from.push_str(&format!(", {} {}", items[i].0, items[i].1));
                }
            }
        }

        // Output items.
        let mut parts = vec![self.kw("SELECT")];
        let distinct = self.rng.chance(20) && matches!(style, Style::Comma | Style::Inner);
        if distinct {
            parts.push(self.kw("DISTINCT"));
        }
        let order_terms: Vec<String>;
        if self.rng.chance(45) {
            // `SELECT *` over the join, in FROM order.
            parts.push("*".to_string());
            order_terms = cols.iter().map(|c| c.name.clone()).collect();
        } else {
            let n = 2 + self.rng.below(3) as usize;
            let mut rendered = Vec::new();
            order_terms = (0..n).map(|i| format!("e{i}")).collect();
            for (i, term) in order_terms.iter().enumerate() {
                let e = match self.rng.below(3) {
                    0 => self.text_expr(&cols, 2),
                    1 => self.bool_expr(&cols, 1),
                    _ => self.int_expr(&cols, 2),
                };
                let alias = self.kw("AS");
                rendered.push(format!("{} {alias} {term}", e.0));
                let _ = i;
            }
            parts.push(rendered.join(", "));
        }
        parts.push(from);
        if !where_conds.is_empty() {
            let extra = if self.rng.chance(30) {
                let e = self.bool_expr(&cols, 1);
                Some(e.0)
            } else {
                None
            };
            if let Some(e) = extra {
                where_conds.push(e);
            }
            let (w, and) = (self.kw("WHERE"), self.kw("AND"));
            parts.push(format!("{w} {}", where_conds.join(&format!(" {and} "))));
        }
        let mut terms = order_terms.clone();
        for i in (1..terms.len()).rev() {
            let j = self.rng.below(i as u64 + 1) as usize;
            terms.swap(i, j);
        }
        let rendered: Vec<String> = terms
            .iter()
            .map(|t| {
                let dir = match self.rng.below(10) {
                    0..=2 => String::new(),
                    3..=6 => format!(" {}", self.kw("ASC")),
                    _ => format!(" {}", self.kw("DESC")),
                };
                format!("{t}{dir}")
            })
            .collect();
        let (o, b) = (self.kw("ORDER"), self.kw("BY"));
        parts.push(format!("{o} {b} {}", rendered.join(", ")));
        if self.rng.chance(20) {
            parts.push(self.kw("LIMIT"));
            parts.push(self.rng.below(7).to_string());
        }
        let sql = self.join(parts);
        self.stmt(sql, Kind::Select, true)
    }

    pub fn update_stmt(&mut self) -> Statement {
        let ti = self.pick_table();
        let (tname, cols) = self.table_infos(ti);
        let targets: Vec<&ColInfo> = cols.iter().filter(|c| !c.pk).collect();
        let mut sets: Vec<String> = Vec::new();
        // The old-row semantics probe: `SET a = b, b = a` swaps.
        let same_ty = |t: Ty| targets.iter().filter(|c| c.ty == t).count() >= 2;
        let swap_ty = if same_ty(Ty::Integer) {
            Some(Ty::Integer)
        } else if same_ty(Ty::Text) {
            Some(Ty::Text)
        } else {
            None
        };
        if let (Some(t), true) = (swap_ty, self.rng.chance(20)) {
            let pair: Vec<&ColInfo> = targets.iter().copied().filter(|c| c.ty == t).collect();
            let (a, b) = (&pair[0].name, &pair[1].name);
            let both = self.bound_of(a).max(self.bound_of(b));
            self.bump(a, both);
            self.bump(b, both);
            sets.push(format!("{a} = {b}"));
            sets.push(format!("{b} = {a}"));
        } else {
            let n = 1 + self.rng.below(targets.len().min(2) as u64) as usize;
            let mut idx: Vec<usize> = (0..targets.len()).collect();
            for i in (1..idx.len()).rev() {
                let j = self.rng.below(i as u64 + 1) as usize;
                idx.swap(i, j);
            }
            for &i in idx.iter().take(n) {
                let c = targets[i];
                let rhs = if c.unique {
                    self.cell(c)
                } else {
                    let mut rhs: Ex = (String::new(), 0, 0);
                    for _ in 0..4 {
                        rhs = match c.ty {
                            Ty::Integer => self.int_expr(&cols, 2),
                            Ty::Text => self.text_expr(&cols, 2),
                        };
                        if Self::mentions_col(&rhs.0, &cols) {
                            break;
                        }
                    }
                    // The column may now hold anything the expression can.
                    self.bump(&c.name, rhs.2);
                    rhs.0
                };
                sets.push(format!("{} = {rhs}", c.name));
            }
        }
        let mut parts = vec![self.kw("UPDATE"), tname, self.kw("SET"), sets.join(", ")];
        if self.profile.where_clause && self.rng.chance(70) {
            parts.push(self.kw("WHERE"));
            parts.push(self.bool_expr(&cols, 2).0);
        }
        let sql = self.join(parts);
        self.stmt(sql, Kind::Update, false)
    }

    pub fn delete_stmt(&mut self) -> Statement {
        let ti = self.pick_table();
        let (tname, cols) = self.table_infos(ti);
        let mut parts = vec![self.kw("DELETE"), self.kw("FROM"), tname];
        if self.profile.where_clause && self.rng.chance(92) {
            parts.push(self.kw("WHERE"));
            parts.push(self.bool_expr(&cols, 2).0);
        }
        let sql = self.join(parts);
        self.stmt(sql, Kind::Delete, false)
    }

    /// A deliberate error. Each probe yields exactly one error class, never
    /// an ambiguity the dialect leaves undefined.
    pub fn probe(&mut self) -> Statement {
        let ti = self.pick_table();
        let (tname, cols) = self.table_infos(ti);
        // A plain INTEGER column the bad-type probe can target safely.
        let plain_int: Option<usize> = cols
            .iter()
            .position(|c| c.ty == Ty::Integer && !c.pk && !c.unique);
        let mut menu: Vec<u32> = vec![0, 1, 2, 3, 5, 5];
        if plain_int.is_some() {
            menu.push(4);
            // The multi-row atomicity probe only outside a transaction:
            // inside one SQLite keeps the rows a failed statement inserted
            // before the bad one, and the dialect leaves that untested.
            if self.profile.multi_row_insert && !self.txn_open {
                menu.push(9);
            }
        }
        if self.profile.update {
            menu.extend([6, 10]);
        }
        if self.profile.delete {
            menu.push(7);
        }
        if self.profile.where_clause {
            menu.push(8);
        }
        if self.profile.txn && !self.txn_open {
            // Outside a transaction these are `ERR txn`. Inside one a bare
            // COMMIT/ROLLBACK *succeeds* (it ends it), so it is not a
            // statement-level probe there.
            menu.extend([11, 12]);
        }
        if self.profile.aggregates {
            // Aggregate misuses: prepare-time rejections whose class the
            // spec pins (`syntax`).
            menu.extend([13, 14, 15, 16]);
        }
        if self.profile.joins && !cols.is_empty() {
            menu.extend([17, 18, 19]);
        }
        if self.profile.constraints {
            menu.extend([20, 21, 22]);
        }
        let which = *self.rng.pick(&menu);
        let sql = match which {
            0 => format!("{} * {} zz_gone", self.kw("SELECT"), self.kw("FROM")),
            1 => format!("{} zz_nocol {} {tname}", self.kw("SELECT"), self.kw("FROM")),
            2 => format!(
                "{} {} {tname} {} (1)",
                self.kw("INSERT"),
                self.kw("INTO"),
                self.kw("VALUES")
            ),
            3 => format!(
                "{} {} {tname} (zz INTEGER)",
                self.kw("CREATE"),
                self.kw("TABLE")
            ),
            4 | 9 => {
                // One bad cell (alphabetic text into an INTEGER column),
                // every other cell valid. Variant 9 hides the bad row
                // behind a good one: statement atomicity.
                let bad = plain_int.unwrap_or(0);
                let row = |g: &mut Self, bad_cell: bool| -> String {
                    let cells: Vec<String> = cols
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            if bad_cell && i == bad {
                                "'zz'".to_string()
                            } else if c.pk && c.ty == Ty::Integer {
                                "NULL".to_string()
                            } else {
                                g.cell(c)
                            }
                        })
                        .collect();
                    format!("({})", cells.join(", "))
                };
                let rows = if which == 9 {
                    let good = row(self, false);
                    let badr = row(self, true);
                    format!("{good}, {badr}")
                } else {
                    row(self, true)
                };
                format!(
                    "{} {} {tname} {} {rows}",
                    self.kw("INSERT"),
                    self.kw("INTO"),
                    self.kw("VALUES")
                )
            }
            5 => {
                let forms = [
                    format!("SELEC * FROM {tname}"),
                    format!("SELECT * FORM {tname}"),
                    "SELECT * FROM".to_string(),
                    format!("INSERT INTO {tname} VALUES (1,"),
                    "SELECT".to_string(),
                    "CREATE TABLE".to_string(),
                    format!("INSERT {tname} VALUES (1)"),
                    ")".to_string(),
                ];
                forms[self.rng.below(forms.len() as u64) as usize].clone()
            }
            6 => format!(
                "{} zz_gone {} {} = 1",
                self.kw("UPDATE"),
                self.kw("SET"),
                cols[0].name
            ),
            7 => format!("{} {} zz_gone", self.kw("DELETE"), self.kw("FROM")),
            8 => format!(
                "{} * {} {tname} {} zz_nocol = 1",
                self.kw("SELECT"),
                self.kw("FROM"),
                self.kw("WHERE")
            ),
            11 => self.kw("COMMIT"),
            12 => self.kw("ROLLBACK"),
            13 => format!(
                "{} {}(*) {} {tname} {} {} zz_nocol",
                self.kw("SELECT"),
                self.kw("COUNT"),
                self.kw("FROM"),
                self.kw("GROUP"),
                self.kw("BY")
            ),
            14 => format!(
                "{} {}(zz_gone) {} {tname}",
                self.kw("SELECT"),
                self.kw("SUM"),
                self.kw("FROM")
            ),
            15 => format!(
                "{} {}(*) {} {tname} {} {}(*) > 1",
                self.kw("SELECT"),
                self.kw("COUNT"),
                self.kw("FROM"),
                self.kw("WHERE"),
                self.kw("COUNT")
            ),
            16 => format!(
                "{} * {} {tname} {} {} {}(*)",
                self.kw("SELECT"),
                self.kw("FROM"),
                self.kw("GROUP"),
                self.kw("BY"),
                self.kw("COUNT")
            ),
            17 => {
                // A self-join makes every column name ambiguous
                // unqualified.
                let col = cols[self.rng.below(cols.len() as u64) as usize]
                    .name
                    .clone();
                format!(
                    "{} {col} {} {tname} a, {tname} b",
                    self.kw("SELECT"),
                    self.kw("FROM")
                )
            }
            18 => {
                let col = cols[self.rng.below(cols.len() as u64) as usize]
                    .name
                    .clone();
                format!("{} zz.{col} {} {tname}", self.kw("SELECT"), self.kw("FROM"))
            }
            19 => {
                let col = cols[self.rng.below(cols.len() as u64) as usize]
                    .name
                    .clone();
                format!(
                    "{} * {} {tname} a {} {} {tname} b {} a.zz_nocol = b.{col}",
                    self.kw("SELECT"),
                    self.kw("FROM"),
                    self.kw("INNER"),
                    self.kw("JOIN"),
                    self.kw("ON")
                )
            }
            20 => {
                // An explicit NULL into a NOT NULL column (no DEFAULT can
                // rescue it).
                let col = cols
                    .iter()
                    .find(|c| c.not_null && !c.pk)
                    .or_else(|| cols.iter().find(|c| !c.pk))
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| cols[0].name.clone());
                format!(
                    "{} {} {tname} ({col}) {} (NULL)",
                    self.kw("INSERT"),
                    self.kw("INTO"),
                    self.kw("VALUES")
                )
            }
            21 => {
                // A duplicate UNIQUE value: the second insert fails (the
                // FIRST may fail too when the value already exists — the
                // oracle decides).
                let mut candidate: Option<(String, String)> = None;
                for c in &cols {
                    if (c.unique || c.pk) && self.seen.get(&c.name).is_some_and(|v| !v.is_empty()) {
                        let key = self.seen[&c.name][0].clone();
                        candidate = Some((c.name.clone(), key));
                        break;
                    }
                }
                match candidate {
                    Some((col, val)) => format!(
                        "{} {} {tname} ({col}) {} ({val})",
                        self.kw("INSERT"),
                        self.kw("INTO"),
                        self.kw("VALUES")
                    ),
                    None => format!("{} zz_nocol {} {tname}", self.kw("SELECT"), self.kw("FROM")),
                }
            }
            _ => {
                // A foreign key with no parent row.
                let fk = cols.iter().find(|c| c.references.is_some()).cloned();
                match fk {
                    Some(c) => format!(
                        "{} {} {tname} ({}) {} (-4242424242424242)",
                        self.kw("INSERT"),
                        self.kw("INTO"),
                        c.name,
                        self.kw("VALUES")
                    ),
                    None => format!(
                        "{} {} {tname} ({}) {} (NULL)",
                        self.kw("INSERT"),
                        self.kw("INTO"),
                        cols[0].name,
                        self.kw("VALUES")
                    ),
                }
            }
        };
        self.stmt(sql, Kind::Probe, false)
    }

    /// The next statement of a mixed workload, weighted by the profile.
    pub fn next_statement(&mut self) -> Statement {
        if self.txn_open {
            return self.inside_transaction();
        }
        let w_insert = 24u64;
        let w_select = 40u64;
        let w_update = if self.profile.update { 14 } else { 0 };
        let w_delete = if self.profile.delete { 6 } else { 0 };
        let w_probe = 10u64;
        let w_txn = if self.profile.txn { 10 } else { 0 };
        let total = w_insert + w_select + w_update + w_delete + w_probe + w_txn;
        let mut roll = self.rng.below(total);
        if roll < w_insert {
            let ti = self.pick_table();
            return self.insert_into(ti);
        }
        roll -= w_insert;
        if roll < w_select {
            return self.select_stmt();
        }
        roll -= w_select;
        if roll < w_update {
            return self.update_stmt();
        }
        roll -= w_update;
        if roll < w_delete {
            return self.delete_stmt();
        }
        roll -= w_delete;
        if roll < w_probe {
            return self.probe();
        }
        self.txn_open = true;
        let sql = self.kw("BEGIN");
        self.stmt(sql, Kind::Txn, false)
    }

    /// One statement of an open transaction: the same mixed workload as
    /// outside it (SELECTs see the transaction's own writes), sometimes a
    /// committing close, sometimes a statement-level error probe or a
    /// nested `BEGIN` — both must leave the transaction running, as SQLite
    /// does.
    fn inside_transaction(&mut self) -> Statement {
        let w_insert = 20u64;
        let w_select = 30u64;
        let w_update = if self.profile.update { 10 } else { 0 };
        let w_delete = if self.profile.delete { 6 } else { 0 };
        let w_close = 20u64;
        let w_probe = 9u64;
        let w_begin = 5u64;
        let total = w_insert + w_select + w_update + w_delete + w_close + w_probe + w_begin;
        let mut roll = self.rng.below(total);
        if roll < w_close {
            return self.close_transaction_now();
        }
        roll -= w_close;
        if roll < w_begin {
            let sql = self.kw("BEGIN");
            return self.stmt(sql, Kind::Probe, false);
        }
        roll -= w_begin;
        if roll < w_probe {
            return self.probe();
        }
        roll -= w_probe;
        if roll < w_insert {
            let ti = self.pick_table();
            return self.insert_into(ti);
        }
        roll -= w_insert;
        if roll < w_select {
            return self.select_stmt();
        }
        roll -= w_select;
        if roll < w_update {
            return self.update_stmt();
        }
        roll -= w_update;
        if roll < w_delete {
            return self.delete_stmt();
        }
        let ti = self.pick_table();
        self.insert_into(ti)
    }

    /// A statement that ends an open transaction (`COMMIT` mostly, sometimes
    /// `ROLLBACK`).
    fn close_transaction_now(&mut self) -> Statement {
        self.txn_open = false;
        let kw = if self.rng.chance(70) {
            "COMMIT"
        } else {
            "ROLLBACK"
        };
        let sql = self.kw(kw);
        self.stmt(sql, Kind::Txn, false)
    }

    /// Close an open transaction, if any, so a script never ends inside one.
    pub fn close_transaction(&mut self) -> Option<Statement> {
        if self.txn_open {
            Some(self.close_transaction_now())
        } else {
            None
        }
    }
}

/// A statement with the oracle's answer, ready to replay at a candidate.
#[derive(Debug, Clone)]
pub struct Checked {
    pub stmt: Statement,
    pub expected: OracleOutcome,
}

/// A full verification script for one seed.
#[derive(Debug, Clone)]
pub struct Script {
    pub seed: u64,
    pub profile: Profile,
    /// `CREATE TABLE`s plus two seed inserts per table.
    pub setup: Vec<Checked>,
    pub steps: Vec<Checked>,
    /// Statements the oracle discarded (outside the dialect); never sent.
    pub discarded: usize,
    /// Oracle messages with no class mapping: a verifier bug, never graded.
    pub unknown: Vec<String>,
}

/// Build a script: generate statements and run each on a fresh in-process
/// oracle so the expected answers are fixed before the candidate is asked.
pub fn build_script(seed: u64, profile: &Profile, steps: usize) -> Script {
    let mut srng = Rng::new(seed ^ 0x5C4E_3A00_0000_0002);
    let table_count = 3 + srng.below(3) as usize;
    let schema = schema::generate(seed, table_count, 5);
    let mut gen = Generator::new(seed, schema, profile.clone());
    let oracle = Oracle::new().expect("in-memory oracle");

    let mut script = Script {
        seed,
        profile: profile.clone(),
        setup: Vec::new(),
        steps: Vec::new(),
        discarded: 0,
        unknown: Vec::new(),
    };

    fn run(script: &mut Script, oracle: &Oracle, stmt: Statement, into_setup: bool) {
        let expected = oracle.exec(&stmt.sql);
        match expected {
            OracleOutcome::Discard(_) => script.discarded += 1,
            OracleOutcome::Unknown(ref m) => script.unknown.push(format!("{}: {m}", stmt.sql)),
            _ => {
                let c = Checked { stmt, expected };
                if into_setup {
                    script.setup.push(c);
                } else {
                    script.steps.push(c);
                }
            }
        }
    }

    for sql in gen.setup_statements() {
        let stmt = Statement {
            sql,
            kind: Kind::Create,
            ordered: false,
        };
        run(&mut script, &oracle, stmt, true);
    }
    for ti in 0..gen.schema().tables.len() {
        for _ in 0..2 {
            let stmt = gen.insert_into(ti);
            run(&mut script, &oracle, stmt, true);
        }
    }
    let mut attempts = 0usize;
    while script.steps.len() < steps && attempts < steps * 4 {
        attempts += 1;
        let stmt = gen.next_statement();
        run(&mut script, &oracle, stmt, false);
    }
    // A script never ends inside an open transaction: the candidate's
    // connection stays up for later checks (and the oracle's too).
    while let Some(stmt) = gen.close_transaction() {
        run(&mut script, &oracle, stmt, false);
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verifier::sql::oracle::Value;
    use std::collections::BTreeSet;

    fn lower(s: &str) -> String {
        s.to_ascii_lowercase()
    }

    #[test]
    fn profiles_are_cumulative() {
        let p = Profile::through(3);
        assert!(p.multi_row_insert && p.column_list_insert && p.where_clause);
        assert!(p.expressions && p.select_no_from && p.order_limit && p.distinct);
        assert!(p.update && p.delete && !p.constraints);
        assert_eq!(Profile::through(0), Profile::stage(0));
        assert_eq!(
            Profile::for_passed(&[0, 3]),
            Profile::stage(0).union(&Profile::stage(3))
        );
        assert!(Profile::through(9).constraints);
    }

    #[test]
    fn scripts_are_deterministic_per_seed() {
        let p = Profile::through(3);
        let a = build_script(11, &p, 200);
        let b = build_script(11, &p, 200);
        let sql = |s: &Script| -> Vec<String> {
            s.setup
                .iter()
                .chain(&s.steps)
                .map(|c| c.stmt.sql.clone())
                .collect()
        };
        assert_eq!(sql(&a), sql(&b));
        assert_ne!(sql(&a), sql(&build_script(12, &p, 200)));
    }

    /// Every profile's statements run on the oracle with no unmapped
    /// messages, and most are gradeable (the comparable-check floor).
    #[test]
    fn every_profile_is_oracle_clean_and_mostly_comparable() {
        for last in [0u32, 1, 2, 3, 7, 9] {
            let p = Profile::through(last);
            let (mut total, mut kept) = (0usize, 0usize);
            for seed in 0..40u64 {
                let s = build_script(seed, &p, 150);
                assert!(
                    s.unknown.is_empty(),
                    "stage {last} seed {seed}: unmapped oracle messages: {:?}",
                    &s.unknown[..s.unknown.len().min(3)]
                );
                kept += s.steps.len();
                total += s.steps.len() + s.discarded;
            }
            let ratio = kept as f64 / total as f64;
            assert!(
                ratio >= 0.9,
                "stage {last}: only {:.1}% of generated statements are gradeable",
                ratio * 100.0
            );
        }
    }

    #[test]
    fn profile_gates_the_grammar() {
        // Stage 0 alone: inserts, plain selects and probes only.
        let p0 = Profile::through(0);
        let mut seen = BTreeSet::new();
        for seed in 0..30u64 {
            let s = build_script(seed, &p0, 120);
            for c in s.setup.iter().chain(&s.steps) {
                let q = lower(&c.stmt.sql);
                seen.insert(c.stmt.kind);
                if c.stmt.kind != Kind::Probe {
                    for banned in [" where ", "order", "limit", "distinct", "update", "delete"] {
                        assert!(!q.contains(banned), "stage 0 leaked {banned:?}: {q}");
                    }
                    assert!(!q.contains("primary key") && !q.contains("default"), "{q}");
                }
                assert!(!matches!(c.stmt.kind, Kind::Update | Kind::Delete), "{q}");
            }
        }
        assert!(seen.contains(&Kind::Insert) && seen.contains(&Kind::Select));

        // Stage 3 reaches everything through UPDATE/DELETE.
        let p3 = Profile::through(3);
        let mut kinds = BTreeSet::new();
        let mut text = String::new();
        for seed in 0..30u64 {
            let s = build_script(seed, &p3, 150);
            for c in &s.steps {
                kinds.insert(format!("{:?}", c.stmt.kind));
                text.push_str(&lower(&c.stmt.sql));
                text.push('\n');
            }
        }
        for k in ["Insert", "Select", "Update", "Delete", "Probe"] {
            assert!(kinds.contains(k), "missing {k}");
        }
        for feature in [
            "where",
            "order",
            "limit",
            "offset",
            "distinct",
            "values",
            "coalesce",
            "upper",
            "length",
            "abs",
            " is null",
            "is not null",
            " and ",
            " or ",
            "not (",
        ] {
            assert!(text.contains(feature), "stage 3 never produced {feature:?}");
        }

        // Aggregates are stage 7 only.
        let p6 = Profile::through(6);
        for seed in 0..30u64 {
            let s = build_script(seed, &p6, 150);
            for c in s.setup.iter().chain(&s.steps) {
                let q = lower(&c.stmt.sql);
                for banned in ["count(", "sum(", "min(", "max(", "group by", "having"] {
                    assert!(!q.contains(banned), "stage 6 leaked {banned:?}: {q}");
                }
            }
        }
        let p7 = Profile::through(7);
        let mut text7 = String::new();
        for seed in 0..40u64 {
            let s = build_script(seed, &p7, 150);
            assert!(
                s.unknown.is_empty(),
                "{:?}",
                &s.unknown[..s.unknown.len().min(2)]
            );
            for c in &s.steps {
                text7.push_str(&lower(&c.stmt.sql));
                text7.push('\n');
            }
        }
        for feature in [
            "count(*)",
            "count(",
            "sum(",
            "min(",
            "max(",
            "group by",
            "having",
            "order by k",
        ] {
            assert!(
                text7.contains(feature),
                "stage 7 never produced {feature:?}"
            );
        }
        // Every grouped query is totally ordered by its key and never puts
        // a bare column in the list: the dialect leaves those untested.
        for seed in 0..40u64 {
            let s = build_script(seed, &p7, 150);
            for c in &s.steps {
                let q = lower(&c.stmt.sql);
                if c.stmt.kind == Kind::Select && q.contains("group by") {
                    assert!(c.stmt.ordered, "grouped query without ORDER BY: {q}");
                    assert!(q.contains("order by k"), "grouped query not keyed: {q}");
                    assert!(
                        !q.contains("select *") && q.contains(" as k"),
                        "grouped query leaks a bare star/column: {q}"
                    );
                }
                if c.stmt.kind == Kind::Select && q.contains("count(") && q.contains("from") {
                    // Every aggregate select item is aliased.
                    assert!(
                        q.contains(" as c0") || q.contains(" as c1") || q.contains(" as c2"),
                        "aggregate item without alias: {q}"
                    );
                }
            }
        }
    }

    /// Ticket 9's constraint shapes: the constrained DDL (PRIMARY KEY,
    /// UNIQUE, NOT NULL, DEFAULT, REFERENCES) appears only from stage 9,
    /// the violations are probed, and omitted-but-defaulted columns are
    /// exercised.
    #[test]
    fn constraint_profile_emits_ddl_and_violation_probes() {
        let p9 = Profile::through(9);
        let mut ddl = String::new();
        let mut probes = String::new();
        let (mut auto_pk, mut omitted_default) = (0, 0);
        for seed in 0..60u64 {
            let s = build_script(seed, &p9, 150);
            assert!(
                s.unknown.is_empty(),
                "{:?}",
                &s.unknown[..s.unknown.len().min(2)]
            );
            assert_eq!(s.discarded, 0, "seed {seed}: discards");
            for c in s.setup.iter().chain(&s.steps) {
                let q = lower(&c.stmt.sql);
                if c.stmt.kind == Kind::Create {
                    ddl.push_str(&q);
                    ddl.push('\n');
                }
                if c.stmt.kind == Kind::Probe {
                    probes.push_str(&q);
                    probes.push('\n');
                }
                if q.contains("insert") && q.contains("values (null") {
                    auto_pk += 1;
                }
                // A column-list INSERT omits at least one column, so the
                // DEFAULT path is exercised.
                if q.contains("insert into") && q.contains(" (") && q.contains(" values ") {
                    omitted_default += 1;
                }
            }
        }
        for feature in [
            "primary key",
            "unique",
            "not null",
            "default ",
            "references ",
        ] {
            assert!(ddl.contains(feature), "stage 9 never emitted {feature:?}");
        }
        assert!(auto_pk >= 5, "NULL/omitted primary keys rare: {auto_pk}");
        assert!(
            omitted_default >= 20,
            "column-list inserts rare: {omitted_default}"
        );
        // The violation probes are present (the class comes from the oracle).
        for needle in ["(null)", "-4242424242424242"] {
            assert!(
                probes.contains(needle),
                "constraint probes missing {needle:?}"
            );
        }
        // Stage 8 must not leak any constraint (predicates may still say
        // `IS NOT NULL`: that is stage 1, not DDL).
        let p8 = Profile::through(8);
        for seed in 0..30u64 {
            let s = build_script(seed, &p8, 150);
            for c in s.setup.iter().chain(&s.steps) {
                let q = lower(&c.stmt.sql);
                if c.stmt.kind == Kind::Create {
                    for banned in [
                        "primary key",
                        "unique",
                        "not null",
                        " default ",
                        "references ",
                    ] {
                        assert!(!q.contains(banned), "stage 8 leaked {banned:?}: {q}");
                    }
                } else {
                    for banned in ["default ", "references "] {
                        assert!(!q.contains(banned), "stage 8 leaked {banned:?}: {q}");
                    }
                }
            }
        }
    }

    /// Ticket 8's join shapes: comma/`INNER`/`LEFT` joins over 2-3 items,
    /// qualified references, `SELECT *` in FROM order, an `ON` per joined
    /// item, a 3-way join, and the ambiguity probe; stage 7 must not leak
    /// any of it, and the lite variant drops `LEFT`.
    #[test]
    fn join_profile_emits_joins_and_lite_drops_left() {
        fn is_join(q: &str) -> bool {
            if q.contains(" join ") {
                return true;
            }
            let Some(rest) = q.split(" from ").nth(1) else {
                return false;
            };
            let mut head = rest;
            for cut in [" where ", " order ", " group ", " having ", " limit "] {
                head = head.split(cut).next().unwrap_or(head);
            }
            head.contains(", ")
        }

        let p8 = Profile::through(8);
        let (mut joins, mut left, mut star, mut on, mut three_way) = (0, 0, 0, 0, 0);
        let mut probes = String::new();
        for seed in 0..80u64 {
            let s = build_script(seed, &p8, 150);
            assert!(
                s.unknown.is_empty(),
                "{:?}",
                &s.unknown[..s.unknown.len().min(2)]
            );
            for c in &s.steps {
                // Tabs separate tokens too; the join heuristic wants spaces.
                let q = lower(&c.stmt.sql)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if c.stmt.kind == Kind::Probe {
                    probes.push_str(&q);
                    probes.push('\n');
                    continue;
                }
                if c.stmt.kind != Kind::Select {
                    continue;
                }
                if is_join(&q) {
                    joins += 1;
                    if q.contains("left join") || q.contains("left outer join") {
                        left += 1;
                        assert!(q.contains(" on "), "LEFT JOIN without ON: {q}");
                    }
                    if q.contains(" join ") {
                        on += 1;
                    }
                    if q.starts_with("select *") {
                        star += 1;
                    }
                    if q.contains(", ") && q.contains(" join ") {
                        three_way += 1;
                    }
                } else {
                    assert!(!q.contains(" join "), "stage-8 join leaked: {q}");
                }
            }
        }
        assert!(joins >= 40, "too few join queries: {joins}");
        assert!(left >= 5, "LEFT JOIN rarely generated: {left}");
        assert!(on >= 10, "ON predicates rarely generated: {on}");
        assert!(star >= 5, "SELECT * over a join rarely generated: {star}");
        assert!(
            three_way >= 3,
            "mixed 3-way joins rarely generated: {three_way}"
        );
        for form in ["zz.", " on a.zz_nocol", " from", ", "] {
            assert!(probes.contains(form), "join probes missing {form:?}");
        }

        // Stage 7 scripts have no joins at all.
        let p7 = Profile::through(7);
        for seed in 0..30u64 {
            let s = build_script(seed, &p7, 150);
            for c in s.setup.iter().chain(&s.steps) {
                let q = lower(&c.stmt.sql)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(!q.contains(" join "), "stage 7 leaked a join: {q}");
                if c.stmt.kind == Kind::Select {
                    assert!(!is_join(&q), "stage 7 leaked a join: {q}");
                }
            }
        }

        // The lite variant (join-inner-only) keeps comma/inner joins and
        // never emits LEFT.
        let mut lite = Profile::through(8);
        lite.narrow(&["join-inner-only".to_string()]);
        let (mut lite_joins, mut lite_left) = (0, 0);
        for seed in 0..60u64 {
            let s = build_script(seed, &lite, 150);
            for c in &s.steps {
                let q = lower(&c.stmt.sql)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if c.stmt.kind == Kind::Select && is_join(&q) {
                    lite_joins += 1;
                    lite_left += usize::from(q.contains("left join"));
                }
            }
        }
        assert!(lite_joins >= 30, "lite joins vanished: {lite_joins}");
        assert_eq!(lite_left, 0, "lite variant emitted LEFT JOIN");
    }

    /// Ticket 7's arithmetic promises: SUM of no rows is NULL and COUNT is
    /// 0 (they must really be exercised), SUM is only generated where the
    /// worst case fits in 64 bits (checked by the overflow gates), and every
    /// aggregate query is gradeable as a frame.
    #[test]
    fn aggregate_queries_grade_and_exercise_the_empty_group() {
        let p = Profile::through(7);
        let (mut sum_seen, mut empty_scalar) = (0usize, 0usize);
        for seed in 0..120u64 {
            let s = build_script(seed, &p, 150);
            for c in &s.steps {
                let q = lower(&c.stmt.sql);
                if c.stmt.kind != Kind::Select {
                    continue;
                }
                if !(q.contains("count(")
                    || q.contains("sum(")
                    || q.contains("min(")
                    || q.contains("max("))
                {
                    continue;
                }
                // The oracle must produce a frame (never an error) and the
                // statement must not be a discard (no REAL results).
                match &c.expected {
                    OracleOutcome::Rows { rows, .. } => {
                        if !q.contains("group by") && rows.len() != 1 {
                            panic!(
                                "a scalar aggregate must return one row: {q} -> {:?}",
                                rows.len()
                            );
                        }
                        if !q.contains("group by") && rows.len() == 1 {
                            let empty = rows[0].iter().any(|v| matches!(v, Value::Null))
                                || rows[0].iter().any(|v| matches!(v, Value::Int(0)));
                            if empty {
                                empty_scalar += 1;
                            }
                        }
                        if q.contains("sum(") {
                            sum_seen += 1;
                        }
                    }
                    other => panic!("aggregate query expected {other:?}: {q}"),
                }
            }
        }
        assert!(sum_seen >= 10, "SUM rarely generated: {sum_seen}");
        assert!(
            empty_scalar >= 5,
            "COUNT 0 / SUM NULL over no matching rows never exercised: {empty_scalar}"
        );
    }

    #[test]
    fn ordered_selects_list_every_output_column_and_limit_needs_order() {
        let p = Profile::through(3);
        let mut ordered_seen = 0;
        for seed in 0..40u64 {
            let s = build_script(seed, &p, 150);
            for c in &s.steps {
                let q = lower(&c.stmt.sql);
                if c.stmt.kind == Kind::Select {
                    if q.contains("limit") || q.contains("offset") {
                        assert!(c.stmt.ordered, "LIMIT without ORDER BY: {q}");
                    }
                    if c.stmt.ordered {
                        ordered_seen += 1;
                        // The expected result must be gradeable as a sequence.
                        assert!(matches!(c.expected, OracleOutcome::Rows { .. }), "{q}");
                    }
                }
            }
        }
        assert!(ordered_seen > 20, "too few ordered selects: {ordered_seen}");
    }

    #[test]
    fn error_probes_cover_the_dialect_classes() {
        use crate::verifier::sql::oracle::ErrClass;
        let p = Profile::through(3);
        let mut classes = BTreeSet::new();
        for seed in 0..60u64 {
            let s = build_script(seed, &p, 200);
            for c in &s.steps {
                if let OracleOutcome::Error(e) = c.expected {
                    classes.insert(e.token());
                }
            }
        }
        for want in [
            ErrClass::Syntax,
            ErrClass::NoTable,
            ErrClass::NoColumn,
            ErrClass::Exists,
            ErrClass::Arity,
            ErrClass::Type,
        ] {
            assert!(classes.contains(want.token()), "no {} probe", want.token());
        }
        // Transactions add nested-BEGIN and bare-COMMIT probes.
        let p6 = Profile::through(6);
        let mut txn = BTreeSet::new();
        for seed in 0..60u64 {
            let s = build_script(seed, &p6, 200);
            for c in &s.steps {
                if let OracleOutcome::Error(e) = c.expected {
                    txn.insert(e.token());
                }
            }
        }
        assert!(txn.contains(ErrClass::Txn.token()), "no txn probe");
    }

    /// Transactions are balanced by construction (`Kind::Txn` begins equal
    /// closes), so a script never ends inside one, and every script of the
    /// stage-6 profile contains at least one `BEGIN` with a `COMMIT` or
    /// `ROLLBACK` (nested-BEGIN probes are `Kind::Probe` and do not open a
    /// transaction).
    #[test]
    fn transaction_scripts_are_balanced() {
        let p = Profile::through(6);
        let (mut scripts_with_txn, mut total_scripts) = (0usize, 0usize);
        for seed in 0..40u64 {
            let s = build_script(seed, &p, 150);
            let stmts = || s.setup.iter().chain(&s.steps);
            let opens = stmts()
                .filter(|c| c.stmt.kind == Kind::Txn && c.stmt.sql.to_lowercase().contains("begin"))
                .count();
            let closes = stmts()
                .filter(|c| {
                    c.stmt.kind == Kind::Txn
                        && (c.stmt.sql.to_lowercase().contains("commit")
                            || c.stmt.sql.to_lowercase().contains("rollback"))
                })
                .count();
            assert_eq!(opens, closes, "seed {seed}: {opens} BEGIN, {closes} closes");
            // The closing statements answer OK 0.
            for c in stmts().filter(|c| c.stmt.kind == Kind::Txn) {
                assert_eq!(c.expected, OracleOutcome::Affected(0), "{:?}", c.stmt.sql);
            }
            total_scripts += 1;
            scripts_with_txn += usize::from(opens > 0);
        }
        assert!(
            scripts_with_txn * 4 >= total_scripts * 3,
            "only {scripts_with_txn}/{total_scripts} scripts ran a transaction"
        );

        // Inside transactions the workload is mixed, not DML-only: SELECTs
        // must run (they see the transaction's own uncommitted writes) and
        // a statement-level error must be able to happen without killing
        // the transaction (a nested BEGIN, a missing column, a type
        // mismatch).
        let (mut tx_selects, mut tx_errors) = (0usize, 0usize);
        for seed in 100..160u64 {
            let s = build_script(seed, &p, 150);
            let mut open = false;
            for c in s.setup.iter().chain(&s.steps) {
                let sql = c.stmt.sql.to_lowercase();
                if c.stmt.kind == Kind::Txn && sql.contains("begin") {
                    open = true;
                    continue;
                }
                if !open {
                    continue;
                }
                if c.stmt.kind == Kind::Txn && (sql.contains("commit") || sql.contains("rollback"))
                {
                    open = false;
                    continue;
                }
                if sql.starts_with("select") {
                    tx_selects += 1;
                }
                if matches!(c.expected, OracleOutcome::Error(_)) {
                    tx_errors += 1;
                }
            }
            assert!(!open, "seed {seed}: a script ended inside a transaction");
        }
        assert!(
            tx_selects >= 20,
            "no SELECTs inside transactions: {tx_selects}"
        );
        assert!(
            tx_errors >= 5,
            "no error-class statements inside transactions: {tx_errors}"
        );
    }

    #[test]
    fn text_never_starts_with_end_and_keywords_vary() {
        let p = Profile::through(3);
        let (mut upper, mut lowered, mut tabs, mut semis) = (false, false, false, false);
        for seed in 0..30u64 {
            let s = build_script(seed, &p, 100);
            for c in s.setup.iter().chain(&s.steps) {
                let q = &c.stmt.sql;
                assert!(!q.contains("'END"), "text literal starts with END: {q}");
                upper |= q.contains("SELECT");
                lowered |= q.contains("select");
                tabs |= q.contains('\t');
                semis |= q.ends_with(';');
                if let OracleOutcome::Rows { rows, .. } = &c.expected {
                    for r in rows {
                        for v in r {
                            if let crate::verifier::sql::oracle::Value::Text(t) = v {
                                assert!(!t.starts_with("END"), "{t:?}");
                            }
                        }
                    }
                }
            }
        }
        assert!(upper && lowered && tabs && semis, "surface not randomized");
    }

    #[test]
    fn swap_update_and_atomicity_probes_appear() {
        let p = Profile::through(3);
        let (mut swap, mut atomic) = (false, false);
        for seed in 0..80u64 {
            let s = build_script(seed, &p, 200);
            for c in &s.steps {
                let q = lower(&c.stmt.sql);
                if c.stmt.kind == Kind::Update {
                    let after = q.split(" set ").nth(1).unwrap_or("");
                    // `a = b, b = a`
                    let sets: Vec<&str> =
                        after.split(" where ").next().unwrap().split(", ").collect();
                    if sets.len() == 2 {
                        let l: Vec<&str> = sets[0].split(" = ").collect();
                        let r: Vec<&str> = sets[1].split(" = ").collect();
                        if l.len() == 2
                            && r.len() == 2
                            && l[0] == r[1].trim_end_matches(';')
                            && l[1] == r[0]
                        {
                            swap = true;
                        }
                    }
                }
                if c.stmt.kind == Kind::Probe
                    && q.contains("'zz'")
                    && q.matches("), (").count() == 1
                {
                    atomic = true;
                }
            }
        }
        assert!(swap, "no swap UPDATE in 80 seeds");
        assert!(atomic, "no multi-row atomicity probe in 80 seeds");
    }

    /// A failed multi-row INSERT changes nothing (the oracle's own rule that
    /// the atomicity probe relies on).
    #[test]
    fn atomicity_probe_leaves_no_rows() {
        let o = Oracle::new().unwrap();
        o.exec("CREATE TABLE t (a INTEGER, b TEXT)");
        assert_eq!(
            o.exec("INSERT INTO t VALUES (1, 'x'), ('zz', 'y')"),
            OracleOutcome::Error(crate::verifier::sql::oracle::ErrClass::Type)
        );
        match o.exec("SELECT COUNT(*) FROM t") {
            OracleOutcome::Rows { rows, .. } => {
                assert_eq!(rows[0][0], crate::verifier::sql::oracle::Value::Int(0))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn constraint_profile_emits_keys_and_respects_them() {
        let p = Profile::through(9);
        let s = build_script(5, &p, 200);
        let ddl: String = s
            .setup
            .iter()
            .filter(|c| c.stmt.kind == Kind::Create)
            .map(|c| lower(&c.stmt.sql))
            .collect();
        assert!(ddl.contains("primary key"), "{ddl}");
        // Unique keys are generated fresh, so constraint errors stay a small
        // minority of the workload.
        let errs = s
            .steps
            .iter()
            .filter(|c| {
                matches!(
                    c.expected,
                    OracleOutcome::Error(crate::verifier::sql::oracle::ErrClass::Constraint)
                )
            })
            .count();
        assert!(
            errs * 4 < s.steps.len(),
            "{errs} of {} are constraint errors",
            s.steps.len()
        );
    }

    /// The comparable-check floor for query quality: filters must actually
    /// select rows, and UPDATE/DELETE must usually touch something, or a
    /// candidate that ignores WHERE would pass.
    #[test]
    fn predicates_select_rows_and_dml_touches_rows() {
        let p = Profile::through(3);
        let (mut where_sel, mut where_hit) = (0usize, 0usize);
        let (mut dml, mut dml_hit) = (0usize, 0usize);
        for seed in 0..60u64 {
            let s = build_script(seed, &p, 150);
            for c in &s.steps {
                let q = lower(&c.stmt.sql);
                match (&c.stmt.kind, &c.expected) {
                    (Kind::Select, OracleOutcome::Rows { rows, .. }) if q.contains(" where ") => {
                        where_sel += 1;
                        where_hit += usize::from(!rows.is_empty());
                    }
                    (Kind::Update | Kind::Delete, OracleOutcome::Affected(n)) => {
                        dml += 1;
                        dml_hit += usize::from(*n > 0);
                    }
                    _ => {}
                }
            }
        }
        assert!(where_sel > 500 && dml > 300, "{where_sel} / {dml}");
        assert!(
            where_hit * 100 >= where_sel * 50,
            "only {where_hit}/{where_sel} filtered SELECTs return rows"
        );
        assert!(
            dml_hit * 100 >= dml * 60,
            "only {dml_hit}/{dml} UPDATE/DELETE touch a row"
        );
    }

    /// WHERE clauses must read a column; a constant predicate proves nothing.
    #[test]
    fn where_clauses_read_columns() {
        let p = Profile::through(3);
        let (mut total, mut constant) = (0usize, 0usize);
        for seed in 0..60u64 {
            let s = build_script(seed, &p, 150);
            let names: Vec<ColInfo> = {
                let g = Generator::new(seed, s_schema(seed), p.clone());
                (0..g.schema.tables.len())
                    .flat_map(|i| g.table_infos(i).1)
                    .collect()
            };
            for c in &s.steps {
                if !matches!(c.stmt.kind, Kind::Select | Kind::Update | Kind::Delete) {
                    continue;
                }
                let q = lower(&c.stmt.sql);
                if let Some(w) = q.split(" where ").nth(1) {
                    total += 1;
                    if !Generator::mentions_col(w, &names) {
                        constant += 1;
                    }
                }
            }
        }
        assert!(total > 500);
        assert!(
            constant * 100 <= total * 3,
            "{constant}/{total} constant WHEREs"
        );
    }

    /// The schema `build_script` derives for a seed (test helper).
    fn s_schema(seed: u64) -> Schema {
        let mut srng = Rng::new(seed ^ 0x5C4E_3A00_0000_0002);
        let table_count = 3 + srng.below(3) as usize;
        schema::generate(seed, table_count, 5)
    }

    /// Dropped parentheses must never change the parse tree the bounds were
    /// computed for (the 5 * (num * NULL) -> `5 * num * NULL` overflow).
    #[test]
    fn bare_operands_never_reassociate() {
        let mut g = Generator::new(1, schema::generate(1, 3, 4), Profile::through(3));
        // A looser child always keeps its parentheses.
        assert_eq!(g.wrap_arith(("a + b".into(), 3, 0), 2, false, 0), "(a + b)");
        // So does a right operand of the same level.
        assert_eq!(g.wrap_arith(("b - c".into(), 3, 0), 3, true, 0), "(b - c)");
        // A left operand of the same level, or a tighter child, may go bare.
        assert_eq!(g.wrap_arith(("a - b".into(), 3, 0), 3, false, 0), "a - b");
        assert_eq!(g.wrap_arith(("b * c".into(), 2, 0), 3, true, 0), "b * c");
        // Atoms are never wrapped.
        assert_eq!(g.wrap_arith(("x".into(), 0, 0), 3, true, 100), "x");
    }

    /// Bounds keep the workload overflow-free, including columns that hold
    /// i64::MAX (the `INT_BOUND` boundary): no script may produce an
    /// overflow discard, and no expression may be built past the bound.
    /// Stage 9 is included because that is where `DEFAULT`s exist — a
    /// defaulted column the generator never writes explicitly still holds
    /// its default value.
    #[test]
    fn workload_never_overflows() {
        let mut with_max = 0;
        for (last, seeds) in [(3u32, 400u64), (9, 200)] {
            let p = Profile::through(last);
            for seed in 0..seeds {
                let s = build_script(seed, &p, 150);
                assert_eq!(
                    s.discarded, 0,
                    "stage {last} seed {seed}: overflow discards"
                );
                assert!(s.unknown.is_empty());
                with_max += usize::from(
                    s.setup
                        .iter()
                        .chain(&s.steps)
                        .any(|c| c.stmt.sql.contains("9223372036854775807")),
                );
            }
        }
        assert!(with_max > 5, "i64::MAX never exercised ({with_max})");
    }

    /// A column's bound starts at its `DEFAULT`: a column that is never
    /// explicitly written still holds the default value.
    #[test]
    fn defaults_seed_the_column_bounds() {
        use crate::verifier::sql::schema::{Column, DefaultVal};
        let schema = Schema {
            tables: vec![Table {
                name: "t".into(),
                columns: vec![
                    Column {
                        name: "id".into(),
                        ty: Ty::Integer,
                        primary_key: true,
                        unique: false,
                        not_null: false,
                        default: None,
                        references: None,
                    },
                    Column {
                        name: "n".into(),
                        ty: Ty::Integer,
                        primary_key: false,
                        unique: false,
                        not_null: false,
                        default: Some(DefaultVal::Int(-9)),
                        references: None,
                    },
                    Column {
                        name: "s".into(),
                        ty: Ty::Text,
                        primary_key: false,
                        unique: false,
                        not_null: false,
                        default: Some(DefaultVal::Text("d123".into())),
                        references: None,
                    },
                ],
            }],
        };
        let g = Generator::new(1, schema, Profile::through(9));
        assert_eq!(g.bound_of("n"), 9, "|DEFAULT -9| must seed the int bound");
        assert_eq!(
            g.bound_of("s"),
            4,
            "the DEFAULT text's length must seed the text bound"
        );
    }

    /// The bound must cover a `DEFAULT`: with a hand-built schema whose
    /// default is `i64::MAX`, a stage-9 script generated before the bound
    /// was seeded produced overflow discards (`n + 72 * id`).
    #[test]
    fn large_defaults_do_not_overflow() {
        use crate::verifier::sql::schema::{Column, DefaultVal};
        let schema = Schema {
            tables: vec![Table {
                name: "t".into(),
                columns: vec![
                    Column {
                        name: "id".into(),
                        ty: Ty::Integer,
                        primary_key: true,
                        unique: false,
                        not_null: false,
                        default: None,
                        references: None,
                    },
                    Column {
                        name: "n".into(),
                        ty: Ty::Integer,
                        primary_key: false,
                        unique: false,
                        not_null: false,
                        default: Some(DefaultVal::Int(i64::MAX)),
                        references: None,
                    },
                ],
            }],
        };
        for seed in 0..16u64 {
            let mut g = Generator::new(seed, schema.clone(), Profile::through(9));
            let oracle = Oracle::new().unwrap();
            let mut stmts: Vec<Statement> = g
                .setup_statements()
                .into_iter()
                .map(|sql| Statement {
                    sql,
                    kind: Kind::Create,
                    ordered: false,
                })
                .collect();
            for _ in 0..80 {
                stmts.push(g.next_statement());
            }
            for st in &stmts {
                let got = oracle.exec(&st.sql);
                assert!(
                    !got.is_discard(),
                    "seed {seed}: overflow on {:?}: {got:?}",
                    st.sql
                );
                assert!(
                    !matches!(got, OracleOutcome::Unknown(_)),
                    "seed {seed}: {:?}: {got:?}",
                    st.sql
                );
            }
        }
    }
}
