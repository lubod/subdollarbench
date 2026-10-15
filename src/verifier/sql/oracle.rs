//! The differential oracle: an in-process SQLite (`rusqlite`, bundled) that
//! answers every generated statement so the harness never hand-writes
//! expected values.
//!
//! Responsibilities:
//! - **DDL translation**: candidate dialect DDL is executed on SQLite with
//!   `STRICT` appended (the dialect's typing is SQLite-strict) and TEXT
//!   primary keys made `NOT NULL` (the dialect rejects NULL keys; SQLite
//!   historically allows them).
//! - **Error classification**: SQLite messages map to the fixed error
//!   classes the harness compares (`syntax`, `no_table`, `no_column`,
//!   `ambiguous`, `exists`, `arity`, `type`, `constraint`, `txn`, `busy`,
//!   `limit`, `full`, `readonly`, `auth`). An unmapped message is a LOUD
//!   `Unknown` outcome: it fails the verifier self-tests rather than being
//!   guessed at.
//! - **Discards**: statements whose SQLite result is REAL or BLOB (the
//!   dialect excludes them; integer `SUM` promotes to REAL on overflow) are
//!   reported as `Discard` so the generator can regenerate instead of the
//!   harness comparing them.
//!
//! Statement atomicity is SQLite's own: a failing statement changes
//! nothing (the dialect's rule), so the oracle's state stays comparable.

use rusqlite::types::ValueRef;
use rusqlite::Connection;

/// The dialect's error classes (compared, never the free text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrClass {
    Syntax,
    NoTable,
    NoColumn,
    Ambiguous,
    Exists,
    Arity,
    Type,
    Constraint,
    Txn,
    Busy,
    Limit,
    Full,
    Readonly,
    Auth,
}

impl ErrClass {
    /// The wire token the candidate must answer (`ERR <class>`).
    pub fn token(self) -> &'static str {
        match self {
            ErrClass::Syntax => "syntax",
            ErrClass::NoTable => "no_table",
            ErrClass::NoColumn => "no_column",
            ErrClass::Ambiguous => "ambiguous",
            ErrClass::Exists => "exists",
            ErrClass::Arity => "arity",
            ErrClass::Type => "type",
            ErrClass::Constraint => "constraint",
            ErrClass::Txn => "txn",
            ErrClass::Busy => "busy",
            ErrClass::Limit => "limit",
            ErrClass::Full => "full",
            ErrClass::Readonly => "readonly",
            ErrClass::Auth => "auth",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum OracleOutcome {
    /// DML/DDL/txn statement: rows affected.
    Affected(u64),
    /// Query result: column names + rows.
    Rows {
        headers: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// The candidate must answer `ERR <class>`.
    Error(ErrClass),
    /// The statement is outside the compared dialect: regenerate.
    Discard(String),
    /// A SQLite message the class table does not know: loud, fails the
    /// verifier self-tests (never graded).
    Unknown(String),
}

impl OracleOutcome {
    pub fn is_discard(&self) -> bool {
        matches!(self, OracleOutcome::Discard(_))
    }
}

/// Map a SQLite error message to a class. Kept in ONE table and
/// unit-tested; unknown messages return None.
pub fn classify(message: &str) -> Option<ErrClass> {
    let m = message;
    let starts = |p: &str| m.starts_with(p);
    let contains = |p: &str| m.contains(p);
    if starts("no such table") {
        return Some(ErrClass::NoTable);
    }
    if starts("no such column") || starts("no such function") {
        return Some(ErrClass::NoColumn);
    }
    if starts("ambiguous column name") {
        return Some(ErrClass::Ambiguous);
    }
    if contains("already exists") {
        return Some(ErrClass::Exists);
    }
    if starts("datatype mismatch") || contains("cannot store") {
        // STRICT tables answer "cannot store TEXT value in INTEGER column …".
        return Some(ErrClass::Type);
    }
    if contains("constraint failed") {
        // UNIQUE/NOT NULL/CHECK/FOREIGN KEY/PRIMARY KEY constraint failures.
        return Some(ErrClass::Constraint);
    }
    if starts("cannot drop column") || contains("after drop column") {
        // Indexed/PK column drops: the dialect refuses with `constraint`.
        return Some(ErrClass::Constraint);
    }
    if starts("syntax error") || starts("near ") || starts("unrecognized token") {
        return Some(ErrClass::Syntax);
    }
    if contains("wrong number of arguments") {
        return Some(ErrClass::Arity);
    }
    if contains("cannot start a transaction within a transaction")
        || contains("cannot commit - no transaction is active")
        || contains("cannot rollback - no transaction is active")
    {
        return Some(ErrClass::Txn);
    }
    if contains("database is locked") || contains("database table is locked") {
        return Some(ErrClass::Busy);
    }
    if contains("database or disk is full") {
        return Some(ErrClass::Full);
    }
    if contains("attempt to write a readonly database") {
        return Some(ErrClass::Readonly);
    }
    if contains("too many") || contains("string or blob too big") {
        return Some(ErrClass::Limit);
    }
    None
}

/// Messages that mean "outside the compared dialect": the statement is
/// discarded (the generator regenerates) instead of being graded. Integer
/// overflow is explicitly not tested by the dialect.
pub fn discard_reason(message: &str) -> Option<String> {
    if message.contains("integer overflow") {
        return Some("integer overflow (not tested by the dialect)".to_string());
    }
    if message.contains("string or blob too big") {
        return Some("value beyond SQLite's internal size limit".to_string());
    }
    None
}

/// The SQLite-backed reference.
pub struct Oracle {
    conn: Connection,
}

/// Translate one candidate-dialect statement for SQLite: `CREATE TABLE`
/// gets `STRICT` (typed columns), and a TEXT `PRIMARY KEY` column becomes
/// `NOT NULL` (the dialect rejects NULL keys).
pub fn translate_ddl(stmt: &str) -> String {
    let trimmed = stmt.trim().trim_end_matches(';').trim_end();
    let upper = trimmed.to_ascii_uppercase();
    if !upper.starts_with("CREATE TABLE") || upper.contains("STRICT") {
        return trimmed.to_string();
    }
    // Split the column list at the first '(' and the matching last ')'.
    let Some(open) = trimmed.find('(') else {
        return trimmed.to_string();
    };
    let Some(close) = trimmed.rfind(')') else {
        return trimmed.to_string();
    };
    let head = &trimmed[..open];
    let body = &trimmed[open + 1..close];
    let tail = &trimmed[close + 1..];
    let cols: Vec<String> = split_top_level(body)
        .into_iter()
        .map(|col| {
            let cu = col.to_ascii_uppercase();
            // The TYPE is the second token (`<name> <type> ...`).
            let type_is_text = cu
                .split_whitespace()
                .nth(1)
                .map(|t| t == "TEXT")
                .unwrap_or(false);
            if cu.contains("PRIMARY KEY") && type_is_text && !cu.contains("NOT NULL") {
                format!("{col} NOT NULL")
            } else {
                col
            }
        })
        .collect();
    format!("{head}({}) STRICT{tail}", cols.join(", "))
}

/// Split a column list on top-level commas (parenthesized defaults).
fn split_top_level(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

impl Oracle {
    pub fn new() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open sqlite: {e}"))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("pragma: {e}"))?;
        Ok(Self { conn })
    }

    /// Execute one statement (DDL translated) and classify the outcome.
    pub fn exec(&self, stmt: &str) -> OracleOutcome {
        let stmt_trimmed = stmt.trim().trim_end_matches(';').trim();
        let upper = stmt_trimmed.to_ascii_uppercase();
        let ddl = translate_ddl(stmt_trimmed);
        let is_query = upper.starts_with("SELECT")
            || upper.starts_with("EXPLAIN")
            || (upper.starts_with("WITH") && !upper.starts_with("WITH RECURSIVE"));
        if is_query {
            return self.query(&ddl);
        }
        match self.conn.execute(&ddl, []) {
            Ok(n) => OracleOutcome::Affected(n as u64),
            Err(e) => {
                let msg = e.to_string();
                if let Some(reason) = discard_reason(&msg) {
                    return OracleOutcome::Discard(reason);
                }
                match classify(&msg) {
                    Some(c) => OracleOutcome::Error(c),
                    None => OracleOutcome::Unknown(msg),
                }
            }
        }
    }

    /// Execute a query and collect headers + rows, discarding REAL/BLOB.
    pub fn query(&self, sql: &str) -> OracleOutcome {
        let mut prepared = match self.conn.prepare(sql) {
            Ok(p) => p,
            Err(e) => {
                let msg = e.to_string();
                if let Some(reason) = discard_reason(&msg) {
                    return OracleOutcome::Discard(reason);
                }
                return match classify(&msg) {
                    Some(c) => OracleOutcome::Error(c),
                    None => OracleOutcome::Unknown(msg),
                };
            }
        };
        let headers: Vec<String> = prepared
            .column_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let mut rows_out: Vec<Vec<Value>> = Vec::new();
        let mut cursor = match prepared.query([]) {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                if let Some(reason) = discard_reason(&msg) {
                    return OracleOutcome::Discard(reason);
                }
                return match classify(&msg) {
                    Some(c) => OracleOutcome::Error(c),
                    None => OracleOutcome::Unknown(msg),
                };
            }
        };
        loop {
            match cursor.next() {
                Ok(Some(row)) => {
                    let mut vals = Vec::with_capacity(headers.len());
                    for i in 0..headers.len() {
                        match row.get_ref(i) {
                            Ok(ValueRef::Null) => vals.push(Value::Null),
                            Ok(ValueRef::Integer(v)) => vals.push(Value::Int(v)),
                            Ok(ValueRef::Text(t)) => {
                                vals.push(Value::Text(String::from_utf8_lossy(t).into_owned()))
                            }
                            Ok(ValueRef::Real(_)) | Ok(ValueRef::Blob(_)) => {
                                return OracleOutcome::Discard(format!(
                                    "REAL/BLOB result in column {i} (outside the dialect)"
                                ))
                            }
                            Err(e) => {
                                return OracleOutcome::Unknown(format!("row read: {e}"));
                            }
                        }
                    }
                    rows_out.push(vals);
                }
                Ok(None) => break,
                Err(e) => {
                    let msg = e.to_string();
                    return if let Some(reason) = discard_reason(&msg) {
                        OracleOutcome::Discard(reason)
                    } else {
                        match classify(&msg) {
                            Some(c) => OracleOutcome::Error(c),
                            None => OracleOutcome::Unknown(msg),
                        }
                    };
                }
            }
        }
        OracleOutcome::Rows {
            headers,
            rows: rows_out,
        }
    }

    /// A second handle on a file-backed database (oracle-vs-oracle gates
    /// compare in-memory against on-disk semantics).
    pub fn new_file(path: &std::path::Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open sqlite file: {e}"))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("pragma: {e}"))?;
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> Oracle {
        Oracle::new().unwrap()
    }

    #[test]
    fn ddl_translation_appends_strict_and_pk_not_null() {
        let t = translate_ddl("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT PRIMARY KEY)");
        assert!(t.ends_with(" STRICT"), "{t}");
        assert!(t.contains("k TEXT PRIMARY KEY NOT NULL"), "{t}");
        // Idempotent on already-strict DDL.
        assert_eq!(translate_ddl(&t), t);
    }

    #[test]
    fn strict_typing_and_text_pk_null_rejection() {
        let o = oracle();
        assert_eq!(
            o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)"),
            OracleOutcome::Affected(0)
        );
        assert_eq!(
            o.exec("CREATE TABLE u (k TEXT PRIMARY KEY, n INTEGER)"),
            OracleOutcome::Affected(0)
        );
        // Wrong type into a typed column -> type.
        assert_eq!(
            o.exec("INSERT INTO t (n) VALUES ('not-an-int')"),
            OracleOutcome::Error(ErrClass::Type)
        );
        // TEXT PRIMARY KEY rejects NULL in the dialect.
        assert_eq!(
            o.exec("INSERT INTO u (k, n) VALUES (NULL, 1)"),
            OracleOutcome::Error(ErrClass::Constraint)
        );
        // INTEGER PRIMARY KEY auto-assigns; NULL is fine.
        assert_eq!(
            o.exec("INSERT INTO t (n) VALUES (1)"),
            OracleOutcome::Affected(1)
        );
        match o.query("SELECT id FROM t") {
            OracleOutcome::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_classes_map_to_tokens() {
        let o = oracle();
        o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER UNIQUE)");
        let cases = [
            ("SELECT * FROM nope", ErrClass::NoTable),
            ("SELECT nope FROM t", ErrClass::NoColumn),
            ("CREATE TABLE t (x INTEGER)", ErrClass::Exists),
            ("SELEC * FROM t", ErrClass::Syntax),
            ("INSERT INTO t (n) VALUES ('x')", ErrClass::Type),
            ("INSERT INTO t (n) VALUES (1), (1)", ErrClass::Constraint),
            ("COMMIT", ErrClass::Txn),
        ];
        for (stmt, want) in cases {
            match o.exec(stmt) {
                OracleOutcome::Error(got) => assert_eq!(got, want, "{stmt}"),
                other => panic!("{stmt}: {other:?}"),
            }
            assert!(!want.token().is_empty());
        }
    }

    #[test]
    fn ambiguous_columns_classify() {
        let o = oracle();
        o.exec("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)");
        o.exec("CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)");
        o.exec("INSERT INTO a (v) VALUES (1)");
        o.exec("INSERT INTO b (v) VALUES (2)");
        match o.exec("SELECT v FROM a, b") {
            OracleOutcome::Error(ErrClass::Ambiguous) => {}
            other => panic!("{other:?}"),
        }
        // Qualified columns are fine; SELECT * lists columns in table order.
        match o.exec("SELECT a.v, b.v FROM a, b") {
            OracleOutcome::Rows { headers, rows } => {
                assert_eq!(headers, vec!["v", "v"]);
                assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(2)]]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn real_and_blob_results_are_discards() {
        let o = oracle();
        o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
        o.exec("INSERT INTO t (n) VALUES (9223372036854775807), (9223372036854775807)");
        // Integer SUM overflow lands outside the dialect (SQLite either
        // raises "integer overflow" or promotes to REAL): discard either way.
        match o.exec("SELECT SUM(n) FROM t") {
            OracleOutcome::Discard(reason) => {
                assert!(
                    reason.contains("overflow") || reason.contains("REAL/BLOB"),
                    "{reason}"
                )
            }
            other => panic!("{other:?}"),
        }
        // Ordinary integer arithmetic stays comparable.
        o.exec("INSERT INTO t (n) VALUES (1)");
        match o.exec("SELECT n + 1 FROM t WHERE n = 1") {
            OracleOutcome::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]),
            other => panic!("{other:?}"),
        }
        // Overflowing arithmetic is a discard (not tested by the dialect);
        // SQLite may raise "integer overflow" or promote to REAL depending
        // on the operation.
        match o.exec("SELECT n + 1 FROM t WHERE n = 9223372036854775807") {
            OracleOutcome::Discard(reason) => assert!(
                reason.contains("overflow") || reason.contains("REAL/BLOB"),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn integer_semantics_match_the_dialect() {
        let o = oracle();
        // Division truncates toward zero; modulo takes the dividend's sign;
        // division by zero yields NULL; unary minus works.
        let checks = [
            ("SELECT -7 / 2", Value::Int(-3)),
            ("SELECT -7 % 3", Value::Int(-1)),
            ("SELECT 7 % -3", Value::Int(1)),
            ("SELECT 1 / 0", Value::Null),
            ("SELECT 1 % 0", Value::Null),
            ("SELECT 2 + 3 * 4", Value::Int(14)),
            ("SELECT (2 + 3) * 4", Value::Int(20)),
        ];
        for (stmt, want) in checks {
            match o.exec(stmt) {
                OracleOutcome::Rows { rows, .. } => {
                    assert_eq!(rows[0][0], want, "{stmt}");
                }
                other => panic!("{stmt}: {other:?}"),
            }
        }
        // NULL ordering: first ascending, last descending.
        let o2 = oracle();
        o2.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
        o2.exec("INSERT INTO t (n) VALUES (1), (NULL), (2)");
        match o2.exec("SELECT n FROM t ORDER BY n ASC") {
            OracleOutcome::Rows { rows, .. } => assert_eq!(
                rows,
                vec![vec![Value::Null], vec![Value::Int(1)], vec![Value::Int(2)]]
            ),
            other => panic!("{other:?}"),
        }
        match o2.exec("SELECT n FROM t ORDER BY n DESC") {
            OracleOutcome::Rows { rows, .. } => assert_eq!(
                rows,
                vec![vec![Value::Int(2)], vec![Value::Int(1)], vec![Value::Null]]
            ),
            other => panic!("{other:?}"),
        }
    }

    /// The seed of the oracle-vs-oracle gate: a hand-written corpus must
    /// produce identical outcomes on in-memory and file-backed SQLite.
    #[test]
    fn oracle_vs_oracle_corpus_agrees() {
        let mem = oracle();
        let dir = std::env::temp_dir().join(format!("sdb-sql-oracle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = Oracle::new_file(&dir.join("ref.db")).unwrap();

        let corpus = [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)",
            "INSERT INTO t (n, s) VALUES (1, 'a')",
            "INSERT INTO t (n, s) VALUES (2, 'b'), (3, NULL)",
            "SELECT * FROM t",
            "SELECT id, n FROM t WHERE n > 1 ORDER BY id",
            "SELECT COUNT(*) FROM t",
            "SELECT s FROM t ORDER BY s",
            "UPDATE t SET n = n + 1 WHERE s = 'a'",
            "SELECT n FROM t WHERE id = 1",
            "DELETE FROM t WHERE n = 3",
            "SELECT * FROM t",
            "SELECT nope FROM t",
            "INSERT INTO t (n) VALUES ('x')",
            "SELECT SUM(n) FROM t",
            "BEGIN",
            "INSERT INTO t (n, s) VALUES (9, 'z')",
            "ROLLBACK",
            "SELECT COUNT(*) FROM t",
        ];
        for stmt in corpus {
            let a = mem.exec(stmt);
            let b = file.exec(stmt);
            assert!(!matches!(a, OracleOutcome::Unknown(_)), "{stmt}: {a:?}");
            assert_eq!(a, b, "oracle disagreement on {stmt}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
