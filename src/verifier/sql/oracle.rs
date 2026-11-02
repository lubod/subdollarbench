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
    if starts("cannot drop column")
        || contains("after drop column")
        || contains("cannot drop PRIMARY KEY column")
        || contains("cannot drop UNIQUE column")
        || contains("cannot drop unique column")
    {
        // Indexed/PK/UNIQUE column drops: the dialect refuses with
        // `constraint`. SQLite names the reason ("cannot drop PRIMARY KEY
        // column", "cannot drop UNIQUE column"); both are the same contract
        // class as an indexed drop.
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
mod tests;
