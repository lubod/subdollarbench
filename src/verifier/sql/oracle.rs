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
    if contains("duplicate column name")
        || contains("already another table or index with this name")
    {
        // ALTER TABLE renames/adds that would take an existing name: a
        // schema object's name is `exists`, like CREATE TABLE's.
        return Some(ErrClass::Exists);
    }
    if starts("no such table") || starts("no such index") {
        // An index is a schema object like a table: the dialect has no
        // separate class for it.
        return Some(ErrClass::NoTable);
    }
    if starts("no such column") || starts("no such function") || contains("has no column named") {
        // The last form is an INSERT column list naming a missing column.
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
    if starts("syntax error")
        || starts("near ")
        || starts("unrecognized token")
        || starts("incomplete input")
        // Prepare-time aggregate misuses (an aggregate in WHERE, or an
        // aggregate as a GROUP BY key) are statement-level rejections.
        || starts("misuse of aggregate")
        || contains("aggregate functions are not allowed")
    {
        return Some(ErrClass::Syntax);
    }
    if contains("wrong number of arguments")
        // INSERT column list vs VALUES row ("1 values for 2 columns") and
        // the no-column-list form ("table t has 3 columns but 1 values ...").
        || (contains(" values for ") && contains(" columns"))
        || (contains(" columns but ") && contains("values were supplied"))
        // Set operations and subqueries with the wrong column count.
        || contains("do not have the same number of result columns")
        || starts("sub-select returns")
    {
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
    // ("string or blob too big" never reaches here: `discard_reason` runs
    // first and drops it.)
    if contains("too many") {
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
    // The dialect has no REAL, so a REAL can only come from integer overflow
    // promoting inside INSERT/UPDATE (`n + 1` at i64::MAX); a STRICT INTEGER
    // column then refuses it with "cannot store REAL value ...".
    if message.contains("cannot store REAL value") || message.contains("cannot store BLOB value") {
        return Some("REAL/BLOB produced by overflow (not tested by the dialect)".to_string());
    }
    if message.contains("string or blob too big") {
        return Some("value beyond SQLite's internal size limit".to_string());
    }
    None
}

/// The SQLite-backed reference.
enum AlterRewrite {
    AsIs,
    Noop,
    Run(String),
}

pub struct Oracle {
    conn: Connection,
}

/// Blank out single-quoted literals (`''` escapes a quote) so keyword and
/// punctuation scans never see text inside a string default. The mask is
/// byte-for-byte the same length as the input: every byte inside a quoted
/// literal becomes an ASCII space, so an index computed on the mask is a
/// valid byte offset in the original even when a literal contains
/// multi-byte characters.
fn mask_quoted(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut in_q = false;
    for b in s.bytes() {
        match b {
            b'\'' => {
                in_q = !in_q;
                out.push(b);
            }
            _ if in_q => out.push(b' '),
            _ => out.push(b),
        }
    }
    // Bytes outside quotes are unchanged; inside they became ASCII spaces.
    String::from_utf8(out).expect("masked bytes are valid UTF-8")
}

/// Byte offset of the `)` matching the `(` at `open` (quote-aware).
fn matching_close(s: &str, open: usize) -> Option<usize> {
    let masked = mask_quoted(s);
    let mut depth = 0i32;
    for (i, ch) in masked.char_indices().skip_while(|(i, _)| *i < open) {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whole-word test on already-masked, upper-cased text.
fn has_word(masked_upper: &str, word: &str) -> bool {
    masked_upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| w == word)
}

/// Translate one candidate-dialect statement for SQLite: `CREATE TABLE`
/// gets `STRICT` (typed columns), and a TEXT `PRIMARY KEY` column, written
/// on the column or as a table-level `PRIMARY KEY (col, ...)`, becomes
/// `NOT NULL` (the dialect rejects NULL keys).
pub fn translate_ddl(stmt: &str) -> String {
    let trimmed = stmt.trim().trim_end_matches(';').trim_end();
    let upper = trimmed.to_ascii_uppercase();
    if !upper.starts_with("CREATE TABLE") {
        return trimmed.to_string();
    }
    let masked = mask_quoted(trimmed);
    let Some(open) = masked.find('(') else {
        return trimmed.to_string();
    };
    let Some(close) = matching_close(trimmed, open) else {
        return trimmed.to_string();
    };
    let head = &trimmed[..open];
    let body = &trimmed[open + 1..close];
    let tail = trimmed[close + 1..].trim();
    // STRICT already present as a table option (a column named `strictness`
    // or a default of 'strict' must not count).
    if has_word(&mask_quoted(tail).to_ascii_uppercase(), "STRICT") {
        return trimmed.to_string();
    }
    let segments = split_top_level(body);

    // Columns named by a table-level `PRIMARY KEY (a, b)` constraint.
    let mut table_pk: Vec<String> = Vec::new();
    for seg in &segments {
        let mu = mask_quoted(seg).to_ascii_uppercase();
        if mu.trim_start().starts_with("PRIMARY KEY") {
            if let (Some(o), Some(c)) = (seg.find('('), seg.rfind(')')) {
                table_pk.extend(
                    seg[o + 1..c]
                        .split(',')
                        .map(|n| n.trim().to_ascii_lowercase()),
                );
            }
        }
    }

    let cols: Vec<String> = segments
        .into_iter()
        .map(|col| {
            let mu = mask_quoted(&col).to_ascii_uppercase();
            let mut toks = mu.split_whitespace();
            let first = toks.next().unwrap_or("");
            // Table-level constraints pass through untouched.
            if matches!(
                first,
                "PRIMARY" | "UNIQUE" | "FOREIGN" | "CHECK" | "CONSTRAINT"
            ) {
                return col;
            }
            // `<name> <type> ...`: the TYPE is the second token.
            let type_is_text = toks.next() == Some("TEXT");
            let is_pk =
                mu.contains("PRIMARY KEY") || table_pk.contains(&first.to_ascii_lowercase());
            if is_pk && type_is_text && !mu.contains("NOT NULL") {
                format!("{col} NOT NULL")
            } else {
                col
            }
        })
        .collect();
    // SQLite wants table options comma-separated: `STRICT, WITHOUT ROWID`.
    let options = if tail.is_empty() {
        "STRICT".to_string()
    } else {
        format!("STRICT, {tail}")
    };
    format!("{head}({}) {options}", cols.join(", "))
}

/// Split a column list on top-level commas (not inside parentheses or
/// quoted literals).
fn split_top_level(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_q = false;
    let mut cur = String::new();
    for ch in body.chars() {
        match ch {
            '\'' => {
                in_q = !in_q;
                cur.push(ch);
            }
            '(' if !in_q => {
                depth += 1;
                cur.push(ch);
            }
            ')' if !in_q => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 && !in_q => {
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

/// Turn a SQLite error message into an outcome: discard first (outside the
/// dialect), then the class table, else a loud `Unknown`.
fn outcome_from_error(msg: String) -> OracleOutcome {
    if let Some(reason) = discard_reason(&msg) {
        return OracleOutcome::Discard(reason);
    }
    match classify(&msg) {
        Some(c) => OracleOutcome::Error(c),
        None => OracleOutcome::Unknown(msg),
    }
}

impl Oracle {
    fn from_conn(conn: Connection) -> Result<Self, String> {
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("pragma: {e}"))?;
        Ok(Self { conn })
    }

    pub fn new() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open sqlite: {e}"))?;
        Self::from_conn(conn)
    }

    /// A second handle on a file-backed database (oracle-vs-oracle gates
    /// compare in-memory against on-disk semantics).
    pub fn new_file(path: &std::path::Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open sqlite file: {e}"))?;
        // A reference twin, never a durable store: skip fsync so a 100k
        // statement gate runs in seconds.
        conn.execute_batch("PRAGMA synchronous=OFF;")
            .map_err(|e| format!("pragma: {e}"))?;
        Self::from_conn(conn)
    }

    /// Execute one statement (DDL translated) and classify the outcome.
    pub fn exec(&self, stmt: &str) -> OracleOutcome {
        let stmt_trimmed = stmt.trim().trim_end_matches(';').trim();
        // SQLite has no `IF [NOT] EXISTS` on ALTER; the dialect does. The
        // rewrite needs the live schema, so it runs here, not in
        // `translate_ddl`.
        match self.alter_if_exists(stmt_trimmed) {
            AlterRewrite::Noop => OracleOutcome::Affected(0),
            AlterRewrite::Run(sql) => self.run(&translate_ddl(&sql)),
            AlterRewrite::AsIs => self.run(&translate_ddl(stmt_trimmed)),
        }
    }

    /// `ALTER TABLE ... ADD [COLUMN] IF NOT EXISTS c ...` and
    /// `... DROP [COLUMN] IF EXISTS c` (ticket 13). A no-op when the
    /// existence condition already holds; otherwise the plain statement runs.
    fn alter_if_exists(&self, stmt: &str) -> AlterRewrite {
        let toks: Vec<&str> = stmt.split_whitespace().collect();
        let kw = |i: usize| toks.get(i).copied().unwrap_or("");
        if !kw(0).eq_ignore_ascii_case("ALTER") || !kw(1).eq_ignore_ascii_case("TABLE") {
            return AlterRewrite::AsIs;
        }
        let table = kw(2);
        let table_exists = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1 COLLATE NOCASE",
                [table],
                |_| Ok(()),
            )
            .is_ok();
        let column_exists = |name: &str| {
            self.conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2 COLLATE NOCASE",
                    [table, name],
                    |_| Ok(()),
                )
                .is_ok()
        };
        let mut i = 3;
        if kw(i).eq_ignore_ascii_case("ADD") {
            i += 1;
            if kw(i).eq_ignore_ascii_case("COLUMN") {
                i += 1;
            }
            if !(kw(i).eq_ignore_ascii_case("IF")
                && kw(i + 1).eq_ignore_ascii_case("NOT")
                && kw(i + 2).eq_ignore_ascii_case("EXISTS"))
            {
                return AlterRewrite::AsIs;
            }
            let Some(name) = toks.get(i + 3) else {
                return AlterRewrite::AsIs;
            };
            let rest = toks[i + 3..].join(" ");
            let sql = format!("ALTER TABLE {table} ADD COLUMN {rest}");
            if !table_exists {
                return AlterRewrite::Run(sql);
            }
            if column_exists(name) {
                AlterRewrite::Noop
            } else {
                AlterRewrite::Run(sql)
            }
        } else if kw(i).eq_ignore_ascii_case("DROP") {
            i += 1;
            if kw(i).eq_ignore_ascii_case("COLUMN") {
                i += 1;
            }
            if !(kw(i).eq_ignore_ascii_case("IF") && kw(i + 1).eq_ignore_ascii_case("EXISTS")) {
                return AlterRewrite::AsIs;
            }
            let Some(name) = toks.get(i + 2) else {
                return AlterRewrite::AsIs;
            };
            let sql = format!("ALTER TABLE {table} DROP COLUMN {name}");
            if !table_exists {
                return AlterRewrite::Run(sql);
            }
            if column_exists(name) {
                AlterRewrite::Run(sql)
            } else {
                AlterRewrite::Noop
            }
        } else {
            AlterRewrite::AsIs
        }
    }

    /// Run one statement. Whether it is a query is decided by SQLite itself
    /// (`column_count() > 0`), not by a keyword prefix, so `WITH ...`,
    /// `VALUES`, `EXPLAIN` and `INSERT ... RETURNING` all route correctly.
    pub fn query(&self, sql: &str) -> OracleOutcome {
        self.run(sql)
    }

    fn run(&self, sql: &str) -> OracleOutcome {
        let mut prepared = match self.conn.prepare(sql) {
            Ok(p) => p,
            Err(e) => return outcome_from_error(e.to_string()),
        };
        // A result-producing statement is a read. `column_count` alone is
        // not enough: ALTER TABLE on a STRICT table compiles a nested
        // `pragma_quick_check` SELECT and reports one result column.
        if prepared.column_count() == 0 || !prepared.readonly() {
            // DML/DDL/transaction control. `execute` reports
            // `sqlite3_changes()`, which keeps the count of the last
            // INSERT/UPDATE/DELETE across statements that change no rows
            // (CREATE, BEGIN, COMMIT, ...), so use the delta of the
            // connection's total instead.
            let before = self.conn.total_changes();
            // The dialect's statement atomicity: a failing statement changes
            // nothing. Plain SQLite 3.46 keeps the rows an earlier
            // multi-row `VALUES` inserted when the failure happens *inside
            // an explicit transaction* (a statement-journal quirk, not a
            // dialect rule). A statement savepoint makes the graded
            // behavior the spec's, in every SQLite version and in both
            // modes.
            let upper = sql.trim_start().to_ascii_uppercase();
            let is_txn_ctl = ["BEGIN", "COMMIT", "END", "ROLLBACK", "SAVEPOINT", "RELEASE"]
                .iter()
                .any(|k| upper.starts_with(k));
            if !is_txn_ctl {
                let _ = self.conn.execute_batch("SAVEPOINT sdb_stmt;");
            }
            let result = prepared.execute([]);
            if !is_txn_ctl {
                // Success merges the savepoint; a failure rolls the whole
                // statement back before releasing.
                if result.is_ok() {
                    let _ = self.conn.execute_batch("RELEASE sdb_stmt;");
                } else {
                    let _ = self
                        .conn
                        .execute_batch("ROLLBACK TO sdb_stmt; RELEASE sdb_stmt;");
                }
            }
            return match result {
                Ok(n) => {
                    let changed = self.conn.total_changes() - before;
                    OracleOutcome::Affected(if changed == 0 { 0 } else { n as u64 })
                }
                // The savepoint already undid the statement's rows
                // (`total_changes` counts rolled-back rows too, so it is no
                // atomicity check).
                Err(e) => outcome_from_error(e.to_string()),
            };
        }
        let headers: Vec<String> = prepared
            .column_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let mut rows_out: Vec<Vec<Value>> = Vec::new();
        let mut cursor = match prepared.query([]) {
            Ok(c) => c,
            Err(e) => return outcome_from_error(e.to_string()),
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
                Err(e) => return outcome_from_error(e.to_string()),
            }
        }
        OracleOutcome::Rows {
            headers,
            rows: rows_out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Statement atomicity holds in autocommit *and* inside a transaction:
    /// plain SQLite 3.46 keeps the rows a failed multi-row `VALUES` had
    /// already inserted inside a transaction (a statement-journal quirk),
    /// and the oracle's statement savepoint makes the graded behavior the
    /// spec's ("a failing statement changes nothing").
    #[test]
    fn a_failing_statement_changes_nothing_in_a_transaction() {
        let o = Oracle::new().unwrap();
        let _ = o.exec("CREATE TABLE t (a INTEGER, b INTEGER)");
        let _ = o.exec("BEGIN");
        match o.exec("INSERT INTO t VALUES (1, 1), ('zz', 2)") {
            OracleOutcome::Error(ErrClass::Type) => {}
            other => panic!("expected a type error, got {other:?}"),
        }
        match o.exec("SELECT * FROM t") {
            OracleOutcome::Rows { rows, .. } => {
                assert!(rows.is_empty(), "the failed statement left rows: {rows:?}")
            }
            other => panic!("{other:?}"),
        }
        let _ = o.exec("ROLLBACK");
        // `INSERT ... SELECT` with one bad row is atomic too (autocommit).
        let _ = o.exec("INSERT INTO t VALUES (5, 5)");
        match o.exec("INSERT INTO t SELECT a, 'zz' FROM t") {
            OracleOutcome::Error(ErrClass::Type) => {}
            other => panic!("expected a type error, got {other:?}"),
        }
        match o.exec("SELECT COUNT(*) FROM t") {
            OracleOutcome::Rows { rows, .. } => {
                assert_eq!(rows[0][0], Value::Int(1), "only the good row")
            }
            other => panic!("{other:?}"),
        }
    }

    /// The aggregate misuses the stage-7 probes emit are statement-level
    /// rejections (class `syntax`), and the empty-group contract the spec
    /// pins holds: `SUM` of no rows is NULL, `COUNT` is 0.
    #[test]
    fn aggregate_misuses_and_the_empty_group() {
        let o = Oracle::new().unwrap();
        let _ = o.exec("CREATE TABLE t (a INTEGER, b TEXT)");
        for sql in [
            "SELECT COUNT(*) FROM t WHERE COUNT(*) > 1",
            "SELECT * FROM t GROUP BY COUNT(*)",
        ] {
            match o.exec(sql) {
                OracleOutcome::Error(ErrClass::Syntax) => {}
                other => panic!("{sql}: {other:?}"),
            }
        }
        match o.exec("SELECT SUM(a), COUNT(*), MAX(b) FROM t WHERE a > 999") {
            OracleOutcome::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Value::Null, "SUM of no rows");
                assert_eq!(rows[0][1], Value::Int(0), "COUNT of no rows");
                assert_eq!(rows[0][2], Value::Null, "MAX of no rows");
            }
            other => panic!("{other:?}"),
        }
    }

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

    #[test]
    fn non_dml_statements_report_zero_affected_after_dml() {
        let o = oracle();
        o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
        assert_eq!(
            o.exec("INSERT INTO t (n) VALUES (1), (2), (3)"),
            OracleOutcome::Affected(3)
        );
        // sqlite3_changes() would still say 3 for each of these.
        for stmt in [
            "CREATE TABLE u (id INTEGER PRIMARY KEY)",
            "CREATE INDEX i ON t (n)",
            "BEGIN",
            "COMMIT",
            "DROP INDEX i",
            "DROP TABLE u",
        ] {
            assert_eq!(o.exec(stmt), OracleOutcome::Affected(0), "{stmt}");
        }
        // DML that touches no rows is 0; real counts are untouched.
        assert_eq!(
            o.exec("UPDATE t SET n = 9 WHERE id = 99"),
            OracleOutcome::Affected(0)
        );
        assert_eq!(o.exec("UPDATE t SET n = n + 1"), OracleOutcome::Affected(3));
        assert_eq!(o.exec("DELETE FROM t"), OracleOutcome::Affected(3));
    }

    #[test]
    fn statements_route_by_column_count_not_keyword() {
        let o = oracle();
        o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
        match o.exec(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3) \
             SELECT x FROM c",
        ) {
            OracleOutcome::Rows { rows, .. } => assert_eq!(
                rows,
                vec![
                    vec![Value::Int(1)],
                    vec![Value::Int(2)],
                    vec![Value::Int(3)]
                ]
            ),
            other => panic!("{other:?}"),
        }
        // A CTE-prefixed INSERT is DML, not a query.
        assert_eq!(
            o.exec("WITH s AS (SELECT 5 AS v) INSERT INTO t (n) SELECT v FROM s"),
            OracleOutcome::Affected(1)
        );
        match o.exec("VALUES (1), (2)") {
            OracleOutcome::Rows { rows, .. } => assert_eq!(rows.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn overflow_in_dml_is_a_discard_not_a_type_error() {
        let o = oracle();
        o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
        o.exec("INSERT INTO t (n) VALUES (9223372036854775807)");
        for stmt in [
            "UPDATE t SET n = n + 1",
            "INSERT INTO t (n) SELECT n + 1 FROM t",
        ] {
            assert!(o.exec(stmt).is_discard(), "{stmt}: {:?}", o.exec(stmt));
        }
        // A genuine wrong-type value is still graded.
        assert_eq!(
            o.exec("INSERT INTO t (n) VALUES ('x')"),
            OracleOutcome::Error(ErrClass::Type)
        );
    }

    #[test]
    fn arity_and_incomplete_input_classify() {
        let o = oracle();
        o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)");
        let cases = [
            ("INSERT INTO t (n, s) VALUES (1)", ErrClass::Arity),
            ("INSERT INTO t (n) VALUES (1, 2)", ErrClass::Arity),
            ("INSERT INTO t VALUES (1)", ErrClass::Arity),
            ("SELECT 1 UNION SELECT 1, 2", ErrClass::Arity),
            (
                "SELECT n FROM t WHERE n IN (SELECT n, s FROM t)",
                ErrClass::Arity,
            ),
            ("SELECT * FROM", ErrClass::Syntax),
            ("SELECT", ErrClass::Syntax),
            ("INSERT INTO t (nope) VALUES (1)", ErrClass::NoColumn),
        ];
        for (stmt, want) in cases {
            match o.exec(stmt) {
                OracleOutcome::Error(got) => assert_eq!(got, want, "{stmt}"),
                other => panic!("{stmt}: {other:?}"),
            }
        }
    }

    #[test]
    fn ddl_translation_is_quote_and_keyword_safe() {
        // A comma inside a quoted default must not split the column, and the
        // literal must survive byte for byte.
        let t = translate_ddl("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT DEFAULT 'a,b')");
        assert!(t.contains("DEFAULT 'a,b'"), "{t}");
        assert!(t.ends_with(" STRICT"), "{t}");
        // A paren inside a quoted default must not unbalance the scan.
        let t = translate_ddl("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT DEFAULT '(')");
        assert!(t.contains("DEFAULT '('") && t.ends_with(" STRICT"), "{t}");
        // The word "strict" in a name or a default is not the STRICT option.
        let t = translate_ddl(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, strictness INTEGER, s TEXT DEFAULT 'strict')",
        );
        assert!(t.ends_with(" STRICT"), "{t}");
        // Table-level PRIMARY KEY on a TEXT column becomes NOT NULL.
        let o = oracle();
        assert_eq!(
            o.exec("CREATE TABLE u (k TEXT, n INTEGER, PRIMARY KEY (k))"),
            OracleOutcome::Affected(0)
        );
        assert_eq!(
            o.exec("INSERT INTO u (k, n) VALUES (NULL, 1)"),
            OracleOutcome::Error(ErrClass::Constraint)
        );
        // Existing table options keep working next to STRICT.
        assert_eq!(
            o.exec("CREATE TABLE w (k TEXT PRIMARY KEY) WITHOUT ROWID"),
            OracleOutcome::Affected(0)
        );
        // End to end: the quoted default really is applied.
        o.exec("CREATE TABLE d (id INTEGER PRIMARY KEY, s TEXT DEFAULT 'a,b')");
        o.exec("INSERT INTO d (id) VALUES (1)");
        match o.exec("SELECT s FROM d") {
            OracleOutcome::Rows { rows, .. } => {
                assert_eq!(rows, vec![vec![Value::Text("a,b".into())]])
            }
            other => panic!("{other:?}"),
        }
    }

    /// Multi-byte text inside a quoted default must not shift the mask: the
    /// opening/closing paren offsets are computed on the masked copy and
    /// used to slice the original, so a mask that changed byte lengths
    /// either panicked or produced a corrupted statement.
    #[test]
    fn ddl_translation_survives_multibyte_quoted_text() {
        let t = translate_ddl("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT DEFAULT 'éé')");
        assert!(t.ends_with(" STRICT"), "{t}");
        assert!(t.contains("'éé'"), "{t}");
        let o = oracle();
        assert_eq!(o.exec(&t), OracleOutcome::Affected(0));
        o.exec("INSERT INTO t (id) VALUES (1)");
        match o.exec("SELECT s FROM t") {
            OracleOutcome::Rows { rows, .. } => {
                assert_eq!(rows, vec![vec![Value::Text("éé".into())]])
            }
            other => panic!("{other:?}"),
        }
    }

    /// Ticket 13: SQLite has no `IF [NOT] EXISTS` on ALTER; the oracle
    /// emulates it from the live schema (no-op or plain statement), and the
    /// ALTER error classes are mapped.
    #[test]
    fn alter_if_not_exists_is_emulated_from_the_schema() {
        let o = Oracle::new().unwrap();
        assert!(matches!(
            o.exec("CREATE TABLE t (a INTEGER)"),
            OracleOutcome::Affected(0)
        ));
        assert!(matches!(
            o.exec("INSERT INTO t VALUES (1)"),
            OracleOutcome::Affected(1)
        ));
        // ADD COLUMN without IF NOT EXISTS: a real column.
        assert!(matches!(
            o.exec("ALTER TABLE t ADD COLUMN b TEXT DEFAULT 'x'"),
            OracleOutcome::Affected(0)
        ));
        // Existing rows read the default.
        let seen = o.exec("SELECT b FROM t");
        match seen {
            OracleOutcome::Rows { rows, .. } => {
                assert_eq!(rows, vec![vec!(Value::Text("x".to_string()))]);
            }
            other => panic!("expected the default in the new column: {other:?}"),
        }
        // IF NOT EXISTS on the live column is a no-op; on a missing one it
        // adds the column.
        assert!(matches!(
            o.exec("ALTER TABLE t ADD COLUMN IF NOT EXISTS b INTEGER"),
            OracleOutcome::Affected(0)
        ));
        assert!(matches!(
            o.exec("ALTER TABLE t ADD COLUMN IF NOT EXISTS c INTEGER"),
            OracleOutcome::Affected(0)
        ));
        // A duplicate add without IF is `exists`.
        assert!(matches!(
            o.exec("ALTER TABLE t ADD COLUMN c INTEGER"),
            OracleOutcome::Error(ErrClass::Exists)
        ));
        // DROP IF EXISTS: a no-op for a missing column, plain otherwise.
        assert!(matches!(
            o.exec("ALTER TABLE t DROP COLUMN IF EXISTS zz"),
            OracleOutcome::Affected(0)
        ));
        assert!(matches!(
            o.exec("ALTER TABLE t DROP COLUMN c"),
            OracleOutcome::Affected(0)
        ));
        // A missing table is still `no_table`, IF or not.
        assert!(matches!(
            o.exec("ALTER TABLE nope ADD COLUMN IF NOT EXISTS c INTEGER"),
            OracleOutcome::Error(ErrClass::NoTable)
        ));
        // Renames; a taken name is `exists`.
        assert!(matches!(
            o.exec("ALTER TABLE t RENAME COLUMN b TO b2"),
            OracleOutcome::Affected(0)
        ));
        assert!(matches!(
            o.exec("ALTER TABLE t RENAME TO t2"),
            OracleOutcome::Affected(0)
        ));
        assert!(matches!(
            o.exec("CREATE TABLE t3 (x INTEGER)"),
            OracleOutcome::Affected(0)
        ));
        assert!(matches!(
            o.exec("ALTER TABLE t2 RENAME TO t3"),
            OracleOutcome::Error(ErrClass::Exists)
        ));
        // Dropping an indexed column is `constraint`.
        assert!(matches!(
            o.exec("CREATE INDEX ix ON t2 (b2)"),
            OracleOutcome::Affected(0)
        ));
        assert!(matches!(
            o.exec("ALTER TABLE t2 DROP COLUMN b2"),
            OracleOutcome::Error(ErrClass::Constraint)
        ));
    }

    /// ALTER TABLE on a STRICT table compiles a nested quick_check SELECT,
    /// so the statement reports a result column; it is still DDL (`OK 0`).
    #[test]
    fn alter_table_on_strict_table_is_ddl() {
        let o = oracle();
        o.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
        o.exec("INSERT INTO t (n) VALUES (1)");
        assert_eq!(
            o.exec("ALTER TABLE t ADD COLUMN extra INTEGER"),
            OracleOutcome::Affected(0)
        );
        match o.exec("SELECT extra FROM t") {
            OracleOutcome::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Null]]),
            other => panic!("{other:?}"),
        }
    }
}
