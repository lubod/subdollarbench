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

/// Tickets 7 and 8 need ticket 1, not 2: a model that failed sorting
/// and paging still gets aggregates and joins graded, and those
/// profiles must not emit `ORDER BY` or `LIMIT` (the frame compares as
/// a multiset then).
#[test]
fn aggregates_and_joins_without_stage2_are_unordered() {
    for last in [7u32, 8, 9] {
        let p = Profile::for_passed(&[0, 1, last]);
        let mut selects = 0;
        for seed in 0..40u64 {
            let s = build_script(seed, &p, 150);
            for c in s.setup.iter().chain(&s.steps) {
                let q = lower(&c.stmt.sql);
                if c.stmt.kind != Kind::Select {
                    continue;
                }
                selects += 1;
                assert!(
                    !q.contains("order by"),
                    "stage {last} without 2 emitted ORDER BY: {q}"
                );
                assert!(
                    !q.contains("limit") && !q.contains("offset"),
                    "stage {last} without 2 emitted LIMIT/OFFSET: {q}"
                );
                assert!(
                    !c.stmt.ordered,
                    "an unordered frame must compare as a multiset: {q}"
                );
            }
        }
        assert!(selects > 100, "too few selects generated: {selects}");
    }
}

/// Every generated `INSERT ... SELECT` parenthesizes its user
/// predicate and caps an INTEGER key below 2^62: an unparenthesized
/// `x OR y` used to let the `pk IS NOT NULL` guard be bypassed (NULL
/// keys then auto-assign in scan order), and an i64::MAX key makes
/// SQLite pick *random* rowids.
#[test]
fn insert_select_parenthesizes_and_caps_integer_keys() {
    let p = Profile::through(10);
    let (mut stmts, mut capped) = (0usize, 0usize);
    for seed in 0..200u64 {
        let schema = s_schema(seed);
        let s = build_script(seed, &p, 150);
        for c in s.setup.iter().chain(&s.steps) {
            let q = lower(&c.stmt.sql)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !q.starts_with("insert into") || !q.contains(" select ") {
                continue;
            }
            stmts += 1;
            // No *top-level* `OR` may follow the WHERE: the user
            // predicate is parenthesized precisely so the key guards
            // always apply.
            if let Some(at) = q.find(" where ") {
                let mut depth = 0i32;
                let rest: Vec<char> = q[at + 7..].chars().collect();
                for i in 0..rest.len() {
                    match rest[i] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        ' ' if depth == 0 && rest.get(i + 1..i + 4) == Some(&['o', 'r', ' ']) => {
                            panic!("top-level OR after WHERE: {q}");
                        }
                        _ => {}
                    }
                }
            }
            let target = q
                .trim_start_matches("insert into ")
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_string();
            let int_key = schema
                .tables
                .iter()
                .find(|t| t.name == target)
                .map(|t| {
                    t.columns
                        .iter()
                        .any(|c| c.primary_key && c.ty == Ty::Integer)
                })
                .unwrap_or(false);
            if int_key {
                capped += 1;
                assert!(
                    q.contains("< 4611686018427387904"),
                    "INTEGER key not capped below 2^62: {q}"
                );
            }
        }
    }
    assert!(stmts >= 30, "too few insert-select statements: {stmts}");
    assert!(capped >= 10, "too few INTEGER-key targets: {capped}");
}

/// Ticket 10's subquery shapes: scalar subqueries in select items and
/// comparisons, `[NOT] IN (subquery)`, `IN (list)`/`NOT IN`, `EXISTS`/
/// `NOT EXISTS` (correlated), and the subquery error probes; stage 9
/// must not leak any of it.
#[test]
fn subquery_profile_emits_subqueries_and_probes() {
    let p10 = Profile::through(10);
    let (mut scalar, mut in_sub, mut in_list, mut exists, mut not_forms) = (0, 0, 0, 0, 0);
    let mut probes = String::new();
    for seed in 0..60u64 {
        let s = build_script(seed, &p10, 150);
        assert!(
            s.unknown.is_empty(),
            "{:?}",
            &s.unknown[..s.unknown.len().min(2)]
        );
        assert_eq!(s.discarded, 0, "seed {seed}: discards");
        for c in &s.steps {
            let q = lower(&c.stmt.sql)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if c.stmt.kind == Kind::Probe {
                probes.push_str(&q);
                probes.push('\n');
                continue;
            }
            if c.stmt.kind != Kind::Select && c.stmt.kind != Kind::Update {
                continue;
            }
            let subs = q.matches("(select ").count();
            if subs == 0 {
                continue;
            }
            if q.contains(" in (select ") {
                in_sub += 1;
            }
            if q.contains(" in (") && !q.contains(" in (select ") {
                in_list += 1;
            }
            if q.contains("exists (select ") {
                exists += 1;
            }
            if q.contains(" not ") {
                not_forms += 1;
            }
            if !q.contains(" in (") && !q.contains("exists (select") {
                scalar += 1;
            }
        }
    }
    assert!(scalar >= 20, "scalar subqueries rare: {scalar}");
    assert!(in_sub >= 8, "IN (subquery) rare: {in_sub}");
    assert!(in_list >= 8, "IN (list) rare: {in_list}");
    assert!(exists >= 8, "EXISTS rare: {exists}");
    assert!(not_forms >= 5, "negated forms rare: {not_forms}");
    for needle in [" in (select ", "exists (select ", "zz_gone", "zz_nocol"] {
        assert!(
            probes.contains(needle),
            "subquery probes missing {needle:?}"
        );
    }
    // Stage 9 scripts have no subqueries.
    let p9 = Profile::through(9);
    for seed in 0..30u64 {
        let s = build_script(seed, &p9, 150);
        for c in s.setup.iter().chain(&s.steps) {
            let q = lower(&c.stmt.sql);
            assert!(!q.contains("(select"), "stage 9 leaked a subquery: {q}");
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
            if c.stmt.kind != Kind::Select || q.contains("(select") {
                // A subquery's `FROM` is not the query's join shape.
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
            if c.stmt.kind == Kind::Txn && (sql.contains("commit") || sql.contains("rollback")) {
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
                let sets: Vec<&str> = after.split(" where ").next().unwrap().split(", ").collect();
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
            if c.stmt.kind == Kind::Probe && q.contains("'zz'") && q.matches("), (").count() == 1 {
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

/// Round-15 H2: rename targets and missing-table probes carry the seed
/// namespace, so a persistent store cannot collide with them.
#[test]
fn fresh_table_names_carry_the_seed_namespace() {
    let seed = 0x5EED_u64;
    let schema = crate::verifier::sql::schema::generate(seed, 2, 3);
    let mut g = Generator::new(seed, schema, Profile::through(13));
    let name = g.fresh_table_name("none");
    assert!(name.starts_with(&format!("s{seed:x}_")), "{name}");
}
