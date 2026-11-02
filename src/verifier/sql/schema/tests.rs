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

/// Two seeds never share table or column names: stage scripts run back
/// to back against one long-lived server (the cumulative suite), so a
/// collision would corrupt the later script.
#[test]
fn identifiers_are_namespaced_per_seed() {
    let names = |s: &Schema| -> BTreeSet<String> {
        s.tables
            .iter()
            .flat_map(|t| {
                std::iter::once(t.name.clone()).chain(t.columns.iter().map(|c| c.name.clone()))
            })
            .collect()
    };
    let a = generate(42, 4, 5);
    let b = generate(43, 4, 5);
    let (na, nb) = (names(&a), names(&b));
    assert!(na.is_disjoint(&nb), "seeds 42 and 43 share a name");
    assert!(na.iter().all(|n| n.starts_with("s2a_")), "{na:?}");
    assert!(nb.iter().all(|n| n.starts_with("s2b_")), "{nb:?}");
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

#[test]
fn keys_and_uniques_follow_the_documented_invariants() {
    let mut text_pk_seen = false;
    for seed in 0..200u64 {
        let s = generate(seed, 4, 6);
        assert_eq!(s.tables[0].columns[0].ty, Ty::Integer, "seed {seed}");
        for t in &s.tables {
            assert!(
                t.columns.iter().filter(|c| c.unique).count() <= 1,
                "seed {seed}: more than one UNIQUE column in {}",
                t.name
            );
            let pk = &t.columns[0];
            assert!(pk.primary_key && !pk.unique && !pk.not_null && pk.default.is_none());
            text_pk_seen |= pk.ty == Ty::Text;
        }
    }
    assert!(text_pk_seen, "no TEXT primary key in 200 seeds");
}
