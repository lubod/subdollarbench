// Unit tests for the sdb engine.  These drive the same `process_line`
// entry point the TCP server uses, so parser, planner, executor, storage,
// transactions and recovery are all exercised in-process.  Everything is
// deterministic and offline: each test owns a private temporary store.

use super::*;
use std::path::{Path, PathBuf};
use std::cmp::Ordering;
use std::sync::atomic::AtomicU64;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "sdb-test-{}-{}-{}",
        std::process::id(),
        tag,
        n
    ));
    let _ = fs::remove_dir_all(&p);
    p
}

struct Env {
    dir: PathBuf,
    db: Arc<Mutex<Db>>,
    keep: bool,
}

impl Env {
    fn new(tag: &str) -> Env {
        let dir = temp_dir(tag);
        let db = Arc::new(Mutex::new(Db::open(dir.to_str().unwrap())));
        Env { dir, db, keep: false }
    }

    fn on(&self, conn: u64, sql: &str) -> String {
        process_line(sql, &self.db, conn).unwrap_or_default()
    }

    fn exec(&self, sql: &str) -> String {
        self.on(1, sql)
    }

    fn ok(&self, sql: &str, n: &str) {
        let r = self.exec(sql);
        assert_eq!(r.trim_end(), format!("OK {}", n), "sql: {sql}");
    }

    fn err(&self, sql: &str, class: &str) {
        let r = self.exec(sql);
        assert!(
            r.starts_with(&format!("ERR {} ", class)),
            "sql: {sql}\nresp: {r}"
        );
    }

    fn rows(&self, sql: &str) -> Vec<Vec<String>> {
        rows_of(&self.exec(sql))
    }

    fn header(&self, sql: &str) -> Vec<String> {
        header_of(&self.exec(sql))
    }
}

impl Env {
    /// Keep the store on disk so a later block can reopen it.
    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

fn rows_of(resp: &str) -> Vec<Vec<String>> {
    let mut it = resp.lines();
    let head = it.next().unwrap_or("");
    assert!(head.starts_with("ROWS "), "expected ROWS, got {resp:?}");
    let _ = it.next();
    let mut out = Vec::new();
    loop {
        let line = match it.next() {
            Some(l) => l,
            None => panic!("missing END in {resp:?}"),
        };
        if let Some(n) = line.strip_prefix("END ") {
            let n: usize = n.trim().parse().unwrap();
            assert_eq!(n, out.len(), "END count mismatch in {resp:?}");
            break;
        }
        out.push(line.split('\t').map(|s| s.to_string()).collect());
    }
    out
}

fn header_of(resp: &str) -> Vec<String> {
    let mut it = resp.lines();
    let head = it.next().unwrap_or("");
    assert!(head.starts_with("ROWS "), "expected ROWS, got {resp:?}");
    let h = it.next().unwrap_or("");
    h.split('\t').map(|s| s.to_string()).collect()
}

fn flat(rows: Vec<Vec<String>>) -> Vec<String> {
    rows.into_iter()
        .map(|r| r.join("|"))
        .collect::<Vec<_>>()
}

// ------------------------------------------------------- initial coverage ---

#[test]
fn lexer_accepts_every_token_shape() {
    let toks = lex("a_1 B2 42 3.5 .5 1e3 2E-2 'it''s' \"Quoted\" ( ) , ; * . + - / % = || <= <> < > >= != -- tail")
        .unwrap();
    let kinds: Vec<String> = toks
        .iter()
        .map(|t| match &t.tk {
            Tk::Ident(s) => format!("i:{s}"),
            Tk::Int(v) => format!("n:{v}"),
            Tk::Real(v) => format!("r:{v}"),
            Tk::Str(s) => format!("s:{s}"),
            Tk::P(c) => format!("p:{c}"),
        })
        .collect();
    assert!(kinds.contains(&"i:a_1".to_string()));
    assert!(kinds.contains(&"i:b2".to_string()));
    assert!(kinds.contains(&"n:42".to_string()));
    assert!(kinds.contains(&"r:3.5".to_string()));
    assert!(kinds.contains(&"r:0.5".to_string()));
    assert!(kinds.contains(&"r:1000".to_string()));
    assert!(kinds.contains(&"s:it's".to_string()));
    assert!(kinds.contains(&"i:quoted".to_string()));
    assert!(kinds.contains(&"p:L".to_string()));
    assert!(kinds.contains(&"p:!".to_string()));
    assert!(kinds.iter().any(|k| k == "p:|"));
    // comment runs to the end of the line
    assert!(lex("SELECT 1 -- x").unwrap().len() == 2);
    assert!(lex("   ").unwrap().is_empty());
}

#[test]
fn lexer_reports_bad_tokens() {
    assert_eq!(lex("'unterminated").unwrap_err().class, "syntax");
    assert_eq!(lex("\"unterminated").unwrap_err().class, "syntax");
    assert_eq!(lex("select @").unwrap_err().class, "syntax");
    assert_eq!(lex("999999999999999999999999999").unwrap().len(), 1);
}

#[test]
fn value_formatting_helpers() {
    let mut s = String::new();
    escape_text("a\\b\tc\nd\re", &mut s);
    assert_eq!(s, "a\\\\b\\tc\\nd\re");
    let mut o = String::new();
    push_value(&Value::Null, &mut o);
    o.push('|');
    push_value(&Value::Int(-7), &mut o);
    o.push('|');
    push_value(&Value::Real(1.25), &mut o);
    o.push('|');
    push_value(&Value::Text("x\ty".into()), &mut o);
    assert_eq!(o, "\\N|-7|1.25|x\\ty");
    assert_eq!(itoa(0), "0");
    assert_eq!(fmt_real(1.5), "1.5");
    assert_eq!(fmt_real(2.0), "2.0");
    assert!(matches!(key_of(&Value::Text("k".into())), Some(Key::T(_))));
    assert!(key_of(&Value::Null).is_none());
    let mut t = String::new();
    push_text(&Value::Null, &mut t);
    push_text(&Value::Int(3), &mut t);
    push_text(&Value::Real(2.5), &mut t);
    push_text(&Value::Text("z".into()), &mut t);
    assert_eq!(t, "32.5z");
    assert_eq!(num_coerce(&Value::Text(" 12 ".into())), Value::Int(12));
    assert_eq!(num_coerce(&Value::Text("1.5".into())), Value::Real(1.5));
    assert_eq!(num_coerce(&Value::Text("zz".into())), Value::Int(0));
    assert_eq!(as_f64(&Value::Null), None);
    assert_eq!(as_f64(&Value::Text("2.5".into())), Some(2.5));
    assert_eq!(neg_value(Value::Int(4)), Value::Int(-4));
    assert!(truthy(&Value::Int(1)));
    assert!(!truthy(&Value::Null));
    assert_eq!(as_tribool(&Value::Int(0)), Some(false));
    assert_eq!(bool_val(None), Value::Null);
    assert_eq!(bool_val(Some(true)), Value::Int(1));
    assert_eq!(sort_cmp(&Value::Null, &Value::Int(1)), Ordering::Less);
    assert_eq!(
        cmp_values(&Value::Text("a".into()), &Value::Text("b".into())),
        Some(Ordering::Less)
    );
    assert_eq!(cmp_values(&Value::Int(1), &Value::Text("x".into())), Some(Ordering::Less));
    assert_eq!(cmp_values(&Value::Text("x".into()), &Value::Int(1)), Some(Ordering::Greater));
    assert_eq!(cmp_values(&Value::Int(1), &Value::Real(1.5)), Some(Ordering::Less));
    assert_eq!(cmp_values(&Value::Real(1.5), &Value::Int(1)), Some(Ordering::Greater));
    assert_eq!(cmp_values(&Value::Real(1.0), &Value::Real(2.0)), Some(Ordering::Less));
    assert_eq!(flip_op(Op::Lt), Op::Gt);
    assert_eq!(flip_op(Op::Eq), Op::Eq);
    assert_eq!(flip_op(Op::Ne), Op::Ne);
    assert_eq!(AggKind::Count.label(), "COUNT");
    assert_eq!(Ty::from_name("integer"), Some(Ty::Int));
    assert_eq!(Ty::from_name("text"), Some(Ty::Text));
    assert_eq!(Ty::from_name("zzz"), None);
    assert_eq!(Ty::Int.code(), "i");
    assert_eq!(Ty::from_code("t"), Some(Ty::Text));
    assert!(is_reserved("select"));
    assert!(!is_reserved("customer"));
    assert_eq!(wal_capacity(1), 1024 * 1024);
    assert_eq!(wal_capacity(2 * 1024 * 1024), 4 * 1024 * 1024);
    let mut e = String::new();
    esc_field("a\tb\\c", &mut e);
    assert_eq!(e, "a\\tb\\\\c");
    assert_eq!(unesc_field(&e), "a\tb\\c");
    assert_eq!(unesc_field("a\\rb"), "a\rb");
    assert_eq!(unesc_field("bad\\"), "bad");
    let mut enc = String::new();
    enc_value(&Value::Null, &mut enc);
    assert_eq!(enc, "N");
    let mut enc2 = String::new();
    enc_value(&Value::Text("q".into()), &mut enc2);
    assert_eq!(dec_value(&enc2), Some(Value::Text("q".into())));
    let mut enc3 = String::new();
    enc_value(&Value::Int(9), &mut enc3);
    assert_eq!(dec_value(&enc3), Some(Value::Int(9)));
    assert_eq!(dec_value("N"), Some(Value::Null));
    assert_eq!(dec_value("r1.5"), Some(Value::Real(1.5)));
    assert_eq!(dec_value("Z"), None);
}

#[test]
fn core_create_insert_select_and_errors() {
    let e = Env::new("core");
    e.ok("CREATE TABLE t (id INTEGER, s TEXT)", "0");
    e.err("CREATE TABLE t (id INTEGER)", "exists");
    e.ok("insert into T values (1, 'a'), (2, NULL)", "2");
    assert_eq!(flat(e.rows("SELECT * FROM t")), vec!["1|a", "2|\\N"]);
    assert_eq!(e.header("SELECT id FROM t"), vec!["id"]);
    e.err("SELECT * FROM nope", "no_table");
    e.err("SELECT zz FROM t", "no_column");
    e.err("INSERT INTO t VALUES (1)", "arity");
    e.err("INSERT INTO t VALUES ('zz', 'x')", "type");
    e.err("INSERT INTO t (id, zz) VALUES (1, 2)", "no_column");
    e.err("SELEC 1", "syntax");
    e.err("SELECT 1 FROM", "syntax");
    e.err("SELECT FROM t", "syntax");
}

#[test]
fn process_line_ignores_blank_and_rejects_multi() {
    let e = Env::new("proto");
    assert!(process_line("", &e.db, 1).is_none());
    assert!(process_line("  \t ", &e.db, 1).is_none());
    assert_eq!(process_line(";", &e.db, 1).unwrap(), "OK 0\n");
    let r = process_line("SELECT 1; SELECT 2", &e.db, 1).unwrap();
    assert!(r.starts_with("ERR syntax"), "{r}");
}

#[test]
fn expressions_functions_and_three_valued_logic() {
    let e = Env::new("expr");
    let r = e.rows("SELECT 1+2, 7/2, 7%2, -3, 'a' || 'b', 'x' = 'x', 1 <> 2, NULL IS NULL");
    assert_eq!(flat(r), vec!["3|3|1|-3|ab|1|1|1"]);
    assert_eq!(e.header("SELECT 1+2 AS three"), vec!["three"]);
    e.ok("CREATE TABLE t (id INTEGER, s TEXT)", "0");
    e.ok("INSERT INTO t VALUES (1, 'Ab'), (2, 'cd'), (3, NULL)", "3");
    assert_eq!(
        flat(e.rows("SELECT id FROM t WHERE s IS NULL")),
        vec!["3"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM t WHERE s IS NOT NULL")),
        vec!["1", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT upper(s), lower(s), length(s), abs(id-10) FROM t WHERE id=1")),
        vec!["AB|ab|2|9"]
    );
    assert_eq!(
        flat(e.rows("SELECT coalesce(s, 'none') FROM t WHERE id=3")),
        vec!["none"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM t WHERE id IN (1, 3)")),
        vec!["1", "3"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM t WHERE id NOT IN (1, 3)")),
        vec!["2"]
    );
    // NULL three valued logic
    assert_eq!(flat(e.rows("SELECT id FROM t WHERE NULL OR id=2")), vec!["2"]);
    assert_eq!(flat(e.rows("SELECT id FROM t WHERE NOT NULL")), Vec::<String>::new());
    assert_eq!(
        flat(e.rows("SELECT 1 WHERE NULL IN (1, 2)")),
        Vec::<String>::new()
    );
    assert_eq!(
        flat(e.rows("SELECT 1 WHERE 5 IN (1, NULL)")),
        Vec::<String>::new()
    );
    assert_eq!(flat(e.rows("SELECT 1 WHERE 1 IN (1, NULL)")), vec!["1"]);
    assert_eq!(flat(e.rows("SELECT 1 WHERE 1 IN ()")), Vec::<String>::new());
    e.err("SELECT * FROM t WHERE COUNT(*) > 1", "syntax");
    e.err("SELECT 1 + ", "syntax");
    assert_eq!(
        flat(e.rows("SELECT 1/0, 1%0, 0/0.0")),
        vec!["\\N|\\N|\\N"]
    );
    assert_eq!(flat(e.rows("SELECT 1.5 + 2.5, 7.0 / 2")), vec!["4.0|3.5"]);
    assert_eq!(
        flat(e.rows("SELECT '12' + 1, 'x' + 1, abs(-2.5), abs('3')")),
        vec!["13|1|2.5|3.0"]
    );
    assert_eq!(
        flat(e.rows("SELECT true, false, NULL IS NOT NULL, 1 < 2, 2 <= 2, 3 > 2, 3 >= 4")),
        vec!["1|0|0|1|1|1|0"]
    );
    e.err("SELECT 1 WHERE 1 = 1 AND", "syntax");
    e.err("SELECT 1 WHERE 1 = 1 OR", "syntax");
    e.err("SELECT 1 WHERE NOT", "syntax");
    e.err("SELECT (1", "syntax");
}

#[test]
fn order_by_limit_distinct_update_delete() {
    let e = Env::new("srt");
    e.ok("CREATE TABLE t (id INTEGER, s TEXT)", "0");
    e.ok("INSERT INTO t VALUES (3, 'c'), (1, 'a'), (2, NULL), (4, 'a')", "4");
    assert_eq!(
        flat(e.rows("SELECT id FROM t ORDER BY id DESC")),
        vec!["4", "3", "2", "1"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM t ORDER BY s, id")),
        vec!["2", "1", "4", "3"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM t ORDER BY s DESC, id")),
        vec!["3", "1", "4", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1")),
        vec!["2", "3"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM t ORDER BY id LIMIT 1, 2")),
        vec!["2", "3"]
    );
    assert_eq!(
        flat(e.rows("SELECT DISTINCT s FROM t ORDER BY s")),
        vec!["\\N", "a", "c"]
    );
    e.ok("UPDATE t SET s = 'z' WHERE id = 2", "1");
    e.ok("UPDATE t SET s = 'q'", "4");
    assert_eq!(flat(e.rows("SELECT DISTINCT s FROM t")), vec!["q"]);
    e.ok("DELETE FROM t WHERE id = 3", "1");
    e.ok("DELETE FROM t", "3");
    assert_eq!(e.rows("SELECT * FROM t").len(), 0);
    e.err("UPDATE t SET nope = 1", "no_column");
    e.err("UPDATE nope SET a = 1", "no_table");
    e.err("DELETE FROM nope", "no_table");
    e.err("SELECT * FROM t LIMIT -1", "syntax");
    // statement atomicity of a multi-row insert
    e.err("INSERT INTO t VALUES (1, 'a'), ('bad', 'b')", "type");
    assert_eq!(e.rows("SELECT * FROM t").len(), 0);
    // column-list insert
    e.ok("INSERT INTO t (s, id) VALUES ('x', 9)", "1");
    assert_eq!(flat(e.rows("SELECT * FROM t")), vec!["9|x"]);
}

#[test]
fn aggregates_scalar_grouped_and_having() {
    let e = Env::new("agg");
    e.ok("CREATE TABLE t (g TEXT, n INTEGER, m INTEGER)", "0");
    e.ok(
        "INSERT INTO t VALUES ('a', 1, 10), ('a', 2, 20), ('b', 3, NULL), ('b', NULL, 40), (NULL, 5, 50)",
        "5",
    );
    assert_eq!(
        flat(e.rows("SELECT COUNT(*), COUNT(n), SUM(n), MIN(n), MAX(n) FROM t")),
        vec!["5|4|11|1|5"]
    );
    assert_eq!(
        e.header("SELECT COUNT(*), SUM(n) FROM t"),
        vec!["COUNT(*)", "SUM(n)"]
    );
    assert_eq!(
        flat(e.rows("SELECT COUNT(*), SUM(n), MIN(n), MAX(n) FROM t WHERE n > 99")),
        vec!["0|\\N|\\N|\\N"]
    );
    assert_eq!(
        flat(e.rows("SELECT g, COUNT(*), SUM(n), MIN(n), MAX(n) FROM t GROUP BY g ORDER BY g")),
        vec!["\\N|1|5|5|5", "a|2|3|1|2", "b|2|3|3|3"]
    );
    assert_eq!(
        flat(e.rows("SELECT upper(g) AS u, COUNT(*) FROM t GROUP BY upper(g) ORDER BY u")),
        vec!["\\N|1", "A|2", "B|2"]
    );
    assert_eq!(
        flat(e.rows("SELECT n + 1 AS k, COUNT(*) FROM t GROUP BY n + 1 ORDER BY k")),
        vec!["\\N|1", "2|1", "3|1", "4|1", "6|1"]
    );
    assert_eq!(
        flat(e.rows("SELECT g, COUNT(*) FROM t GROUP BY g HAVING COUNT(*) > 1 ORDER BY g")),
        vec!["a|2", "b|2"]
    );
    assert_eq!(
        flat(e.rows("SELECT g, SUM(n) AS s FROM t GROUP BY g HAVING SUM(n) > 3 ORDER BY s")),
        vec!["\\N|5"]
    );
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM t HAVING COUNT(*) > 100")),
        Vec::<String>::new()
    );
    assert_eq!(
        flat(e.rows("SELECT MIN(g), MAX(g) FROM t")),
        vec!["a|b"]
    );
    e.err("SELECT SUM(*) FROM t", "arity");
    e.err("SELECT COUNT(1, 2) FROM t", "arity");
    e.err("SELECT SUM(zz) FROM t", "no_column");
    e.err("SELECT COUNT(*) FROM t GROUP BY zz", "no_column");
    e.err("SELECT COUNT(*) FROM t WHERE COUNT(*) > 1", "syntax");
    e.err("SELECT COUNT(*) FROM t GROUP BY COUNT(*)", "syntax");
    // aggregates in ORDER BY and DISTINCT
    assert_eq!(
        flat(e.rows("SELECT g, COUNT(*) FROM t GROUP BY g ORDER BY COUNT(*) DESC, g")),
        vec!["a|2", "b|2", "\\N|1"]
    );
    assert_eq!(
        flat(e.rows("SELECT DISTINCT COUNT(*) FROM t GROUP BY g ORDER BY 1")),
        vec!["1", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) AS c, g FROM t GROUP BY g HAVING c > 1 ORDER BY g")),
        vec!["2|a", "2|b"]
    );
}

#[test]
fn joins_inner_left_comma_and_composition() {
    let e = Env::new("join");
    e.ok("CREATE TABLE a (id INTEGER, v TEXT)", "0");
    e.ok("CREATE TABLE b (id INTEGER, aid INTEGER, w TEXT)", "0");
    e.ok("CREATE TABLE c (bid INTEGER, z INTEGER)", "0");
    e.ok("INSERT INTO a VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')", "3");
    e.ok("INSERT INTO b VALUES (10, 1, 'b1'), (11, 1, 'b2'), (12, 2, 'b3')", "3");
    e.ok("INSERT INTO c VALUES (10, 100), (12, 300)", "2");
    assert_eq!(
        flat(e.rows("SELECT a.id, b.w FROM a, b WHERE a.id = b.aid ORDER BY b.id")),
        vec!["1|b1", "1|b2", "2|b3"]
    );
    assert_eq!(
        flat(e.rows("SELECT a.id, b.w FROM a INNER JOIN b ON a.id = b.aid ORDER BY b.id")),
        vec!["1|b1", "1|b2", "2|b3"]
    );
    assert_eq!(
        flat(e.rows("SELECT a.id, b.id FROM a LEFT JOIN b ON a.id = b.aid ORDER BY a.id, b.id")),
        vec!["1|10", "1|11", "2|12", "3|\\N"]
    );
    assert_eq!(
        flat(e.rows("SELECT a.id, b.w, c.z FROM a LEFT OUTER JOIN b ON a.id = b.aid LEFT JOIN c ON b.id = c.bid ORDER BY a.id, c.z")),
        vec!["1|b2|\\N", "1|b1|100", "2|b3|300", "3|\\N|\\N"]
    );
    assert_eq!(
        flat(e.rows("SELECT * FROM a, b WHERE a.id = b.aid AND b.id = 10")),
        vec!["1|a1|10|1|b1"]
    );
    assert_eq!(
        e.header("SELECT * FROM a, b"),
        vec!["id", "v", "id", "aid", "w"]
    );
    assert_eq!(
        flat(e.rows("SELECT a.id FROM a CROSS JOIN b WHERE b.id = 10 AND a.id = 1")),
        vec!["1"]
    );
    // aliases and self join
    assert_eq!(
        flat(e.rows("SELECT x.id, y.id FROM a x, a y WHERE x.id < y.id ORDER BY x.id, y.id")),
        vec!["1|2", "1|3", "2|3"]
    );
    e.err("SELECT id FROM a, b", "ambiguous");
    e.err("SELECT z.id FROM a", "no_column");
    e.err("SELECT * FROM a JOIN b ON z.id = 1", "no_column");
    e.err("SELECT * FROM a JOIN nope ON 1 = 1", "no_table");
    e.err("SELECT * FROM a JOIN b ON a.zz = b.id", "no_column");
    // ordering, filtering, distinct, limit over joins
    assert_eq!(
        flat(e.rows("SELECT DISTINCT a.id FROM a, b WHERE a.id <= b.aid ORDER BY a.id")),
        vec!["1", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT b.w FROM a, b WHERE a.id = b.aid ORDER BY b.w DESC LIMIT 2")),
        vec!["b3", "b2"]
    );
    // aggregate over a join
    assert_eq!(
        flat(e.rows("SELECT a.v, COUNT(*) FROM a, b WHERE a.id = b.aid GROUP BY a.v ORDER BY a.v")),
        vec!["a1|2", "a2|1"]
    );
    // USING
    assert_eq!(
        flat(e.rows("SELECT a.id, b.w FROM a JOIN b USING (id)")),
        Vec::<String>::new()
    );
    assert_eq!(
        flat(e.rows("SELECT b.id, b.w FROM b JOIN a ON a.id = b.aid WHERE b.id = 12")),
        vec!["12|b3"]
    );
    // unqualified column resolves when unique
    assert_eq!(
        flat(e.rows("SELECT w FROM a, b WHERE a.id = b.aid ORDER BY w")),
        vec!["b1", "b2", "b3"]
    );
}

#[test]
fn constraints_primary_unique_notnull_default_and_fk() {
    let e = Env::new("cons");
    e.ok(
        "CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT UNIQUE, req TEXT NOT NULL DEFAULT 'd')",
        "0",
    );
    e.ok("INSERT INTO p (code) VALUES ('a')", "1");
    e.ok("INSERT INTO p (id, code, req) VALUES (NULL, 'b', 'x')", "1");
    e.err("INSERT INTO p (id, code, req) VALUES (7, NULL, NULL)", "constraint");
    e.ok("INSERT INTO p (code, req) VALUES ('b2', 'y')", "1");
    assert_eq!(
        flat(e.rows("SELECT id, code, req FROM p ORDER BY id")),
        vec!["1|a|d", "2|b|x", "3|b2|y"]
    );
    e.err("INSERT INTO p (code) VALUES ('a')", "constraint");
    e.err("INSERT INTO p (code, req) VALUES ('c', NULL)", "constraint");
    // text primary key rejects null
    e.ok("CREATE TABLE tk (k TEXT PRIMARY KEY, v INTEGER)", "0");
    e.err("INSERT INTO tk VALUES (NULL, 1)", "constraint");
    e.ok("INSERT INTO tk VALUES ('k', 1)", "1");
    e.err("INSERT INTO tk VALUES ('k', 2)", "constraint");
    // defaults only apply on omission
    e.ok("CREATE TABLE d (id INTEGER, a INTEGER DEFAULT 5, b TEXT DEFAULT 'z', c INTEGER)", "0");
    e.ok("INSERT INTO d (id, c) VALUES (1, 9)", "1");
    e.ok("INSERT INTO d VALUES (2, NULL, NULL, 0)", "1");
    assert_eq!(
        flat(e.rows("SELECT a, b, c FROM d ORDER BY id")),
        vec!["5|z|9", "\\N|\\N|0"]
    );
    // foreign keys are enforced in both directions
    e.ok(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id), note TEXT)",
        "0",
    );
    e.ok("INSERT INTO child (pid, note) VALUES (1, 'ok'), (2, 'ok2'), (NULL, 'none')", "3");
    e.err("INSERT INTO child (pid) VALUES (99)", "constraint");
    e.err("DELETE FROM p WHERE id = 1", "constraint");
    e.ok("DELETE FROM child WHERE pid = 1", "1");
    e.ok("DELETE FROM p WHERE id = 1", "1");
    // updating a parent key breaks the child link
    e.err("UPDATE p SET id = 5 WHERE id = 2", "constraint");
    // update violations are atomic
    e.err("UPDATE p SET code = 'b2' WHERE id = 2", "constraint");
    assert_eq!(flat(e.rows("SELECT id, code FROM p ORDER BY id")), vec!["2|b", "3|b2"]);
    e.err("UPDATE p SET req = NULL WHERE id = 2", "constraint");
    // table level constraints
    e.ok(
        "CREATE TABLE tp (a INTEGER, b INTEGER, UNIQUE (a, b), PRIMARY KEY (a))",
        "0",
    );
    e.ok("INSERT INTO tp VALUES (1, 1), (2, 1)", "2");
    e.err("INSERT INTO tp VALUES (1, 2)", "constraint");
    e.err("INSERT INTO tp VALUES (3, 1), (3, 1)", "constraint");
    e.ok(
        "CREATE TABLE tc (x INTEGER, y INTEGER, FOREIGN KEY (y) REFERENCES tp(a))",
        "0",
    );
    e.err("INSERT INTO tc VALUES (1, 99)", "constraint");
    e.ok("INSERT INTO tc VALUES (1, 1)", "1");
    e.err("DELETE FROM tp WHERE a = 1", "constraint");
}

#[test]
fn subqueries_scalar_in_exists_and_insert_select() {
    let e = Env::new("sub");
    e.ok("CREATE TABLE a (id INTEGER, v INTEGER)", "0");
    e.ok("CREATE TABLE b (id INTEGER, w TEXT)", "0");
    e.ok("CREATE TABLE out (id INTEGER, note TEXT)", "0");
    e.ok("INSERT INTO a VALUES (1, 10), (2, 20), (3, 30), (4, NULL)", "4");
    e.ok("INSERT INTO b VALUES (1, 'x'), (2, 'y')", "2");
    assert_eq!(
        flat(e.rows("SELECT (SELECT COUNT(*) FROM a), (SELECT SUM(v) FROM a)")),
        vec!["4|60"]
    );
    assert_eq!(flat(e.rows("SELECT (SELECT MAX(v) FROM a WHERE v > 99)")), vec!["\\N"]);
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE id IN (SELECT id FROM b) ORDER BY id")),
        vec!["1", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE id NOT IN (SELECT id FROM b) ORDER BY id")),
        vec!["3", "4"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.id = a.id) ORDER BY id")),
        vec!["1", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.id = a.id) ORDER BY id")),
        vec!["3", "4"]
    );
    assert_eq!(
        flat(e.rows("SELECT id, (SELECT w FROM b WHERE b.id = a.id) FROM a ORDER BY id")),
        vec!["1|x", "2|y", "3|\\N", "4|\\N"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE v = (SELECT MIN(v) FROM a)")),
        vec!["1"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE id IN (1, NULL) ORDER BY id")),
        vec!["1"]
    );
    e.err("SELECT (SELECT id, v FROM a)", "arity");
    e.err("SELECT (SELECT zz FROM a)", "no_column");
    e.err("SELECT id FROM a WHERE id IN (SELECT id FROM nope)", "no_table");
    e.err("SELECT * FROM nope WHERE id IN (SELECT id FROM a)", "no_table");
    e.ok("INSERT INTO out SELECT id, w FROM b", "2");
    assert_eq!(flat(e.rows("SELECT * FROM out ORDER BY id")), vec!["1|x", "2|y"]);
    e.err("INSERT INTO out SELECT id FROM b", "arity");
    // correlated aggregate subquery
    assert_eq!(
        flat(e.rows("SELECT id, (SELECT COUNT(*) FROM a x WHERE x.id <= a.id) FROM a ORDER BY id")),
        vec!["1|1", "2|2", "3|3", "4|4"]
    );
}

#[test]
fn indexes_survive_and_change_no_answer() {
    let e = Env::new("idx");
    e.ok("CREATE TABLE t (k INTEGER, v TEXT)", "0");
    e.ok("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (2, 'c'), (3, NULL)", "4");
    e.ok("CREATE INDEX t_k ON t (k)", "0");
    e.err("CREATE INDEX t_k ON t (k)", "exists");
    e.err("CREATE INDEX zz ON nope (k)", "no_table");
    e.err("CREATE INDEX zz ON t (nope)", "no_column");
    e.err("DROP INDEX nope", "no_table");
    e.ok("CREATE UNIQUE INDEX t_v ON t (v)", "0");
    e.err("INSERT INTO t VALUES (9, 'a')", "constraint");
    e.err("UPDATE t SET v = 'b' WHERE k = 1", "constraint");
    e.ok("INSERT INTO t VALUES (9, NULL)", "1");
    e.ok("INSERT INTO t VALUES (10, NULL)", "1");
    assert_eq!(
        flat(e.rows("SELECT v FROM t WHERE k = 2 ORDER BY v")),
        vec!["b", "c"]
    );
    let plan = e.rows("EXPLAIN SELECT * FROM t WHERE k = 2");
    assert!(plan[0][0].contains("INDEX t.t_k"), "{plan:?}");
    let plan = e.rows("EXPLAIN SELECT * FROM t WHERE k > 1");
    assert!(plan[0][0].contains("INDEX t.t_k"), "{plan:?}");
    let plan = e.rows("EXPLAIN SELECT * FROM t WHERE k > 1 ORDER BY k");
    assert!(plan[0][0].contains("INDEX t.t_k"), "{plan:?}");
    let plan = e.rows("EXPLAIN SELECT * FROM t WHERE v = 'a'");
    assert!(plan[0][0].contains("INDEX t.t_v"), "{plan:?}");
    let plan = e.rows("EXPLAIN SELECT * FROM t WHERE v = 'a' AND k = 1");
    assert!(plan[0][0].contains("INDEX t.t_v"), "{plan:?}");
    assert!(!plan.iter().any(|r| r[0].contains("SCAN t")), "{plan:?}");
    e.ok("DROP INDEX t_v", "0");
    e.err("DROP INDEX t_v", "no_table");
    e.ok("CREATE UNIQUE INDEX IF NOT EXISTS t_k ON t (k)", "0");
    // duplicate values make a unique index creation fail atomically
    e.err("CREATE UNIQUE INDEX t_k2 ON t (k)", "constraint");
}

#[test]
fn alter_table_add_drop_rename() {
    let e = Env::new("alter");
    e.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)", "0");
    e.ok("INSERT INTO t VALUES (1, 'x'), (2, 'y')", "2");
    e.ok("CREATE INDEX t_a ON t (a)", "0");
    e.ok("ALTER TABLE t ADD COLUMN n INTEGER DEFAULT 7", "0");
    assert_eq!(
        flat(e.rows("SELECT id, a, n FROM t ORDER BY id")),
        vec!["1|x|7", "2|y|7"]
    );
    e.err("ALTER TABLE t ADD COLUMN a TEXT", "exists");
    e.ok("ALTER TABLE t ADD COLUMN IF NOT EXISTS a TEXT", "0");
    e.err("ALTER TABLE nope ADD COLUMN z INTEGER", "no_table");
    e.ok("ALTER TABLE t ADD COLUMN z TEXT", "0");
    assert_eq!(flat(e.rows("SELECT z FROM t ORDER BY id")), vec!["\\N", "\\N"]);
    e.err("ALTER TABLE t DROP COLUMN id", "constraint");
    e.err("ALTER TABLE t DROP COLUMN a", "constraint");
    e.err("ALTER TABLE t DROP COLUMN nope", "no_column");
    e.ok("ALTER TABLE t DROP COLUMN IF EXISTS nope", "0");
    e.ok("ALTER TABLE t DROP COLUMN z", "0");
    e.err("SELECT z FROM t", "no_column");
    e.ok("ALTER TABLE t RENAME COLUMN a TO b", "0");
    assert_eq!(flat(e.rows("SELECT b FROM t ORDER BY id")), vec!["x", "y"]);
    assert!(e.rows("EXPLAIN SELECT * FROM t WHERE b = 'x'")[0][0].contains("INDEX t.t_a"));
    e.err("ALTER TABLE t RENAME COLUMN b TO id", "exists");
    e.err("ALTER TABLE t RENAME COLUMN nope TO q", "no_column");
    e.err("ALTER TABLE nope RENAME TO q", "no_table");
    e.ok("ALTER TABLE t RENAME TO u", "0");
    e.err("SELECT * FROM t", "no_table");
    assert_eq!(flat(e.rows("SELECT b FROM u ORDER BY b")), vec!["x", "y"]);
    e.ok("CREATE TABLE taken (a INTEGER)", "0");
    e.err("ALTER TABLE u RENAME TO taken", "exists");
    e.err("ALTER TABLE u RENAME TO u", "exists");
}

#[test]
fn explain_and_show_stats_shapes() {
    let e = Env::new("obs");
    e.ok("CREATE TABLE users (id INTEGER, email TEXT)", "0");
    e.ok("CREATE INDEX users_email ON users (email)", "0");
    let r = e.exec("EXPLAIN SELECT * FROM users WHERE email = 'a'");
    assert!(r.starts_with("ROWS 1\n"), "{r}");
    assert_eq!(e.header("EXPLAIN SELECT * FROM users WHERE email = 'a'"), vec!["plan"]);
    let plan = e.rows("EXPLAIN SELECT * FROM users WHERE id = 1");
    assert!(plan.iter().any(|x| x[0].contains("SCAN users")), "{plan:?}");
    assert!(!plan.iter().any(|x| x[0].contains("INDEX")), "{plan:?}");
    let plan = e.rows("EXPLAIN SELECT * FROM users WHERE email = 'a' AND id > 1");
    assert!(plan.iter().any(|x| x[0].contains("INDEX users.users_email")), "{plan:?}");
    // ORDER BY with a LIMIT plus range scans
    let plan = e.rows("EXPLAIN SELECT id FROM users WHERE id >= 2 AND id < 5 ORDER BY id LIMIT 1");
    assert!(!plan.is_empty());
    e.err("EXPLAIN SELECT * FROM nope", "no_table");
    let plan = e.rows("EXPLAIN INSERT INTO users VALUES (1, 'x')");
    assert!(plan.is_empty());
    let stats = e.rows("SHOW STATS");
    assert_eq!(stats.len(), 10);
    for row in &stats {
        assert_eq!(row.len(), 2);
        assert!(row[1].parse::<u64>().is_ok(), "{row:?}");
    }
    assert!(stats.iter().any(|r| r[0] == "statements"));
    assert!(stats.iter().any(|r| r[0] == "connections_current"));
    assert_eq!(
        e.header("SHOW STATS"),
        vec!["name".to_string(), "value".to_string()]
    );
    assert!(Stats::get(&STATS.statements) > 0);
    assert!(Stats::get(&STATS.inserts) > 0);
    e.err("SHOW NOPE", "syntax");
    // keyword accounting covers every counter kind
    let before = Stats::get(&STATS.selects);
    let _ = e.exec("SELECT 1");
    assert!(Stats::get(&STATS.selects) > before);
}

#[test]
fn transactions_commit_rollback_and_isolation() {
    let e = Env::new("txn");
    e.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", "0");
    e.ok("INSERT INTO t VALUES (1, 100)", "1");
    e.ok("BEGIN", "0");
    e.err("BEGIN", "txn");
    e.ok("INSERT INTO t VALUES (2, 200)", "1");
    // other connections still see only the committed image
    assert_eq!(flat(rows_of(&e.on(2, "SELECT v FROM t ORDER BY id"))), vec!["100"]);
    e.ok("ROLLBACK", "0");
    e.err("ROLLBACK", "txn");
    assert_eq!(flat(e.rows("SELECT v FROM t ORDER BY id")), vec!["100"]);
    e.ok("BEGIN TRANSACTION", "0");
    e.ok("UPDATE t SET v = 150 WHERE id = 1", "1");
    e.ok("COMMIT", "0");
    e.err("COMMIT", "txn");
    assert_eq!(flat(e.rows("SELECT v FROM t")), vec!["150"]);
    // connection close discards an open transaction
    e.ok("BEGIN", "0");
    e.ok("INSERT INTO t VALUES (3, 300)", "1");
    {
        let mut g = e.db.lock().unwrap();
        g.end_conn(1);
    }
    assert_eq!(flat(e.rows("SELECT COUNT(*) FROM t")), vec!["1"]);
    // commit with no writes is harmless
    e.ok("BEGIN", "0");
    e.ok("INSERT INTO t VALUES (4, 400)", "1");
    e.ok("COMMIT", "0");
    assert_eq!(flat(e.rows("SELECT COUNT(*) FROM t")), vec!["2"]);
}

#[test]
fn concurrent_increment_is_serializable() {
    let e = Env::new("conc");
    e.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", "0");
    e.ok("INSERT INTO t VALUES (1, 0)", "1");
    let db = Arc::clone(&e.db);
    let mut handles = Vec::new();
    for c in 0..4u64 {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            let conn = 10 + c;
            for _ in 0..8 {
                loop {
                    let _ = process_line("BEGIN", &db, conn);
                    let r = process_line("SELECT v FROM t WHERE id = 1", &db, conn).unwrap();
                    let v: i64 = rows_of(&r)[0][0].parse().unwrap();
                    let u = process_line(&format!("UPDATE t SET v = {} WHERE id = 1", v + 1), &db, conn)
                        .unwrap();
                    let c = process_line("COMMIT", &db, conn).unwrap();
                    if !u.starts_with("ERR") && !c.starts_with("ERR") {
                        break;
                    }
                    let _ = process_line("ROLLBACK", &db, conn);
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(flat(e.rows("SELECT v FROM t")), vec!["32"]);
}

#[test]
fn persistence_reopen_and_wal_recovery() {
    let dir = temp_dir("persist");
    {
        let db = Env::new_at_path(&dir);
        let env = Env { dir: dir.clone(), db: Arc::clone(&db), keep: false };
        env.ok("CREATE TABLE p (id INTEGER PRIMARY KEY, s TEXT UNIQUE)", "0");
        env.ok("CREATE TABLE c (id INTEGER, pid INTEGER REFERENCES p(id))", "0");
        env.ok("CREATE INDEX c_pid ON c (pid)", "0");
        env.ok("INSERT INTO p (s) VALUES ('a'), ('b')", "2");
        env.ok("INSERT INTO c VALUES (1, 1), (2, 2)", "2");
        env.ok("ALTER TABLE p ADD COLUMN n INTEGER DEFAULT 3", "0");
        env.ok("BEGIN", "0");
        env.ok("INSERT INTO p (s) VALUES ('c')", "1");
        env.ok("COMMIT", "0");
        env.ok("BEGIN", "0");
        env.ok("INSERT INTO p (s) VALUES ('rolled')", "1");
        env.ok("ROLLBACK", "0");
        env.keep();
    }
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db, keep: false };
        assert_eq!(
            flat(e.rows("SELECT id, s, n FROM p ORDER BY id")),
            vec!["1|a|3", "2|b|3", "3|c|3"]
        );
        assert_eq!(flat(e.rows("SELECT COUNT(*) FROM c")), vec!["2"]);
        e.err("INSERT INTO p (s) VALUES ('a')", "constraint");
        e.err("INSERT INTO c VALUES (9, 99)", "constraint");
        e.err("DELETE FROM p WHERE id = 1", "constraint");
        let plan = e.rows("EXPLAIN SELECT * FROM c WHERE pid = 1");
        assert!(plan[0][0].contains("INDEX c.c_pid"), "{plan:?}");
        e.ok("INSERT INTO p (s) VALUES ('later')", "1");
        // drop column is persisted too
        e.ok("ALTER TABLE p DROP COLUMN n", "0");
        e.ok("ALTER TABLE p RENAME TO pp", "0");
        e.keep();
    }
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db, keep: false };
        e.err("SELECT * FROM p", "no_table");
        assert_eq!(
            flat(e.rows("SELECT s FROM pp ORDER BY id")),
            vec!["a", "b", "c", "later"]
        );
        e.err("SELECT n FROM pp", "no_column");
    }
    let _ = fs::remove_dir_all(&dir);
}

impl Env {
    fn new_at_path(dir: &Path) -> Arc<Mutex<Db>> {
        Arc::new(Mutex::new(Db::open(dir.to_str().unwrap())))
    }
}

#[test]
fn torn_wal_tail_and_stale_header_are_safe() {
    let dir = temp_dir("torn");
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db: Arc::clone(&db), keep: false };
        e.ok("CREATE TABLE t (id INTEGER, v TEXT)", "0");
        e.ok("INSERT INTO t VALUES (1, 'one')", "1");
        e.ok("INSERT INTO t VALUES (2, 'two')", "1");
        e.keep();
    }
    let wal = dir.join("sdb.wal");
    let mut data = fs::read(&wal).unwrap();
    if let Some(p) = data.iter().position(|&b| b == 0) {
        data.truncate(p);
    }
    // Append a torn record without its newline: it must be ignored.
    data.extend_from_slice(b"I\tt\t2\t1\ti3\tthree");
    fs::write(&wal, &data).unwrap();
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db, keep: false };
        assert_eq!(flat(e.rows("SELECT id FROM t ORDER BY id")), vec!["1", "2"]);
        e.ok("INSERT INTO t VALUES (3, 'three')", "1");
        e.keep();
    }
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db, keep: false };
        assert_eq!(flat(e.rows("SELECT id FROM t ORDER BY id")), vec!["1", "2", "3"]);
        e.keep();
    }
    // With a snapshot in place, a WAL whose header names another
    // generation is discarded: the snapshot is the truth.
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db: Arc::clone(&db), keep: false };
        {
            let mut g = db.lock().unwrap();
            g.wal_bytes = WAL_COMPACT_BYTES;
            g.maybe_compact();
        }
        e.keep();
    }
    fs::write(&wal, "G\t999\nI\tt\t2\t1\ti9\tbogus\n").unwrap();
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db, keep: false };
        assert_eq!(flat(e.rows("SELECT id FROM t ORDER BY id")), vec!["1", "2", "3"]);
        e.ok("INSERT INTO t VALUES (4, 'four')", "1");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compaction_snapshot_round_trip() {
    let dir = temp_dir("compact");
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db: Arc::clone(&db), keep: false };
        e.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", "0");
        e.ok("CREATE UNIQUE INDEX t_v ON t (v)", "0");
        e.ok("INSERT INTO t (v) VALUES ('a'), ('b')", "2");
        {
            let mut g = db.lock().unwrap();
            g.wal_bytes = WAL_COMPACT_BYTES;
            g.maybe_compact();
            assert!(g.wal_bytes < WAL_COMPACT_BYTES);
        }
        assert!(dir.join("sdb.snap").exists());
        e.ok("INSERT INTO t (v) VALUES ('c')", "1");
        e.keep();
    }
    {
        let db = Env::new_at_path(&dir);
        let e = Env { dir: dir.clone(), db, keep: false };
        assert_eq!(flat(e.rows("SELECT id, v FROM t ORDER BY id")), vec!["1|a", "2|b", "3|c"]);
        e.err("INSERT INTO t (v) VALUES ('a')", "constraint");
        assert!(e.rows("EXPLAIN SELECT * FROM t WHERE v = 'b'")[0][0].contains("INDEX t.t_v"));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn network_connection_pipelining_and_eof() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    let e = Env::new("net");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let db = Arc::clone(&e.db);
    let h = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        handle_conn(sock, db, 5, None);
    });
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.write_all(b"CREATE TABLE t (id INTEGER, s TEXT)\nINSERT INTO t VALUES (1, 'a'), (2, 'b')\nSELECT s FROM t ORDER BY id\n")
        .unwrap();
    let mut rd = BufReader::new(sock.try_clone().unwrap());
    let mut lines = Vec::new();
    for _ in 0..8 {
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        lines.push(l.trim_end().to_string());
        if l.starts_with("END") {
            break;
        }
    }
    assert_eq!(
        lines,
        vec!["OK 0", "OK 2", "ROWS 1", "s", "a", "b", "END 2"]
    );
    // statement errors keep the connection alive
    sock.write_all(b"SELECT * FROM nope\nSELECT 1+1\n").unwrap();
    let mut l = String::new();
    rd.read_line(&mut l).unwrap();
    assert!(l.starts_with("ERR no_table"), "{l}");
    for _ in 0..4 {
        let mut x = String::new();
        rd.read_line(&mut x).unwrap();
        if x.starts_with("END") {
            break;
        }
    }
    // EOF with a half line: no statement is executed and the connection ends
    sock.write_all(b"INSERT INTO t VALUES (3, 'unterminated'").unwrap();
    drop(rd);
    drop(sock);
    h.join().unwrap();
    assert_eq!(flat(e.rows("SELECT COUNT(*) FROM t")), vec!["2"]);
    // pipeline several errors, then a valid one
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let db = Arc::clone(&e.db);
    let h = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        handle_conn(sock, db, 6, None);
    });
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.write_all(b"BAD\nSELECT 1\n").unwrap();
    let mut rd = BufReader::new(sock.try_clone().unwrap());
    let mut l = String::new();
    rd.read_line(&mut l).unwrap();
    assert!(l.starts_with("ERR syntax"));
    for _ in 0..4 {
        let mut x = String::new();
        rd.read_line(&mut x).unwrap();
        if x.starts_with("END") {
            break;
        }
    }
    drop(rd);
    drop(sock);
    h.join().unwrap();
}

#[test]
fn apply_record_stream_formats_and_malformed_inputs() {
    let e = Env::new("apply");
    let mut g = e.db.lock().unwrap();
    assert!(!g.apply_record(""));
    assert!(!g.apply_record("Z\tx"));
    assert!(!g.apply_record("C"));
    assert!(g.apply_record("C\tt\tid:i\tv:t"));
    assert!(!g.apply_record("C\tt\tid:i\tv:t"));
    assert!(!g.apply_record("C\tnocolon"));
    assert!(!g.apply_record("C\tbadty\tid:z"));
    assert!(g.apply_record("C\ttk\tid:i\tK"));
    // schema metadata: every kind, plus malformed variants
    assert!(g.apply_record(
        "C\ttm\tid:i\tv:t\tK\tn0\td1tzz\tp0\tA\tu1\tf1:ttm:p0"
    ));
    for bad in [
        "C\tb1\ta:i\tK\tz",
        "C\tb2\ta:i\tK\tnX",
        "C\tb3\ta:i\tK\tn9",
        "C\tb4\ta:i\tK\tdX",
        "C\tb5\ta:i\tK\td9i1",
        "C\tb6\ta:i\tK\td0z",
        "C\tb7\ta:i\tK\tpX",
        "C\tb8\ta:i\tK\tuX",
        "C\tb9\ta:i\tK\tf",
        "C\tb10\ta:i\tK\tfX:p:p",
        "C\tb11\ta:i\tK\tB",
    ] {
        let ok = g.apply_record(bad);
        assert!(!ok || bad.ends_with("\tB"), "{bad} -> {ok}");
    }
    // inserts: valid, empty and malformed
    assert!(g.apply_record("I\tt\t2\t1\ti1\ttone"));
    assert!(!g.apply_record("I\tt\t2\t1\ti1"));
    assert!(!g.apply_record("I\tt\t0\t1"));
    assert!(!g.apply_record("I\tt\tx\t1\ti1\ttx"));
    assert!(!g.apply_record("I\tt\t2\t1\tQ\ttx"));
    assert!(g.apply_record("I\tnope\t2\t1\ti1\ttx"));
    assert!(g.apply_record("I\tt\t2\t0"));
    assert!(g.apply_record("I\tt\t2\t1\ti2\ttwo"));
    assert!(g.apply_record("I\tt\t2\t1\ti3\tthree"));
    // updates
    assert!(g.apply_record("U\tt\t2\t1\t0\ti5\ttfive"));
    assert!(!g.apply_record("U\tt\t3\t1\t0\ti5\ttfive"));
    assert!(!g.apply_record("U\tt\t2\tx\t0\ti5\ttfive"));
    assert!(!g.apply_record("U\tt\t2\t1\t0\ti5"));
    assert!(!g.apply_record("U\tt\t2\t1\t9\ti5\ttfive"));
    assert!(!g.apply_record("U\tnope\t2\t1\t0\ti5\ttx"));
    assert!(!g.apply_record("U\tt\t2\t1\t0\tQ\ttx"));
    assert!(!g.apply_record("U\tt\t2\t1\tx"));
    // deletes (single row and full replacement)
    assert!(!g.apply_record("D\tt\tx"));
    assert!(!g.apply_record("D\tnope\t0"));
    assert!(!g.apply_record("D\tt\t9\t0"));
    assert!(!g.apply_record("D\tt\t1\t9"));
    assert!(!g.apply_record("D\tt\t1\tx"));
    assert!(g.apply_record("D\tt\t1\t0"));
    assert_eq!(g.tables["t"].rows.len(), 2);
    assert!(g.apply_record("E\tt\t2\t1\ti7\ttseven"));
    assert_eq!(g.tables["t"].rows.len(), 1);
    assert!(!g.apply_record("E\tt\t3\t1\ti7\ttseven"));
    assert!(!g.apply_record("E\tt\t2\t2\ti7\ttseven"));
    assert!(!g.apply_record("E\tt\t2\t1\tQ\tseven"));
    assert!(!g.apply_record("E\tnope\t2\t1\ti7\ttseven"));
    // indexes
    assert!(g.apply_record("X\tix\tt\t1\t1"));
    assert!(g.apply_record("X\tix\tt\t1\t1"));
    assert!(!g.apply_record("X\tix2\tt\tx\t1"));
    assert!(!g.apply_record("X\tix3\tnope\t0\t0"));
    assert!(!g.apply_record("X\tix4\tt\t9\t0"));
    assert!(g.apply_record("Y\tix"));
    assert!(g.apply_record("Y\tnope"));
    // alter add
    assert!(g.apply_record("P\tt\tz\ti\ti9"));
    assert!(!g.apply_record("P\tt\tz\ti\ti9"));
    assert!(!g.apply_record("P\tt\tz2\tq\t-"));
    assert!(!g.apply_record("P\tt\tz3\ti\tQ"));
    assert!(!g.apply_record("P\tnope\tz\ti\t-"));
    // alter drop / rename
    assert!(!g.apply_record("Q\tnope\tz"));
    assert!(!g.apply_record("Q\tt\tnope"));
    assert!(g.apply_record("Q\tt\tz"));
    assert!(!g.apply_record("W\tnope\ta\tb"));
    assert!(!g.apply_record("W\tt\tnope\tb"));
    assert!(g.apply_record("W\tt\tv\tw"));
    assert!(!g.apply_record("V\tnope\tx"));
    assert!(g.apply_record("V\tt\tt9"));
    drop(g);
}

#[test]
fn index_range_streaming_orders_and_nulls() {
    let e = Env::new("stream");
    e.ok("CREATE TABLE t (k INTEGER, v TEXT)", "0");
    e.ok(
        "INSERT INTO t VALUES (NULL, 'n'), (-1, 'm'), (0, 'z'), (1, 'a'), (2, 'b'), (2, 'c'), (3, 'd')",
        "7",
    );
    e.ok("CREATE INDEX t_k ON t (k)", "0");
    assert_eq!(
        flat(e.rows("SELECT v FROM t WHERE k IS NULL")),
        vec!["n"]
    );
    assert_eq!(
        flat(e.rows("SELECT k FROM t WHERE k IS NOT NULL ORDER BY k")),
        vec!["-1", "0", "1", "2", "2", "3"]
    );
    assert_eq!(
        flat(e.rows("SELECT k FROM t ORDER BY k DESC")),
        vec!["3", "2", "2", "1", "0", "-1", "\\N"]
    );
    assert_eq!(
        flat(e.rows("SELECT k FROM t ORDER BY k LIMIT 2 OFFSET 1")),
        vec!["-1", "0"]
    );
    assert_eq!(
        flat(e.rows("SELECT k FROM t WHERE k >= 1 ORDER BY k DESC LIMIT 1")),
        vec!["3"]
    );
    assert_eq!(
        flat(e.rows("SELECT v FROM t WHERE k > 0 AND k < 2 ORDER BY k, v")),
        vec!["a"]
    );
    assert_eq!(
        flat(e.rows("SELECT v FROM t WHERE k = 5 AND k = 6")),
        Vec::<String>::new()
    );
    assert_eq!(
        flat(e.rows("SELECT v FROM t WHERE k < -5")),
        Vec::<String>::new()
    );
    assert_eq!(
        flat(e.rows("SELECT v FROM t WHERE k IS NULL AND k = 1")),
        Vec::<String>::new()
    );
    assert_eq!(
        flat(e.rows("SELECT t2.v FROM t t2 WHERE t2.k = 2 ORDER BY t2.v")),
        vec!["b", "c"]
    );
    assert_eq!(
        e.header("SELECT t.* FROM t WHERE k = 1"),
        vec!["k", "v"]
    );
    e.err("SELECT zz.* FROM t", "no_table");
    e.err("SELECT t.* FROM t x", "no_table");
    // constant-only queries and aggregate plans
    assert_eq!(flat(e.rows("SELECT 1")), vec!["1"]);
    assert_eq!(flat(e.rows("SELECT COUNT(*) FROM t")), vec!["7"]);
    e.err("SELECT * FROM", "syntax");
}

#[test]
fn subquery_aggregate_composition() {
    let e = Env::new("subagg");
    e.ok("CREATE TABLE a (id INTEGER, v INTEGER)", "0");
    e.ok("INSERT INTO a VALUES (1, 10), (2, 20), (3, 20), (4, NULL)", "4");
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE id IN (SELECT v/10 FROM a WHERE v IS NOT NULL GROUP BY v/10 HAVING COUNT(*) >= 1 ORDER BY 1 LIMIT 2) ORDER BY id")),
        vec!["1", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT (SELECT v FROM a ORDER BY v DESC LIMIT 1)")),
        vec!["20"]
    );
    assert_eq!(
        flat(e.rows("SELECT (SELECT COUNT(*) FROM a GROUP BY v HAVING COUNT(*) > 1)")),
        vec!["2"]
    );
    assert_eq!(
        flat(e.rows("SELECT (SELECT COUNT(*) FROM a GROUP BY v HAVING COUNT(*) > 9)")),
        vec!["\\N"]
    );
    assert_eq!(
        flat(e.rows("SELECT EXISTS (SELECT 1 FROM a, a b WHERE a.id = b.id AND a.id = 4)")),
        vec!["1"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE EXISTS (SELECT 1 FROM a b WHERE b.v = a.v GROUP BY b.id HAVING COUNT(*) > 0) ORDER BY id")),
        vec!["1", "2", "3"]
    );
    assert_eq!(
        flat(e.rows("SELECT (SELECT DISTINCT v FROM a WHERE v = 20)")),
        vec!["20"]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM a WHERE id IN (SELECT id FROM a WHERE v IS NULL OR v = 10 ORDER BY id DESC LIMIT 1)")),
        vec!["4"]
    );
}

#[test]
fn explain_write_and_join_plans() {
    let e = Env::new("explain");
    e.ok("CREATE TABLE users (id INTEGER, email TEXT)", "0");
    e.ok("CREATE TABLE notes (id INTEGER, uid INTEGER, body TEXT)", "0");
    e.ok("CREATE INDEX users_email ON users (email)", "0");
    e.ok("CREATE INDEX notes_uid ON notes (uid)", "0");
    let plan = e.rows("EXPLAIN SELECT 1");
    assert_eq!(plan, vec![vec!["SCAN CONSTANT ROW".to_string()]]);
    let plan = e.rows("EXPLAIN SELECT COUNT(*) FROM users");
    assert!(plan.iter().any(|r| r[0].contains("SCAN users")), "{plan:?}");
    let plan = e.rows("EXPLAIN UPDATE users SET email = 'z' WHERE email = 'a'");
    assert!(plan.iter().any(|r| r[0].contains("INDEX users.users_email")), "{plan:?}");
    let plan = e.rows("EXPLAIN DELETE FROM users WHERE id = 1");
    assert!(plan.iter().any(|r| r[0].contains("SCAN users")), "{plan:?}");
    e.err("EXPLAIN UPDATE nope SET a = 1", "no_table");
    let plan = e.rows(
        "EXPLAIN SELECT * FROM users u, notes n WHERE u.id = n.uid AND n.uid = 1",
    );
    assert!(plan.iter().any(|r| r[0].contains("INDEX n.notes_uid")), "{plan:?}");
    assert!(plan.iter().any(|r| r[0].contains("SCAN u")), "{plan:?}");
    e.err("EXPLAIN SELECT * FROM users JOIN nope ON 1 = 1", "no_table");
    let plan = e.rows("EXPLAIN SELECT MAX(id) FROM users WHERE email = 'a'");
    assert!(plan.iter().any(|r| r[0].contains("INDEX users.users_email")), "{plan:?}");
}

#[test]
fn foreign_key_declaration_errors() {
    let e = Env::new("fkerr");
    e.ok("CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT UNIQUE, plain INTEGER)", "0");
    e.ok("CREATE TABLE c (x INTEGER REFERENCES p(id))", "0");
    // A child may be declared before its parent exists; resolving the key
    // on the first write reports the mismatch.
    e.ok("CREATE TABLE bad1 (x INTEGER REFERENCES nope(id))", "0");
    e.err("INSERT INTO bad1 VALUES (1)", "no_table");
    e.ok("CREATE TABLE bad2 (x INTEGER REFERENCES p(zz))", "0");
    e.err("INSERT INTO bad2 VALUES (1)", "syntax");
    e.ok("CREATE TABLE bad3 (x INTEGER REFERENCES p(id, code))", "0");
    e.err("INSERT INTO bad3 VALUES (1)", "syntax");
    e.ok("CREATE TABLE bad4 (x INTEGER REFERENCES p(plain))", "0");
    e.err("INSERT INTO bad4 VALUES (1)", "syntax");
    e.ok(
        "CREATE TABLE bad5 (a INTEGER, b INTEGER, FOREIGN KEY (a, b) REFERENCES p(id))",
        "0",
    );
    e.err("INSERT INTO bad5 VALUES (1, 2)", "syntax");
    e.ok(
        "CREATE TABLE bad6 (a INTEGER, FOREIGN KEY (a) REFERENCES p(code, id))",
        "0",
    );
    e.err("INSERT INTO bad6 VALUES (1)", "syntax");
    e.ok("CREATE TABLE c2 (x TEXT, FOREIGN KEY (x) REFERENCES p(code))", "0");
    e.ok("INSERT INTO p (id, code, plain) VALUES (1, 'k', 0)", "1");
    e.ok("INSERT INTO c2 VALUES ('k')", "1");
    e.err("INSERT INTO c2 VALUES ('nope')", "constraint");
}

#[test]
fn internal_helpers_and_transaction_edges() {
    let e = Env::new("internal");
    e.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", "0");
    e.ok("INSERT INTO t VALUES (1, 'a')", "1");
    {
        let mut g = e.db.lock().unwrap();
        g.cur = 1;
        // table_mut copies the committed image into an open transaction
        g.begin_txn(1, true).unwrap();
        assert!(g.table_mut("t").is_some());
        assert!(g.table_mut("t").is_some());
        assert!(g.table_view_mut("t").is_some());
        assert!(g.table_view_mut("nope").is_none());
        g.abort_txn(1);
        assert!(g.txns.is_empty());
        // join key encoding
        assert!(join_key_value(&Value::Null).is_none());
        assert_eq!(join_key_value(&Value::Int(3)).unwrap(), "i3");
        assert_eq!(join_key_value(&Value::Text("x".into())).unwrap(), "tx");
        assert_eq!(join_key_value(&Value::Real(1.5)).unwrap(), "r1.5");
        // index helpers
        assert!(g.index_owner("t").is_none());
        let _ = g.index_owner("nope");
        assert!(g.all_table_names().contains(&"t".to_string()));
        assert!(g.table_ref("t").is_some());
        assert!(g.table_exists("t"));
        assert!(!g.table_exists("nope"));
        let tt = g.take_table("t").unwrap();
        g.put_table("t", tt);
        assert!(g.table_ref("t").is_some());
        assert!(g.resolve_fks("t", g.table_ref("t").unwrap()).is_ok());
        assert!(g.resolve_parent_fks("t", g.table_ref("t").unwrap(), "t", g.table_ref("t").unwrap()).is_ok());
        assert!(g.fk_exists("t", g.table_ref("t").unwrap(), &RFk { cols: vec![0], parent: "t".into(), pcols: vec![0] }, &[Value::Int(1), Value::Text("a".into())]));
        assert_eq!(wal_capacity(0), 1024 * 1024);
        assert_eq!(const_eval(&Expr::Lit(Value::Int(4))), Some(Value::Int(4)));
        assert_eq!(const_eval(&Expr::Col { qual: None, name: "x".into() }), None);
        assert_eq!(const_eval(&Expr::Neg(Box::new(Expr::Lit(Value::Int(4))))), Some(Value::Int(-4)));
        assert_eq!(const_eval(&Expr::Not(Box::new(Expr::Lit(Value::Int(0))))), None);
        assert_eq!(const_eval(&Expr::IsNull(Box::new(Expr::Lit(Value::Null)), false)), None);
        assert_eq!(const_eval(&Expr::Bin(Op::Add, Box::new(Expr::Lit(Value::Int(1))), Box::new(Expr::Lit(Value::Int(1))))), Some(Value::Int(2)));
        let rt = const_rvalue(&RExpr::Lit(Value::Int(4)));
        assert_eq!(rt, Some(Value::Int(4)));
        assert_eq!(const_rvalue(&RExpr::Col(0)), None);
    }
    // transaction copy-on-write through SQL, including a read-only txn
    e.ok("BEGIN", "0");
    assert_eq!(flat(e.rows("SELECT * FROM t")), vec!["1|a"]);
    e.ok("INSERT INTO t VALUES (2, 'b')", "1");
    e.ok("COMMIT", "0");
    assert_eq!(flat(e.rows("SELECT id FROM t ORDER BY id")), vec!["1", "2"]);
    // a table renamed inside a transaction that rolls back stays hidden
    e.ok("BEGIN", "0");
    e.ok("ALTER TABLE t RENAME TO hidden", "0");
    e.err("SELECT * FROM t", "no_table");
    e.ok("ROLLBACK", "0");
    assert_eq!(flat(e.rows("SELECT id FROM t ORDER BY id")), vec!["1", "2"]);
    assert_eq!(e.rows("PRAGMA foreign_keys"), vec![vec!["1".to_string()]]);
    e.ok("PRAGMA foreign_keys = ON", "0");
    e.ok("PRAGMA cache_size(100)", "0");
    e.err("COMMIT TRANSACTION", "txn");
    e.err("END", "txn");
}

// ------------------------------------------------------ disk-full incident ---

#[test]
fn free_space_and_full_error_mapping() {
    // statvfs must answer for a directory that exists, and the check must
    // refuse an impossible request while allowing a tiny one.
    let dir = temp_dir("space");
    fs::create_dir_all(&dir).unwrap();
    let free = fs_available(&dir).expect("statvfs on a real directory");
    assert!(free > 0, "free space should be positive");
    let e = Env::new("space-db");
    assert!(e.db.lock().unwrap().have_space(0));
    assert!(!e.db.lock().unwrap().have_space(u64::MAX));
    // A missing directory cannot be measured; the check then defers to the
    // write error path instead of rejecting the statement.
    assert!(fs_available(&dir.join("missing")).is_none());

    // ENOSPC/EDQUOT reach the wire as class `full`; anything else is `io`.
    let enospc = std::io::Error::from_raw_os_error(28);
    assert!(is_no_space(&enospc));
    assert!(is_no_space(&std::io::Error::from_raw_os_error(122)));
    assert!(!is_no_space(&std::io::Error::from_raw_os_error(5)));
    let resp = full_err().response();
    assert!(resp.starts_with("ERR full "), "resp: {resp}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn failed_log_write_is_rolled_back_and_the_store_stays_readable() {
    let e = Env::new("fullwrite");
    e.ok("CREATE TABLE seed (id INTEGER, v TEXT)", "0");
    e.ok("INSERT INTO seed VALUES (1, 'a'), (2, 'b')", "2");
    let before = flat(e.rows("SELECT * FROM seed ORDER BY id"));
    assert_eq!(before, vec!["1|a", "2|b"]);

    // Make the log unwritable: the next statement cannot be made durable.
    {
        let mut g = e.db.lock().unwrap();
        let w = g.wal.clone().unwrap();
        let path = g.wal_path.clone();
        let mut wg = w.lock();
        let ro = OpenOptions::new().read(true).open(&path).unwrap();
        wg.file = Arc::new(ro);
    }
    let r = e.exec("INSERT INTO seed VALUES (3, 'c')");
    assert!(r.starts_with("ERR "), "resp: {r}");
    drop(r);

    // Nothing was applied and the connection still answers.
    assert_eq!(flat(e.rows("SELECT COUNT(*) AS n FROM seed")), vec!["2"]);
    assert_eq!(flat(e.rows("SELECT * FROM seed ORDER BY id")), before);
    assert_eq!(flat(e.rows("SELECT 1 AS one")), vec!["1"]);
}

// -------------------------------------------------------- replication ---

use std::io::{BufRead, BufReader, Read, Write};

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn read_line(rd: &mut BufReader<TcpStream>) -> String {
    let mut l = String::new();
    rd.read_line(&mut l).unwrap();
    l
}

fn recv_within<T: Send + 'static>(rx: &std::sync::mpsc::Receiver<T>, ms: u64) -> T {
    rx.recv_timeout(Duration::from_millis(ms))
        .expect("timed out waiting for replication message")
}

fn wait_until<F: Fn() -> bool>(f: F, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    f()
}

#[test]
fn replication_helpers_records_and_apply() {
    // ---- pure helpers -------------------------------------------------
    assert_eq!(
        repl::parse_hostport("host:123"),
        Some(("host".to_string(), 123))
    );
    assert_eq!(
        repl::parse_hostport(" host : 45 "),
        Some(("host".to_string(), 45))
    );
    assert_eq!(repl::parse_hostport("nohost"), None);
    assert_eq!(repl::parse_hostport("host:xx"), None);
    assert_eq!(
        repl::parse_nodes("a:1, b:2,, :9,bad,c:3"),
        vec![
            ("a".to_string(), 1),
            ("b".to_string(), 2),
            ("c".to_string(), 3)
        ]
    );
    assert!(repl::parse_nodes("").is_empty());

    let dir = temp_dir("repl-lsn");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(repl::load_lsn(&dir), 0);
    repl::store_lsn(&dir, 41);
    assert_eq!(repl::load_lsn(&dir), 41);
    assert_eq!(repl::lsn_file(&dir), dir.join("repl.lsn"));
    fs::write(repl::lsn_file(&dir), "junk\n").unwrap();
    assert_eq!(repl::load_lsn(&dir), 0);

    assert!(repl::key(6, 900) > repl::key(5, 1));
    assert!(repl::key(5, 1) > repl::key(5, 2));

    for k in [
        std::io::ErrorKind::WouldBlock,
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::Interrupted,
    ] {
        assert!(repl::is_timeout(&std::io::Error::new(k, "x")));
    }
    assert!(!repl::is_timeout(&std::io::Error::new(
        std::io::ErrorKind::Other,
        "x"
    )));

    // `mx` recovers a poisoned mutex instead of propagating the panic.
    let m = Arc::new(Mutex::new(7u32));
    let m2 = Arc::clone(&m);
    let _ = thread::spawn(move || {
        let _g = m2.lock().unwrap();
        panic!("poison on purpose");
    })
    .join();
    assert_eq!(*repl::mx(&m), 7);

    // ---- process-wide entry points with no cluster --------------------
    // Unit tests never call `repl::init`, so the global state is empty and
    // every accessor must answer the lone-node defaults.
    assert!(!repl::active());
    assert!(!repl::is_replica());
    assert_eq!(
        repl::show_role(),
        "ROWS 2\nname\tvalue\nrole\tprimary\nlsn\t0\nEND 2\n"
    );
    repl::note_commit();
    repl::commit_batch(vec!["T\tB".to_string()]);
    repl::commit_batch(vec!["I\tx\t1\t1\ti1".to_string()]);

    // ---- a live primary instance --------------------------------------
    let e = Env::new("repl-local");
    e.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)", "0");
    e.ok("CREATE INDEX ix ON t (v)", "0");
    e.ok("INSERT INTO t VALUES (1, 'a'), (2, 'c')", "2");
    let st = repl::test_instance(
        6001,
        dir.clone(),
        vec![],
        Arc::clone(&e.db),
        true,
        7,
        true,
        None,
    );
    assert!(repl::active_of(Some(&st)));
    assert!(!repl::is_replica_of(Some(&st)));
    assert_eq!(
        repl::show_role_of(Some(&st)),
        "ROWS 2\nname\tvalue\nrole\tprimary\nlsn\t7\nEND 2\n"
    );
    st.test_set_primary(false);
    assert!(repl::is_replica_of(Some(&st)));
    assert_eq!(
        repl::show_role_of(Some(&st)),
        "ROWS 2\nname\tvalue\nrole\treplica\nlsn\t7\nEND 2\n"
    );
    let lonely = repl::test_instance(
        6002,
        dir.clone(),
        vec![],
        Arc::clone(&e.db),
        true,
        1,
        false,
        None,
    );
    assert!(!repl::active_of(Some(&lonely)));
    repl::note_commit_on(&lonely);
    assert_eq!(lonely.test_lsn(), 2);
    // An inactive node never streams: it just advances its own LSN.
    repl::commit_batch_on(&lonely, vec!["I\tt\t2\t1\ti9\ttnine".to_string()]);
    assert_eq!(lonely.test_lsn(), 3);
    lonely.test_set_primary(false);
    repl::commit_batch_on(&lonely, vec!["I\tt\t2\t1\ti9\ttnine".to_string()]);
    assert_eq!(lonely.test_lsn(), 4);
    repl::commit_batch_on(&st, vec!["T\tB".to_string(), "T\tE".to_string()]);
    assert_eq!(st.test_lsn(), 7);

    // ---- snapshot of a populated schema --------------------------------
    let recs = repl::snapshot_records(&e.db.lock().unwrap());
    assert!(
        recs.iter().any(|r| r.starts_with("C\tt\tid:i\tv:t")),
        "{recs:?}"
    );
    assert!(recs.iter().any(|r| r.starts_with("I\tt\t2\t1\ti1\tta")));
    assert!(recs.iter().any(|r| r.starts_with("I\tt\t2\t1\ti2\ttc")));
    assert!(recs.iter().any(|r| r.starts_with("X\tix\tt")));

    // ---- a fresh replica installs the snapshot and applies batches -----
    let r = Env::new("repl-install");
    let rst = repl::test_instance(
        6003,
        r.dir.clone(),
        vec![],
        Arc::clone(&r.db),
        false,
        0,
        true,
        None,
    );
    assert!(repl::apply_snapshot(&rst, &recs, 7));
    assert_eq!(rst.test_lsn(), 7);
    assert_eq!(repl::load_lsn(&r.dir), 7);
    assert_eq!(flat(r.rows("SELECT COUNT(*) FROM t")), vec!["2"]);
    assert_eq!(flat(r.rows("SELECT v FROM t WHERE id = 2")), vec!["c"]);
    // The unique index survived the install.
    r.err("INSERT INTO t VALUES (3, 'a')", "constraint");

    assert!(repl::apply_batch(
        &rst,
        &["I\tt\t2\t1\ti3\ttthree".to_string()],
        8
    ));
    assert_eq!(rst.test_lsn(), 8);
    assert_eq!(flat(r.rows("SELECT v FROM t WHERE id = 3")), vec!["three"]);
    // A record that cannot be applied aborts the batch (and the LSN).
    assert!(!repl::apply_batch(&rst, &["NOPE".to_string()], 9));
    assert_eq!(rst.test_lsn(), 8);
    assert!(!repl::apply_snapshot(&rst, &["NOPE".to_string()], 9));
    assert_eq!(rst.test_lsn(), 8);
    repl::force_resync(&rst);
    assert_eq!(rst.test_lsn(), 0);
    assert_eq!(repl::load_lsn(&r.dir), 0);
}

#[test]
fn replication_primary_streams_snapshot_batches_and_control() {
    let e = Env::new("repl-stream");
    e.ok("CREATE TABLE t (id INTEGER, v TEXT)", "0");
    e.ok("INSERT INTO t VALUES (1, 'a')", "1");
    let st = repl::test_instance(
        6100,
        e.dir.clone(),
        vec![("127.0.0.1".to_string(), 9999)],
        Arc::clone(&e.db),
        true,
        3,
        true,
        None,
    );

    // Replica with an older LSN: the primary sends a full snapshot.
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let st2 = Arc::clone(&st);
    let h = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        repl::serve_replica(&st2, sock, 4444, 2);
    });
    let mut c = TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut rd = BufReader::new(c.try_clone().unwrap());
    let head = read_line(&mut rd);
    let f: Vec<&str> = head.trim_end().split('\t').collect();
    assert_eq!(&f[..2], ["SNAP", "3"], "{head:?}");
    let n: usize = f[2].parse().unwrap();
    let mut snap = Vec::new();
    for _ in 0..n {
        snap.push(read_line(&mut rd).trim_end().to_string());
    }
    assert!(snap.iter().any(|r| r.starts_with("C\tt\tid:i\tv:t")));
    assert!(snap.iter().any(|r| r.starts_with("I\tt\t2\t1\ti1\tta")));
    c.write_all(b"A 3\n").unwrap();

    // A commit on the primary blocks on the frame until the replica acks it.
    let st3 = Arc::clone(&st);
    let committer = thread::spawn(move || {
        repl::commit_batch_on(&st3, vec!["I\tt\t2\t1\ti2\ttwo".to_string()]);
    });
    let bhead = read_line(&mut rd);
    let bf: Vec<&str> = bhead.trim_end().split('\t').collect();
    assert_eq!(bf[0], "B");
    assert_eq!(bf[1], "4");
    let len: usize = bf[2].parse().unwrap();
    let mut body = vec![0u8; len];
    rd.read_exact(&mut body).unwrap();
    assert_eq!(String::from_utf8_lossy(&body), "I\tt\t2\t1\ti2\ttwo\n");
    assert!(!committer.is_finished(), "commit must wait for the ack");
    c.write_all(b"A 4\n").unwrap();
    committer.join().unwrap();
    assert_eq!(st.test_lsn(), 4);
    assert_eq!(repl::load_lsn(&e.dir), 4);
    assert_eq!(st.test_reps_len(), 1);

    // Closing the replica connection unregisters it.
    drop(rd);
    drop(c);
    h.join().unwrap();
    assert_eq!(st.test_reps_len(), 0);

    // A replica that is already current gets LIVE, not a snapshot.
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let st2 = Arc::clone(&st);
    let h = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        repl::serve_replica(&st2, sock, 4444, 4);
    });
    let c = TcpStream::connect(addr).unwrap();
    let mut rd = BufReader::new(c.try_clone().unwrap());
    assert_eq!(read_line(&mut rd).trim_end(), "LIVE\t4");
    drop(rd);
    drop(c);
    h.join().unwrap();

    // VOTE reports this node's role and LSN.
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let st2 = Arc::clone(&st);
    let h = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        repl::handle_control_on(&st2, &sock, " VOTE\n");
    });
    let c = TcpStream::connect(addr).unwrap();
    let mut rd = BufReader::new(c.try_clone().unwrap());
    assert_eq!(read_line(&mut rd).trim_end(), "R\tprimary\t4\t6100");
    drop(rd);
    drop(c);
    h.join().unwrap();

    // HELLO rides the same control entry point.
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let st2 = Arc::clone(&st);
    let h = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        repl::handle_control_on(&st2, &sock, " HELLO\t4444\t0\n");
    });
    let c = TcpStream::connect(addr).unwrap();
    let mut rd = BufReader::new(c.try_clone().unwrap());
    assert!(read_line(&mut rd).starts_with("SNAP\t4\t"));
    drop(rd);
    drop(c);
    h.join().unwrap();

    // PROMOTE from a peer we know about makes a replica follow it.
    st.test_set_primary(false);
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let st2 = Arc::clone(&st);
    let h = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        repl::handle_control_on(&st2, &sock, " PROMOTE\t9999\t9\n");
    });
    let c = TcpStream::connect(addr).unwrap();
    thread::sleep(Duration::from_millis(50));
    drop(c);
    h.join().unwrap();
    assert_eq!(st.test_addr(), Some("127.0.0.1:9999".to_string()));

    // A stale/lower promotion claim leaves the address alone but marks the
    // primary as dead so this node can run its own election immediately.
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let st2 = Arc::clone(&st);
    let h = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        repl::handle_control_on(&st2, &sock, " PROMOTE\t9999\t1\n");
    });
    let c = TcpStream::connect(addr).unwrap();
    thread::sleep(Duration::from_millis(50));
    drop(c);
    h.join().unwrap();
    assert!(!st.test_has_contact());

    // Unknown commands and malformed arguments are ignored.
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let st2 = Arc::clone(&st);
    let h = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        repl::handle_control_on(&st2, &sock, " WHAT\n");
        repl::handle_control_on(&st2, &sock, " PROMOTE\tx\ty\n");
        repl::handle_control_on(&st2, &sock, "\n");
    });
    let c = TcpStream::connect(addr).unwrap();
    thread::sleep(Duration::from_millis(100));
    drop(c);
    h.join().unwrap();

    // promote/demote flip the role and the advertised address.
    repl::promote(&st);
    assert!(st.test_is_primary());
    assert_eq!(st.test_addr(), None);
    repl::demote(&st, "127.0.0.1", 9999);
    assert!(!st.test_is_primary());
    assert_eq!(st.test_addr(), Some("127.0.0.1:9999".to_string()));

    // A commit with a replica whose receiver is gone does not wedge.
    let _ = repl::apply_snapshot(&st, &[], 50);
    repl::promote(&st);
    let t0 = Instant::now();
    repl::commit_batch_on(&st, vec!["I\tt\t2\t1\ti3\ttthree".to_string()]);
    assert!(t0.elapsed() < Duration::from_secs(3));
}

#[test]
fn replication_syncer_follows_primary_frames() {
    use std::sync::mpsc::channel;
    let r = Env::new("repl-sync");
    let addr = format!("127.0.0.1:{}", free_port());
    let lst = TcpListener::bind(&addr).unwrap();
    let (tx1, rx1) = channel::<()>();
    let (txgo, rxgo) = channel::<()>();
    let (tx2, rx2) = channel::<()>();
    let (tx3, rx3) = channel::<()>();
    let server = thread::spawn(move || {
        // session 1: LIVE, SNAP, batch, bad batch
        let (s, _) = lst.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut rd = BufReader::new(s.try_clone().unwrap());
        let hello = read_line(&mut rd);
        assert!(hello.starts_with("\u{1}SDBR HELLO"), "{hello:?}");
        let mut w = s.try_clone().unwrap();
        w.write_all(b"LIVE\t5\n").unwrap();
        thread::sleep(Duration::from_millis(320)); // read-timeout retry branch
        w.write_all(b"SNAP\t6\t1\n").unwrap();
        thread::sleep(Duration::from_millis(320)); // timeout while reading a record
        w.write_all(b"C\tt\tid:i\tv:t\n").unwrap();
        assert_eq!(read_line(&mut rd).trim_end(), "A 6");
        w.write_all(b"B\t7\t14\nI\tt\t2\t1\ti1\tta\n").unwrap();
        assert_eq!(read_line(&mut rd).trim_end(), "A 7");
        tx1.send(()).unwrap();
        rxgo.recv().unwrap();
        w.write_all(b"B\t8\t5\nNOPE\n").unwrap();
        drop(w);
        drop(s);

        // session 2: a snapshot that cannot be installed
        let (s, _) = lst.accept().unwrap();
        let mut rd = BufReader::new(s.try_clone().unwrap());
        let _ = read_line(&mut rd);
        let mut w = s.try_clone().unwrap();
        w.write_all(b"SNAP\t9\t1\nNOPE\n").unwrap();
        thread::sleep(Duration::from_millis(120));
        drop(w);
        drop(s);
        tx2.send(()).unwrap();

        // session 3: unknown frames and a heartbeat
        let (s, _) = lst.accept().unwrap();
        let mut rd = BufReader::new(s.try_clone().unwrap());
        let _ = read_line(&mut rd);
        let mut w = s.try_clone().unwrap();
        w.write_all(b"Z\t1\nH\t9\n").unwrap();
        thread::sleep(Duration::from_millis(120));
        drop(w);
        drop(s);
        tx3.send(()).unwrap();
    });

    let st = repl::test_instance(
        6200,
        r.dir.clone(),
        vec![],
        Arc::clone(&r.db),
        false,
        0,
        true,
        Some(addr),
    );
    let s2 = Arc::clone(&st);
    let sync_thread = thread::spawn(move || repl::syncer(s2));

    recv_within(&rx1, 5000);
    assert!(wait_until(|| st.test_lsn() == 7, 3000), "lsn did not reach 7");
    assert_eq!(flat(r.rows("SELECT v FROM t WHERE id = 1")), vec!["a"]);
    txgo.send(()).unwrap();
    recv_within(&rx2, 5000);
    assert!(wait_until(|| st.test_lsn() == 0, 3000));
    recv_within(&rx3, 5000);
    server.join().unwrap();

    // Stop the syncer: it parks while this node is primary.
    st.test_set_primary(true);
    thread::sleep(Duration::from_millis(150));
    let _ = sync_thread.is_finished();
}

#[test]
fn replication_election_chooses_the_best_node() {
    // A peer that answers VOTE with a fixed role/LSN.
    fn vote_peer(role: &'static str, lsn: u64) -> u16 {
        let lst = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = lst.local_addr().unwrap().port();
        thread::spawn(move || {
            let (s, _) = lst.accept().unwrap();
            let mut rd = BufReader::new(s.try_clone().unwrap());
            let _ = read_line(&mut rd);
            let mut w = s.try_clone().unwrap();
            w.write_all(format!("R\t{role}\t{lsn}\t{port}\n").as_bytes())
                .unwrap();
        });
        port
    }
    // A peer that accepts but never answers (timeout path).
    fn mute_peer() -> u16 {
        let lst = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = lst.local_addr().unwrap().port();
        thread::spawn(move || {
            let (s, _) = lst.accept().unwrap();
            thread::sleep(Duration::from_millis(900));
            drop(s);
        });
        port
    }

    let e = Env::new("repl-elect");
    let mk = |nodes: Vec<(String, u16)>, lsn: u64| {
        repl::test_instance(
            6300,
            e.dir.clone(),
            nodes,
            Arc::clone(&e.db),
            false,
            lsn,
            true,
            None,
        )
    };

    // 1. A live primary with more data wins: follow it.
    let p = vote_peer("primary", 9);
    let st = mk(vec![("127.0.0.1".to_string(), p)], 3);
    st.test_set_contact(Some(Instant::now() - Duration::from_secs(5)));
    st.test_set_contacted(true);
    repl::elect(&st);
    assert_eq!(st.test_addr(), Some(format!("127.0.0.1:{p}")));
    assert!(!st.test_is_primary());

    // 2. A primary claiming less data than us cannot stop our promotion.
    let p = vote_peer("primary", 2);
    let st = mk(vec![("127.0.0.1".to_string(), p)], 5);
    st.test_set_contact(Some(Instant::now() - Duration::from_secs(5)));
    st.test_set_contacted(true);
    repl::elect(&st);
    assert!(st.test_is_primary());

    // 3. A better replica: wait, then promote when the cluster is small.
    let p = vote_peer("replica", 9);
    let st = mk(vec![("127.0.0.1".to_string(), p)], 4);
    st.test_set_contact(Some(Instant::now() - Duration::from_secs(6)));
    st.test_set_contacted(true);
    repl::elect(&st);
    assert!(st.test_is_primary());

    // 4. No reachable peers: the contacted survivor promotes itself.
    let dead = free_port();
    let st = mk(vec![("127.0.0.1".to_string(), dead)], 2);
    st.test_set_contact(Some(Instant::now() - Duration::from_secs(5)));
    st.test_set_contacted(true);
    repl::elect(&st);
    assert!(st.test_is_primary());

    // 5. A node that never heard from its primary never crowns itself.
    let dead = free_port();
    let st = mk(vec![("127.0.0.1".to_string(), dead)], 2);
    st.test_set_contact(Some(Instant::now() - Duration::from_secs(5)));
    repl::elect(&st);
    assert!(!st.test_is_primary());

    // 6. A muted peer just times the vote out.
    let m = mute_peer();
    let st = mk(vec![("127.0.0.1".to_string(), m)], 2);
    assert_eq!(repl::vote_call(&st, "127.0.0.1", m), None);

    // 7. A peer list that includes our own port is skipped.
    let st = mk(vec![("127.0.0.1".to_string(), 6300)], 2);
    assert!(repl::gather_votes(&st).is_empty());

    // 8. demote is exactly the "stand down" transition.
    let st = mk(vec![], 5);
    st.test_set_primary(true);
    repl::demote(&st, "127.0.0.1", 7000);
    assert!(!st.test_is_primary());
    assert_eq!(st.test_addr(), Some("127.0.0.1:7000".to_string()));

    // 9. The election loop promotes a stale, contacted replica on its own.
    let st = mk(vec![], 1);
    let s2 = Arc::clone(&st);
    thread::spawn(move || repl::election_loop(s2));
    thread::sleep(Duration::from_millis(80));
    st.test_set_contact(Some(Instant::now() - Duration::from_secs(5)));
    st.test_set_contacted(true);
    assert!(wait_until(|| st.test_is_primary(), 3000));

    // 10. The heartbeat and watch loops run against a primary with a peer.
    let peer = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer_port = peer.local_addr().unwrap().port();
    thread::spawn(move || {
        for s in peer.incoming() {
            let s = match s {
                Ok(s) => s,
                Err(_) => break,
            };
            thread::spawn(move || {
                let mut rd = BufReader::new(s.try_clone().unwrap());
                let line = read_line(&mut rd);
                if line.contains("VOTE") {
                    let mut w = s.try_clone().unwrap();
                    let _ = w.write_all(b"R\treplica\t1\t1\n");
                }
            });
        }
    });
    let pst = repl::test_instance(
        6301,
        e.dir.clone(),
        vec![("127.0.0.1".to_string(), peer_port)],
        Arc::clone(&e.db),
        true,
        2,
        true,
        None,
    );
    let l1 = Arc::clone(&pst);
    thread::spawn(move || repl::primary_loop(l1));
    let l2 = Arc::clone(&pst);
    thread::spawn(move || repl::watch_loop(l2));
    thread::sleep(Duration::from_millis(1400));
    assert_eq!(pst.test_lsn(), 2);
}

// ------------------------------------- protocol edges, spill and streams ---

fn read_stmt(rd: &mut BufReader<TcpStream>) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let l = read_line(rd).trim_end().to_string();
        let done = l.starts_with("END ") || l.starts_with("OK ") || l.starts_with("ERR ");
        out.push(l);
        if done {
            return out;
        }
    }
}

#[test]
fn network_auth_limits_and_oversized_lines() {
    let e = Env::new("netedge");

    // ---- password gate ------------------------------------------------
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let db = Arc::clone(&e.db);
    let h = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        handle_conn(sock, db, 71, Some("s3cret".to_string()));
    });
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut rd = BufReader::new(sock.try_clone().unwrap());
    sock.write_all(b"SELECT 1\n").unwrap();
    assert!(read_stmt(&mut rd)[0].starts_with("ERR auth"));
    sock.write_all(b"AUTH wrong\n").unwrap();
    assert!(read_stmt(&mut rd)[0].starts_with("ERR auth"));
    sock.write_all(b"AUTH\n").unwrap();
    assert!(read_stmt(&mut rd)[0].starts_with("ERR auth"));
    sock.write_all(b"AUTHX\n").unwrap();
    assert!(read_stmt(&mut rd)[0].starts_with("ERR auth"));
    sock.write_all(b"BAD\n").unwrap();
    assert!(read_stmt(&mut rd)[0].starts_with("ERR auth"));
    sock.write_all(b"  AUTH   s3cret  \n").unwrap();
    assert_eq!(read_stmt(&mut rd)[0], "OK 0");
    sock.write_all(b"SELECT 1\n").unwrap();
    let r = read_stmt(&mut rd);
    assert_eq!(r[0], "ROWS 1");
    assert_eq!(r[1], "1");

    // A complete statement longer than the cap is refused and the next one
    // still runs: the newline was already in the buffer.
    let mut big = vec![b'x'; MAX_LINE + 1000];
    big.push(b'\n');
    big.extend_from_slice(b"SELECT 2\n");
    sock.write_all(&big).unwrap();
    assert!(read_stmt(&mut rd)[0].starts_with("ERR limit"));
    let r = read_stmt(&mut rd);
    assert_eq!(r[0], "ROWS 1");
    assert_eq!(r[1], "2");

    // A partial line that crosses the cap without a newline is answered
    // once and then swallowed up to its terminator.
    let big = vec![b'y'; MAX_LINE + 100_000];
    sock.write_all(&big).unwrap();
    assert!(read_stmt(&mut rd)[0].starts_with("ERR limit"));
    sock.write_all(b"\nSELECT 3\n").unwrap();
    let r = read_stmt(&mut rd);
    assert_eq!(r[0], "ROWS 1");
    assert_eq!(r[1], "3");
    drop(rd);
    drop(sock);
    h.join().unwrap();

    // An empty configured password accepts a bare AUTH.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let db = Arc::clone(&e.db);
    let h = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        handle_conn(sock, db, 72, Some(String::new()));
    });
    let mut sock = TcpStream::connect(addr).unwrap();
    let mut rd = BufReader::new(sock.try_clone().unwrap());
    sock.write_all(b"AUTH\nSELECT 4\n").unwrap();
    assert_eq!(read_stmt(&mut rd)[0], "OK 0");
    assert_eq!(read_stmt(&mut rd)[1], "4");
    drop(rd);
    drop(sock);
    h.join().unwrap();
}

#[test]
fn parse_args_forms_and_fallbacks() {
    let (p, d, r) = parse_args_from(vec![
        "--port".to_string(),
        "7001".to_string(),
        "--data".to_string(),
        "/tmp/x".to_string(),
        "--replicaof".to_string(),
        "host:1".to_string(),
    ]);
    assert_eq!((p, d.as_str(), r.as_deref()), (7001, "/tmp/x", Some("host:1")));

    let (p, d, r) = parse_args_from(vec![
        "--port=7002".to_string(),
        "--data=/tmp/y".to_string(),
        "--replicaof=host:2".to_string(),
        "ignored".to_string(),
    ]);
    assert_eq!((p, d.as_str(), r.as_deref()), (7002, "/tmp/y", Some("host:2")));

    // Missing and invalid values keep the defaults; later flags win.
    let (p, d, _) = parse_args_from(vec![
        "--port".to_string(),
        "notanumber".to_string(),
        "--port".to_string(),
        "7003".to_string(),
        "--data".to_string(),
        "/tmp/z".to_string(),
        "--data=/tmp/w".to_string(),
        "--port".to_string(),
    ]);
    assert_eq!(p, 7003);
    assert_eq!(d, "/tmp/w");

    // The bare call exercises the environment/default paths.
    let (_, d, _) = parse_args_from(vec![]);
    assert!(!d.is_empty());
}

#[test]
fn spill_backed_rows_sort_update_delete_and_alter() {
    // Keep the in-memory row budget tiny so the same code path the memory
    // cap uses in production (spill files) runs on a small fixture.
    env::set_var("SDB_MEM_BUDGET", "262144");
    let e = Env::new("spill");
    e.ok("CREATE TABLE big (id INTEGER, s TEXT)", "0");
    let pad = "z".repeat(400);
    // Other tests may have moved the process-wide spill directory in the
    // meantime (each open installs its own); point it back at this store and
    // insert in batches, so a single unlucky window cannot hide the path.
    let mut spilled = false;
    for batch in 0..4 {
        let mut vals = Vec::new();
        for i in 0..300 {
            let id = batch * 300 + i;
            vals.push(format!("({}, '{}')", id, pad));
        }
        set_spill_dir(e.dir.join("spill"));
        e.ok(&format!("INSERT INTO big VALUES {}", vals.join(",")), "300");
        spilled |= e.db.lock().unwrap().tables["big"].rows.disk_mode();
    }
    assert!(spilled, "rows should be spill-backed");

    // Sequential scans, ORDER BY and random point probes.
    assert_eq!(flat(e.rows("SELECT COUNT(*) FROM big")), vec!["1200"]);
    assert_eq!(
        flat(e.rows("SELECT id FROM big ORDER BY id LIMIT 3")),
        vec!["0", "1", "2"]
    );
    assert_eq!(
        flat(e.rows("SELECT s FROM big WHERE id = 617")),
        vec![pad.clone()]
    );
    assert_eq!(
        flat(e.rows("SELECT id FROM big ORDER BY id DESC LIMIT 2")),
        vec!["1199", "1198"]
    );

    // UPDATE writes a new disk image (row grows), DELETE keeps survivors.
    e.ok("UPDATE big SET s = 'short' WHERE id % 100 = 0", "12");
    assert_eq!(
        flat(e.rows("SELECT s FROM big WHERE id = 600")),
        vec!["short"]
    );
    e.ok("DELETE FROM big WHERE id >= 1100", "100");
    assert_eq!(flat(e.rows("SELECT COUNT(*) FROM big")), vec!["1100"]);

    // Schema changes rewrite every spilled row.
    e.ok("ALTER TABLE big ADD COLUMN note TEXT DEFAULT 'n'", "0");
    assert_eq!(
        flat(e.rows("SELECT note FROM big WHERE id = 7")),
        vec!["n"]
    );
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM big WHERE note = 'n'")),
        vec!["1100"]
    );
    e.ok("ALTER TABLE big DROP COLUMN note", "0");
    assert_eq!(flat(e.rows("SELECT id FROM big WHERE id = 9")), vec!["9"]);

    // A fresh insert keeps appending to the disk image.
    e.ok("INSERT INTO big VALUES (5000, 'late')", "1");
    assert_eq!(
        flat(e.rows("SELECT s FROM big WHERE id = 5000")),
        vec!["late"]
    );
    // Reopening rebuilds the same rows from the log.
    let reopened = Db::open(e.dir.to_str().unwrap());
    assert!(reopened.tables["big"].rows.disk_mode() || reopened.tables["big"].rows.len() == 1101);
}

#[test]
fn streaming_join_aggregate_over_spilled_rows() {
    env::set_var("SDB_MEM_BUDGET", "262144");
    let e = Env::new("joinstream");
    e.ok("CREATE TABLE dim (k INTEGER, label TEXT)", "0");
    let mut dv = Vec::new();
    for k in 0..10 {
        dv.push(format!("({}, 'l{}')", k, k));
    }
    dv.push("(99, 'lone')".to_string());
    e.ok(&format!("INSERT INTO dim VALUES {}", dv.join(",")), "11");
    e.ok("CREATE TABLE fact (id INTEGER, k INTEGER, v INTEGER)", "0");
    let mut spilled = false;
    for batch in 0..4 {
        let mut fv = Vec::new();
        for i in 0..1000 {
            let id = batch * 1000 + i;
            fv.push(format!("({}, {}, {})", id, id % 10, id));
        }
        set_spill_dir(e.dir.join("spill"));
        e.ok(&format!("INSERT INTO fact VALUES {}", fv.join(",")), "1000");
        spilled |= e.db.lock().unwrap().tables["fact"].rows.disk_mode();
    }
    assert!(spilled);

    // Spilled left side, hashed right side.
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM fact f JOIN dim d ON f.k = d.k")),
        vec!["4000"]
    );
    assert_eq!(
        flat(e.rows(
            "SELECT d.label, COUNT(*), SUM(f.v) FROM fact f JOIN dim d ON f.k = d.k \
             GROUP BY d.label ORDER BY d.label LIMIT 3"
        )),
        vec!["l0|400|798000", "l1|400|798400", "l2|400|798800"]
    );
    // Spilled right side, hashed left side.
    assert_eq!(
        flat(e.rows(
            "SELECT COUNT(*) FROM dim d JOIN fact f ON d.k = f.k"
        )),
        vec!["4000"]
    );
    // LEFT JOIN keeps the unmatched dimension row.
    let left = flat(e.rows(
        "SELECT d.k, COUNT(f.id) FROM dim d LEFT JOIN fact f ON d.k = f.k \
         GROUP BY d.k ORDER BY d.k",
    ));
    assert_eq!(left.len(), 11);
    assert_eq!(left[10], "99|0");
    // USING, extra ON conjuncts, comma joins and a non-equality join.
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM fact JOIN dim USING (k)")),
        vec!["4000"]
    );
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM fact f JOIN dim d ON f.k = d.k AND f.v > 100")),
        vec!["3899"]
    );
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM fact f, dim d WHERE f.k = d.k")),
        vec!["4000"]
    );
    // No equality pair: the general join path answers instead.
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM fact f JOIN dim d ON f.k > d.k")),
        vec!["18000"]
    );
    // HAVING, DISTINCT and descending order exercise the aggregator finish.
    assert_eq!(
        flat(e.rows(
            "SELECT DISTINCT d.k, COUNT(*) c FROM fact f JOIN dim d ON f.k = d.k \
             GROUP BY d.k HAVING COUNT(*) > 399 ORDER BY c DESC, d.k ASC LIMIT 3 OFFSET 1"
        )),
        vec!["1|400", "2|400", "3|400"]
    );
    // A type-mismatched equality pair falls back to the general path.
    assert_eq!(
        flat(e.rows("SELECT COUNT(*) FROM fact f JOIN dim d ON f.id = d.label")),
        vec!["0"]
    );
}
