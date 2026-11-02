//! Seeded schema generation for the sdb-sql generator: 3-5 tables with 2-6
//! INTEGER/TEXT columns each, SQLite-subset identifier rules
//! (`[a-z_][a-z0-9_]*`), deterministic per seed so a failing run can be
//! reproduced.

use std::collections::BTreeSet;

/// The two dialect types (no REAL, no BLOB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Integer,
    Text,
}

impl Ty {
    pub fn sql(self) -> &'static str {
        match self {
            Ty::Integer => "INTEGER",
            Ty::Text => "TEXT",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: Ty,
    /// Primary key (INTEGER PRIMARY KEY assigns max+1 on NULL/omitted;
    /// TEXT PRIMARY KEY is NOT NULL).
    pub primary_key: bool,
    pub unique: bool,
    pub not_null: bool,
    /// Optional literal default (rendered in the DDL and INSERT-omitted
    /// cases; only decimal integers and simple quoted strings are used).
    pub default: Option<DefaultVal>,
    /// `REFERENCES <table>(<column>)` with restrict semantics (stage 9).
    /// The parent is always the first table's INTEGER primary key, so the
    /// child column is `INTEGER`.
    pub references: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DefaultVal {
    Int(i64),
    Text(String),
}

impl DefaultVal {
    pub fn sql(&self) -> String {
        match self {
            DefaultVal::Int(v) => v.to_string(),
            DefaultVal::Text(v) => format!("'{}'", v.replace('\'', "''")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub tables: Vec<Table>,
}

/// Small deterministic PRNG (xorshift64*) so schemas are seed-reproducible
/// without pulling in a second RNG surface.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Mix the seed so adjacent seeds never collide (a bare `| 1` made
        // 42 and 43 identical) and zero stays usable.
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// Next raw 64-bit value (named `next_u64`: a bare `next` shadows the
    /// standard iterator method and confuses readers).
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// True with probability `pct`/100.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    /// A uniformly chosen element (the slice must be non-empty).
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    /// A short lowercase identifier from a syllable pool.
    pub fn ident(&mut self, used: &mut BTreeSet<String>) -> String {
        const A: [&str; 12] = [
            "id", "name", "val", "cnt", "txt", "num", "slot", "ref", "tag", "pos", "amt", "idx",
        ];
        let mut_name_seed: u64 = self.next_u64();
        for attempt in 0..64u64 {
            let base = A[((mut_name_seed.wrapping_add(attempt)) % A.len() as u64) as usize];
            let candidate = if attempt == 0 && self.below(2) == 0 {
                base.to_string()
            } else {
                format!("{}{}", base, (mut_name_seed.wrapping_add(attempt)) % 97)
            };
            if !used.contains(&candidate) {
                used.insert(candidate.clone());
                return candidate;
            }
        }
        // Fallback: monotonically unique.
        let mut n = 1000u64;
        loop {
            let candidate = format!("c{n}");
            if !used.contains(&candidate) {
                used.insert(candidate.clone());
                return candidate;
            }
            n += 1;
        }
    }
}

/// Generate a schema: `table_count` tables (2-6 columns each) with the
/// first table always carrying an INTEGER PRIMARY KEY (a stable anchor for
/// later tickets) and at most one extra unique/primary-key column per
/// table.
///
/// Every identifier is namespaced with the seed. Stage scripts run one
/// after another against a single long-lived candidate (the cumulative
/// suite), and a candidate is free to keep its state, so two scripts must
/// never collide on a table name.
pub fn generate(seed: u64, table_count: usize, max_columns: usize) -> Schema {
    let mut rng = Rng::new(seed);
    let mut used = BTreeSet::new();
    let ns = format!("s{seed:x}_");
    let mut tables: Vec<Table> = Vec::new();
    for t in 0..table_count {
        let name = format!("{ns}{}", rng.ident(&mut used));
        let cols_n = 2 + rng.below((max_columns.saturating_sub(2)).max(1) as u64 + 1) as usize;
        let cols_n = cols_n.min(max_columns);
        let mut columns = Vec::new();
        // The first table anchors on an INTEGER PRIMARY KEY; later tables
        // sometimes key on TEXT so the NOT NULL key rule is exercised.
        let text_pk = t > 0 && rng.below(3) == 0;
        let mut has_unique = false;
        for c in 0..cols_n {
            let cname = format!("{ns}{}", rng.ident(&mut used));
            let ty = if c == 0 {
                if text_pk {
                    Ty::Text
                } else {
                    Ty::Integer
                }
            } else if rng.below(2) == 0 {
                Ty::Integer
            } else {
                Ty::Text
            };
            let primary_key = c == 0;
            // At most one extra UNIQUE column per table.
            let unique = !primary_key && !has_unique && rng.below(4) == 0;
            has_unique |= unique;
            let not_null = !primary_key && rng.below(5) == 0;
            let default = if !primary_key && rng.below(4) == 0 {
                Some(match ty {
                    Ty::Integer => DefaultVal::Int(rng.range(-9, 9)),
                    Ty::Text => DefaultVal::Text(format!("d{}", rng.below(9))),
                })
            } else {
                None
            };
            columns.push(Column {
                name: cname,
                ty,
                primary_key,
                unique,
                not_null,
                default,
                references: None,
            });
        }
        // Later tables may carry a foreign key to the first table's
        // INTEGER primary key (restrict semantics, `PRAGMA
        // foreign_keys=ON`). The parent column exists from the first
        // table, so the child column's values can be anchored on it.
        if t > 0 && columns.len() < max_columns && rng.below(10) < 6 {
            let fname = format!("{ns}{}", rng.ident(&mut used));
            columns.push(Column {
                name: fname,
                ty: Ty::Integer,
                primary_key: false,
                unique: false,
                not_null: false,
                default: None,
                references: Some((tables[0].name.clone(), tables[0].columns[0].name.clone())),
            });
        }
        tables.push(Table { name, columns });
    }
    Schema { tables }
}

impl Schema {
    /// Dialect DDL for the candidate (no STRICT keyword: the dialect's
    /// strictness is implicit; the oracle appends STRICT when it executes).
    pub fn create_statements(&self) -> Vec<String> {
        self.tables
            .iter()
            .map(|t| {
                let cols: Vec<String> = t
                    .columns
                    .iter()
                    .map(|c| {
                        let mut s = format!("{} {}", c.name, c.ty.sql());
                        if c.primary_key {
                            s.push_str(" PRIMARY KEY");
                        }
                        if c.unique {
                            s.push_str(" UNIQUE");
                        }
                        if c.not_null {
                            s.push_str(" NOT NULL");
                        }
                        if let Some(d) = &c.default {
                            s.push_str(&format!(" DEFAULT {}", d.sql()));
                        }
                        if let Some((pt, pc)) = &c.references {
                            s.push_str(&format!(" REFERENCES {pt}({pc})"));
                        }
                        s
                    })
                    .collect();
                format!("CREATE TABLE {} ({})", t.name, cols.join(", "))
            })
            .collect()
    }

    /// Constraint-free DDL (`name TYPE` only). Primary keys, UNIQUE,
    /// NOT NULL, DEFAULT and REFERENCES are a stage-9 feature, so earlier
    /// stages must not see them.
    pub fn create_statements_plain(&self) -> Vec<String> {
        self.tables
            .iter()
            .map(|t| {
                let cols: Vec<String> = t
                    .columns
                    .iter()
                    .map(|c| format!("{} {}", c.name, c.ty.sql()))
                    .collect();
                format!("CREATE TABLE {} ({})", t.name, cols.join(", "))
            })
            .collect()
    }

    pub fn table(&self, idx: usize) -> &Table {
        &self.tables[idx % self.tables.len()]
    }
}

#[cfg(test)]
mod tests;
