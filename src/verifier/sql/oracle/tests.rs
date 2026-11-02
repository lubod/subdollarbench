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
        // PK and UNIQUE columns: named refusals, same contract class as
        // an indexed drop (round-15 H1).
        ("ALTER TABLE t DROP COLUMN id", ErrClass::Constraint),
        ("ALTER TABLE t DROP COLUMN n", ErrClass::Constraint),
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
