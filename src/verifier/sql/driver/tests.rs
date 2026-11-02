use super::super::compare::render_response;
use super::super::oracle::{Oracle, OracleOutcome};
use super::super::toy::{Fault, Toy};
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;

/// Round-16 I1: the post-failover suite is built from the cumulative
/// profile narrowed by the lite variant each ticket passed at. A lite
/// ticket 8 (`join-inner-only`) must not generate LEFT JOIN.
#[test]
fn lite_variants_narrow_the_post_failover_suite() {
    let passed: Vec<u32> = (0..=13).collect();
    let variants = vec![(8u32, vec!["join-inner-only".to_string()])];
    let profile = effective_profile(&passed, &variants);
    for seed in 0..200u64 {
        let script = build_script(seed, &profile, 120);
        for st in script.setup.iter().chain(&script.steps) {
            assert!(
                !st.stmt.sql.to_ascii_uppercase().contains("LEFT JOIN"),
                "seed {seed}: LEFT JOIN in a post-failover lite-8 suite: {}",
                st.stmt.sql
            );
        }
    }
}

/// Review round 14, G1: the persistence dataset and the upgrade
/// sentinel use only the grammar the ladder passed, and neither exists
/// without ticket 4. A full ladder keeps exactly the old profile, so
/// every cell that passed tickets 0-4 grades as before.
#[test]
fn persistence_and_sentinel_profiles_follow_the_passed_tickets() {
    let full: Vec<u32> = (0..=4).collect();
    assert_eq!(persistence_profile(&full, &[]), Some(Profile::through(4)));
    let later: Vec<u32> = vec![0, 1, 2, 3, 4, 5, 6, 8];
    assert_eq!(persistence_profile(&later, &[]), Some(Profile::through(4)));
    assert_eq!(persistence_profile(&[0, 1, 3, 9], &[]), None);
    assert_eq!(sentinel_profile(&[0, 1, 3, 9], &[]), None);

    // Live ling `.17` cell: tickets 1-3 failed or skipped.
    let p = persistence_profile(&[0, 4], &[]).unwrap();
    assert_eq!(p, Profile::for_passed(&[0]));
    assert!(!p.column_list_insert && !p.where_clause && !p.order_limit);
    // Live DeepSeek `_103729`: ticket 2 failed, so no ORDER BY.
    let p = persistence_profile(&[0, 1, 3, 4], &[]).unwrap();
    assert!(p.column_list_insert && p.update && !p.order_limit && !p.distinct);

    // Live DeepSeek `_142825`: ticket 7 failed, ticket 8 passed. Its
    // sentinel must not need aggregates (it was never written).
    let s = sentinel_profile(&later, &[]).unwrap();
    assert!(s.joins && s.txn && !s.aggregates);
    assert_eq!(sentinel_profile(&full, &[]), Some(Profile::through(4)));
    // A lite pass narrows the sentinel too.
    let lite = vec![(8u32, vec!["join-inner-only".to_string()])];
    let s = sentinel_profile(&later, &lite).unwrap();
    assert!(s.joins && !s.left_join);
}

/// A server that answers every connection from a fresh oracle; this is
/// what a correct candidate looks like on the wire.
fn oracle_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // One database for every connection, like a real server: the
    // stage-0 scenario checks that concurrent clients see each other.
    let oracle = std::sync::Arc::new(std::sync::Mutex::new(Oracle::new().unwrap()));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(sock) = stream else { break };
            let oracle = oracle.clone();
            thread::spawn(move || {
                let mut sock = sock;
                sock.set_nodelay(true).unwrap();
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                // Hostile input (invalid UTF-8, an unterminated line)
                // ends the connection, never the server.
                while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    let out = oracle.lock().unwrap().exec(line.trim_end());
                    let text = match out {
                        OracleOutcome::Discard(_) | OracleOutcome::Unknown(_) => {
                            "ERR syntax".to_string()
                        }
                        out => super::super::compare::render_expected(&out),
                    };
                    if sock.write_all(text.as_bytes()).is_err() || sock.write_all(b"\n").is_err() {
                        break;
                    }
                    line.clear();
                }
            });
        }
    });
    addr
}

/// A server backed by the mutant toy, with a fresh toy per connection
/// (the shrink trials reconnect).
fn toy_server(fault: Option<Fault>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(sock) = stream else { break };
            thread::spawn(move || {
                let mut toy = Toy::new(fault);
                let mut sock = sock;
                sock.set_nodelay(true).unwrap();
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 {
                    let resp = toy.exec(line.trim_end()).unwrap();
                    let text = render_response(&Ok(resp));
                    sock.write_all(text.as_bytes()).unwrap();
                    sock.write_all(b"\n").unwrap();
                    line.clear();
                }
            });
        }
    });
    addr
}

/// A server that keeps one engine across every connection: the state a
/// real candidate has, and the reason shrink needs a reset.
fn shared_toy_server(fault: Option<Fault>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let toy = std::sync::Arc::new(std::sync::Mutex::new(Toy::new(fault)));
        for stream in listener.incoming() {
            let Ok(sock) = stream else { break };
            let toy = toy.clone();
            thread::spawn(move || {
                let mut sock = sock;
                sock.set_nodelay(true).unwrap();
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 {
                    let resp = toy.lock().unwrap().exec(line.trim_end()).unwrap();
                    let text = render_response(&Ok(resp));
                    sock.write_all(text.as_bytes()).unwrap();
                    sock.write_all(b"\n").unwrap();
                    line.clear();
                }
            });
        }
    });
    addr
}

#[test]
fn stages_0_to_3_pass_against_an_oracle_server() {
    let addr = oracle_server();
    let addr = addr.to_string();
    for stage in 0..=3u32 {
        for tag in ["a", "b"] {
            // One long-lived server, so every run needs its own seed
            // (and namespace), exactly as the pipeline derives them.
            let seed = stage_seed(tag, stage);
            let passed: Vec<u32> = (0..=stage).collect();
            let out = run_stage(
                &addr,
                Duration::from_secs(10),
                seed,
                &passed,
                150,
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("stage {stage} seed {seed}: {e}"));
            assert!(out.passed, "stage {stage} seed {seed}: {}", out.detail);
            assert!(out.checks > 100, "stage {stage}: {} checks", out.checks);
            assert!(out.repro.is_none());
        }
    }
}

#[test]
fn a_broken_server_fails_with_a_minimal_repro() {
    // The toy server gives every connection a fresh engine, so a no-op
    // reset is truthful and the shrink path is exercised.
    let addr = toy_server(Some(Fault::NullsLast)).to_string();
    let mut found = None;
    for seed in 0..60u64 {
        let mut reset = || Ok(());
        let out = run_stage(
            &addr,
            Duration::from_secs(10),
            seed,
            &[0, 1, 2, 3],
            80,
            Some(&mut reset),
            None,
        )
        .unwrap();
        if !out.passed {
            found = Some(out);
            break;
        }
    }
    let out = found.expect("NullsLast must fail some seed");
    assert!(!out.detail.is_empty());
    let repro = out.repro.expect("a failing run must shrink to a repro");
    assert!(
        repro.statements.len() <= 8,
        "repro has {} statements:\n{}",
        repro.statements.len(),
        repro.render()
    );
    let text = repro.render();
    assert!(
        text.contains("Expected:") && text.contains("Got:"),
        "{text}"
    );
    // The repro is self-contained and still fails on a fresh engine.
    let mut mk = || -> Box<dyn Candidate> { Box::new(Toy::new(Some(Fault::NullsLast))) };
    assert!(super::super::shrink::first_failure(&repro.statements, &mut mk).is_some());
}

/// A candidate that keeps state across connections cannot be replayed:
/// shrink trials against left-over state answer `exists` at the first
/// CREATE and the minimizer reports that nonsense instead of the real
/// failure (live cell 2026-10-16). With no reset the driver must fall
/// back to the original run's prefix, which still reproduces.
#[test]
fn a_stateful_server_without_reset_falls_back_to_the_prefix_repro() {
    let mut found = None;
    for seed in 0..60u64 {
        // A fresh server per attempt: the run itself must start from an
        // empty database, or its "original failure" is already an
        // artifact.
        let addr = shared_toy_server(Some(Fault::NullsLast)).to_string();
        let out = run_stage(
            &addr,
            Duration::from_secs(10),
            seed,
            &[0, 1, 2, 3],
            80,
            None,
            None,
        )
        .unwrap();
        if !out.passed {
            found = Some(out);
            break;
        }
    }
    let out = found.expect("NullsLast must fail some seed");
    assert!(out.kind.is_some());
    let repro = out.repro.expect("a failing run must still carry a repro");
    assert_eq!(repro.checks, 0, "prefix repros are not minimized");
    assert_eq!(Some(repro.kind), out.kind);
    // The prefix ends at the statement that answered wrongly...
    assert_eq!(repro.statements.len(), out.checks);
    // ... and still reproduces on a fresh engine.
    let mut mk = || -> Box<dyn Candidate> { Box::new(Toy::new(Some(Fault::NullsLast))) };
    let replay = super::super::shrink::first_failure(&repro.statements, &mut mk)
        .expect("the prefix must reproduce on a fresh engine");
    assert_eq!(replay.index + 1, repro.statements.len());
}

/// A reset that cannot be trusted (it does not reproduce the failure)
/// must not feed the shrinker either.
#[test]
fn a_broken_reset_falls_back_to_the_prefix_repro() {
    let addr = toy_server(Some(Fault::NullsLast)).to_string();
    let mut found = None;
    for seed in 0..60u64 {
        let mut reset = || Err::<(), String>("restart failed".to_string());
        let out = run_stage(
            &addr,
            Duration::from_secs(10),
            seed,
            &[0, 1, 2, 3],
            80,
            Some(&mut reset),
            None,
        )
        .unwrap();
        if !out.passed {
            found = Some(out);
            break;
        }
    }
    let out = found.expect("NullsLast must fail some seed");
    let repro = out.repro.expect("a failing run must still carry a repro");
    assert_eq!(repro.checks, 0, "an untrusted reset disables shrinking");
    assert!(repro.statements.len() > 1, "not the bogus single-CREATE");
}

#[test]
fn a_dead_server_is_a_candidate_failure_not_a_verifier_fault() {
    // Grab a port, then close it: connect is refused. A model whose
    // server never binds failed the ticket; reporting it as a verifier
    // fault would score it as "measured nothing" and skip the lite try.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let out = run_stage(&addr, Duration::from_millis(500), 1, &[0], 10, None, None)
        .expect("an unreachable server is graded, not a verifier fault");
    assert!(!out.passed);
    assert_eq!(out.kind, Some(MismatchKind::Protocol));
    assert!(out.detail.contains("server unreachable"), "{}", out.detail);
}

#[test]
fn stage_seeds_are_stable_and_distinct() {
    let a = stage_seed("verify-abc", 0);
    let b = stage_seed("verify-abc", 0);
    let c = stage_seed("verify-abc", 1);
    let d = stage_seed("verify-xyz", 0);
    assert_eq!(a, b, "same input, same seed");
    assert_ne!(a, c, "different stage, different seed");
    assert_ne!(a, d, "different verification, different seed");
    // Pinned: a recorded seed must replay the same script forever.
    assert_eq!(a, 10_323_051_711_405_321_790);
}
