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
        assert!(
            script.unknown.is_empty(),
            "seed {seed}: unmapped oracle messages: {:?}",
            script.unknown
        );
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
    let (h, r) =
        rows(t.exec("SELECT j1.x AS k, j2.z AS v FROM j1 INNER JOIN j2 ON j1.x = j2.x ORDER BY k"));
    assert_eq!(h, vec!["k", "v"]);
    assert_eq!(r.len(), 2);
    // Comma join + WHERE, qualified by the table names.
    let (_, r) = rows(t.exec("SELECT j1.y AS a FROM j1, j2 WHERE j1.x = j2.x"));
    assert_eq!(r.len(), 2);
    // LEFT JOIN pads the missing right side with NULL.
    let (_, r) =
        rows(t.exec("SELECT j1.x AS k, j2.x AS m FROM j1 LEFT JOIN j2 ON j1.x = j2.x ORDER BY k"));
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
    let (_, r) = rows(
        t.exec("SELECT j1.x = (j2.x % 48) AS e0 FROM j1 INNER JOIN j2 ON j1.x = j2.z ORDER BY e0"),
    );
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
