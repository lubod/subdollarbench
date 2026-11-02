use super::super::oracle::Oracle;
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

/// One oracle-backed server with a shared database, like a real
/// candidate: the data survives connections and (in the restart tests)
/// only if the server keeps it.
fn oracle_server() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::Mutex<Oracle>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let oracle = std::sync::Arc::new(std::sync::Mutex::new(Oracle::new().unwrap()));
    let shared = oracle.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(sock) = stream else { break };
            let oracle = oracle.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                sock.set_nodelay(true).unwrap();
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    let out = oracle.lock().unwrap().exec(line.trim_end());
                    let text = if out.is_discard() {
                        "ERR syntax".to_string()
                    } else {
                        render_expected(&out)
                    };
                    if sock.write_all(format!("{text}\n").as_bytes()).is_err() {
                        return;
                    }
                    line.clear();
                }
            });
        }
    });
    (addr, shared)
}

#[test]
fn sentinel_round_trips_against_a_fresh_store() {
    let sent = build(11, 4, &Profile::through(4)).expect("sentinel builds");
    assert!(!sent.writes.is_empty() && !sent.queries.is_empty());
    let (addr, _db) = oracle_server();
    let addr = addr.to_string();
    write(&addr, Duration::from_secs(5), &sent).expect("write");
    check(&addr, Duration::from_secs(5), &sent).expect("check after write");
}

#[test]
fn sentinel_check_catches_a_lost_store() {
    let sent = build(12, 4, &Profile::through(4)).expect("sentinel builds");
    // A fresh server never saw the writes: the reads must fail.
    let (addr, _db) = oracle_server();
    let err = check(&addr.to_string(), Duration::from_secs(5), &sent).unwrap_err();
    assert!(err.starts_with("drift:"), "{err}");
}

/// A store that answers the first read and then breaks is *drifting*,
/// not "not checkable": swallowing it as unreachable used to hide the
/// regression behind the new tables' suite.
#[test]
fn check_after_an_answer_is_drift_not_unreachable() {
    let sent = build(16, 4, &Profile::through(4)).unwrap();
    assert!(
        sent.queries.len() >= 2,
        "the fixture needs two reads to answer one and break on the next"
    );
    let expected = sent.queries[0].expected.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(sock) = stream else { break };
            let expected = expected.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                if matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    // The first read answers exactly as recorded...
                    let _ = sock.write_all(format!("{expected}\n").as_bytes());
                }
                // ...then the connection vanishes: the next read is
                // unanswered on a store that demonstrably answered.
            });
        }
    });
    let err = check(&addr, Duration::from_secs(5), &sent).unwrap_err();
    assert!(err.starts_with("drift:"), "{err}");
    assert!(err.contains("after 1 answered read"), "{err}");
}

#[test]
fn sentinel_is_deterministic_and_seed_namespaced() {
    let a = build(13, 4, &Profile::through(4)).unwrap();
    let b = build(13, 4, &Profile::through(4)).unwrap();
    let c = build(14, 4, &Profile::through(4)).unwrap();
    assert_eq!(a, b);
    assert_ne!(a.writes, c.writes);
    // Reads are ordered: the recorded frame compares byte for byte.
    assert!(a
        .queries
        .iter()
        .all(|q| q.sql.to_lowercase().contains("order by")));
}

/// Review round 14, G1: a ladder that failed tickets 1-3 is graded on
/// tickets 0 and 4 only, so ticket 4's dataset must not need a column
/// list, `WHERE`, `ORDER BY` or `LIMIT` — and its reads still prove the
/// data is there, as a multiset of rows.
#[test]
fn sentinel_without_tickets_1_to_3_uses_only_their_grammar() {
    let p = Profile::for_passed(&[0, 4]);
    for seed in 0..20u64 {
        let sent = build(seed, 4, &p).expect("sentinel builds without ORDER BY");
        assert!(!sent.writes.is_empty() && !sent.queries.is_empty());
        for w in &sent.writes {
            let w = w.to_lowercase();
            if let (Some(into), Some(values)) = (w.find("into"), w.find("values")) {
                assert!(!w[into..values].contains('('), "column list: {w}");
            }
        }
        for q in &sent.queries {
            assert!(!q.ordered, "{}", q.sql);
            let sql = q.sql.to_lowercase();
            for kw in ["order", "limit", "where", "distinct"] {
                assert!(!sql.contains(kw), "{kw} in {sql}");
            }
        }
    }
    // And it round-trips against a correct server.
    let sent = build(3, 4, &p).unwrap();
    let (addr, _db) = oracle_server();
    let addr = addr.to_string();
    write(&addr, Duration::from_secs(5), &sent).expect("write");
    check(&addr, Duration::from_secs(5), &sent).expect("check after write");
    // A fresh store still drifts.
    let (fresh, _db) = oracle_server();
    let err = check(&fresh.to_string(), Duration::from_secs(5), &sent).unwrap_err();
    assert!(
        err.starts_with("drift:") && err.contains("rows in any order"),
        "{err}"
    );
}

/// With ticket 2 passed the sentinel is exactly what it always was: only
/// totally ordered reads, compared byte for byte.
#[test]
fn sentinel_with_order_by_keeps_only_ordered_reads() {
    let sent = build(13, 4, &Profile::through(4)).unwrap();
    assert!(sent.queries.iter().all(|q| q.ordered));
    // A sentinel saved before `ordered` existed loads as ordered.
    let mut json = serde_json::to_value(&sent).unwrap();
    for q in json["queries"].as_array_mut().unwrap() {
        q.as_object_mut().unwrap().remove("ordered");
    }
    let old: Sentinel = serde_json::from_value(json).unwrap();
    assert_eq!(old, sent);
}

#[test]
fn unordered_reads_compare_rows_as_a_multiset() {
    let want = "ROWS 2\na\tb\n1\tx\n2\t\\N\n1\tx\nEND 3";
    let shuffled = "ROWS 2\na\tb\n2\t\\N\n1\tx\n1\tx\nEND 3";
    assert!(same_answer(shuffled, want, false));
    assert!(
        !same_answer(shuffled, want, true),
        "ordered reads stay byte for byte"
    );
    // A lost duplicate, a changed value, header or count is drift.
    for bad in [
        "ROWS 2\na\tb\n2\t\\N\n1\tx\nEND 2",
        "ROWS 2\na\tb\n2\t\\N\n1\tx\n1\ty\nEND 3",
        "ROWS 2\na\tc\n2\t\\N\n1\tx\n1\tx\nEND 3",
        "ROWS 2\na\tb\n2\t\\N\n1\tx\n1\tx\nEND 4",
        "ERR syntax",
    ] {
        assert!(!same_answer(bad, want, false), "{bad:?}");
    }
    assert!(same_answer("ROWS 1\na\nEND 0", "ROWS 1\na\nEND 0", false));
    assert!(same_answer("ERR syntax", "ERR syntax", false));
}

/// R36 F3: the four known empty seeds fall through to a later seed.
#[test]
fn r36_f3_empty_seed_moves_on() {
    let profile = Profile::through(4);
    for seed in [
        9231256963851754504u64,
        5445690653175984392,
        12529112926788068867,
        15487456800110868460,
    ] {
        let sent = build(seed, 4, &profile).expect("a later seed must build");
        assert!(
            sent.seed > seed,
            "bad seed {seed} did not move on (used {})",
            sent.seed
        );
        assert!(!sent.writes.is_empty() && !sent.queries.is_empty());
    }
}

/// R36 F3: good seeds keep exactly the same dataset; these hashes were
/// recorded before the retry wrapper was added.
#[test]
fn r36_f3_good_seed_unchanged() {
    use sha2::{Digest, Sha256};
    let profile = Profile::through(4);
    let expected = [
        (
            1u64,
            "fdb5cc4ed37a127c3f0f3a511c3fb9068cddf88be65fa7eafa65600953165cc8",
        ),
        (
            2u64,
            "8b4c319d8571f10490fd3ed218c4af2005694593de20cbb739daaccfa88da786",
        ),
        (
            3u64,
            "c14e9eeb7307ccc94d4ee62824fb6b188204d02b9197aceb871e482e785eab21",
        ),
    ];
    for (seed, want) in expected {
        let sent = build(seed, 4, &profile).expect("good seed");
        assert_eq!(sent.seed, seed, "a good seed must not move");
        let txt = serde_json::to_string(&sent).unwrap();
        let mut h = Sha256::new();
        h.update(txt.as_bytes());
        assert_eq!(format!("{:x}", h.finalize()), want, "seed {seed} changed");
    }
}

/// R36 F3: the full scan is intentionally ignored (20k builds). The
/// known bad seeds and good-seed fixtures above are the gate evidence.
#[test]
#[ignore = "20k sentinel builds; run manually for the scan evidence"]
fn r36_f3_scan_20k_no_fault() {
    let profile = Profile::through(4);
    for seed in 0u64..20_000 {
        build(seed, 4, &profile).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    }
}

#[test]
fn load_all_skips_junk_and_sorts() {
    let dir = std::env::temp_dir().join(format!("sdb-sentinel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    save(
        &dir.join("sentinel-4.json"),
        &build(15, 4, &Profile::through(4)).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.join("sentinel-x.json"), "{not json").unwrap();
    std::fs::write(dir.join("other.json"), "{}").unwrap();
    let all = load_all(&dir);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].stage, 4);
    let _ = std::fs::remove_dir_all(&dir);
}
