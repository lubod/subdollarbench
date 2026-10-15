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
            "id", "name", "val", "cnt", "txt", "num", "key", "ref", "tag", "pos", "amt", "idx",
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
pub fn generate(seed: u64, table_count: usize, max_columns: usize) -> Schema {
    let mut rng = Rng::new(seed);
    let mut used = BTreeSet::new();
    let mut tables = Vec::new();
    for t in 0..table_count {
        let name = rng.ident(&mut used);
        let cols_n = 2 + rng.below((max_columns.saturating_sub(2)).max(1) as u64 + 1) as usize;
        let cols_n = cols_n.min(max_columns);
        let mut columns = Vec::new();
        for c in 0..cols_n {
            let cname = rng.ident(&mut used);
            let ty = if c == 0 || rng.below(2) == 0 {
                Ty::Integer
            } else {
                Ty::Text
            };
            let primary_key = c == 0;
            let unique = !primary_key && rng.below(4) == 0;
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
            });
        }
        let _ = t;
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
                        s
                    })
                    .collect();
                format!("CREATE TABLE {} ({})", t.name, cols.join(", "))
            })
            .collect()
    }

    pub fn table(&self, idx: usize) -> &Table {
        &self.tables[idx % self.tables.len()]
    }

    /// Columns that are not the INTEGER PRIMARY KEY rowid alias (safe to
    /// target in generated INSERT/UPDATE statements).
    pub fn insertable_columns(table: &Table) -> Vec<&Column> {
        table
            .columns
            .iter()
            .filter(|c| !(c.primary_key && c.ty == Ty::Integer))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_deterministic_and_valid() {
        let a = generate(42, 4, 6);
        let b = generate(42, 4, 6);
        let a_json = format!("{:?}", a.tables);
        let b_json = format!("{:?}", b.tables);
        assert_eq!(a_json, b_json, "same seed must generate the same schema");
        let c = generate(43, 4, 6);
        assert_ne!(a_json, format!("{:?}", c.tables), "seeds must differ");

        for t in &a.tables {
            assert!(!t.name.is_empty());
            assert!(t.columns.len() >= 2 && t.columns.len() <= 6);
            for col in &t.columns {
                assert!(col
                    .name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'));
            }
        }
        // Exactly one PK per table; the first table's PK is the INTEGER
        // rowid alias anchor.
        for t in &a.tables {
            assert_eq!(t.columns.iter().filter(|c| c.primary_key).count(), 1);
        }
        assert!(a.tables[0].columns[0].primary_key);
        assert_eq!(a.tables[0].columns[0].ty, Ty::Integer);
    }

    #[test]
    fn ddl_shape_is_subset_sqlite() {
        let s = generate(7, 3, 5);
        for stmt in s.create_statements() {
            assert!(stmt.starts_with("CREATE TABLE "));
            assert!(stmt.ends_with(')'));
            assert!(
                !stmt.contains("STRICT"),
                "candidate DDL stays dialect-level"
            );
        }
    }
}
