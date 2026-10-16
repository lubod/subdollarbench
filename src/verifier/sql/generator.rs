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
            constraints: self.constraints || o.constraints,
        }
    }

    /// The cumulative profile of a set of passed stages.
    pub fn for_passed(stages: &[u32]) -> Profile {
        stages
            .iter()
            .fold(Profile::default(), |acc, s| acc.union(&Profile::stage(*s)))
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
    /// A deliberate error (missing table/column, arity, type, syntax,
    /// duplicate create, atomicity).
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
            bounds,
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

    fn bound_of(&self, col: &str) -> u128 {
        self.bounds.get(col).copied().unwrap_or(0)
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
            .any(|tok| cols.iter().any(|c| c.name == tok))
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
            .filter(|&i| self.seen.get(&cols[i].name).is_some_and(|v| !v.is_empty()))
            .collect();
        if !anchored.is_empty() && self.rng.chance(45) {
            let c = &cols[anchored[self.rng.below(anchored.len() as u64) as usize]];
            let n = self.seen[&c.name].len();
            let lit = self.seen[&c.name][self.rng.below(n as u64) as usize].clone();
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
            if self.profile.multi_row_insert {
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
            _ => format!(
                "{} {tname} {} zz_nocol = 1",
                self.kw("UPDATE"),
                self.kw("SET")
            ),
        };
        self.stmt(sql, Kind::Probe, false)
    }

    /// The next statement of a mixed workload, weighted by the profile.
    pub fn next_statement(&mut self) -> Statement {
        let w_insert = 24u64;
        let w_select = 40u64;
        let w_update = if self.profile.update { 14 } else { 0 };
        let w_delete = if self.profile.delete { 6 } else { 0 };
        let w_probe = 10u64;
        let total = w_insert + w_select + w_update + w_delete + w_probe;
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
        self.probe()
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
    script
}

#[cfg(test)]
mod tests {
    use super::*;
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
        for last in [0u32, 1, 2, 3, 9] {
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
                    },
                    Column {
                        name: "n".into(),
                        ty: Ty::Integer,
                        primary_key: false,
                        unique: false,
                        not_null: false,
                        default: Some(DefaultVal::Int(-9)),
                    },
                    Column {
                        name: "s".into(),
                        ty: Ty::Text,
                        primary_key: false,
                        unique: false,
                        not_null: false,
                        default: Some(DefaultVal::Text("d123".into())),
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
                    },
                    Column {
                        name: "n".into(),
                        ty: Ty::Integer,
                        primary_key: false,
                        unique: false,
                        not_null: false,
                        default: Some(DefaultVal::Int(i64::MAX)),
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
