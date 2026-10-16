//! One stage run: build a differential script, replay it against the
//! candidate over the wire, shrink the first failure, and enforce the
//! verifier's own health floors.
//!
//! The floors are the same ones the self-tests assert (docs/PROPOSAL-sdb-sql.md,
//! "per-stage comparable-check floors"): an unmapped oracle message or a
//! gradeable share below [`GRADEABLE_FLOOR`] means the generator is at
//! fault, not the candidate, so the run fails as a verifier error.

use super::client::TcpCandidate;
use super::compare::{render_expected, render_response, MismatchKind};
use super::generator::{build_script, Profile, Statement};
use super::run::{run_script, Candidate, Verdict};
use super::scenarios::ScenarioResult;
use super::shrink::{first_failure, shrink_with_budget, Repro};
use super::wire::Response;
use std::time::Duration;

/// Fraction of generated statements that must be gradeable (not discarded
/// by the oracle). Below this the run is a verifier fault.
pub const GRADEABLE_FLOOR: f64 = 0.9;

/// Step statements per stage script (setup adds the schema seeds on top).
pub const DEFAULT_STEPS: usize = 150;

/// Ladder titles (docs/PROPOSAL-sdb-sql.md); used in stage results and logs.
pub fn stage_title(stage: u32) -> &'static str {
    match stage {
        0 => "Genesis: tables, insert, select",
        1 => "Expressions and WHERE",
        2 => "Sorting and paging",
        3 => "UPDATE and DELETE",
        4 => "Persistence",
        5 => "Crash safety",
        6 => "Transactions",
        7 => "Aggregates",
        8 => "Joins",
        9 => "Constraints",
        10 => "Subqueries",
        11 => "Indexes + perf gate",
        12 => "Concurrency and isolation",
        13 => "Schema change",
        14 => "Observability",
        15 => "Tests and CI gate",
        16 => "Larger than memory",
        17 => "Incident: disk full",
        18 => "Hostile input and limits",
        19 => "Replication and failover",
        20 => "PostgreSQL wire protocol",
        _ => "unknown stage",
    }
}

/// Stages the differential verifier can run today (the scripted scenarios
/// for later stages are not built yet). Stages 4-5 add no grammar; stage 6
/// adds transactions.
pub fn differential_supported(stage: u32) -> bool {
    stage <= 8
}

/// A deterministic seed for (verification, stage): every verification of a
/// ticket uses a fresh seed, so a repair cannot overfit to the reported
/// example (docs/PROPOSAL-sdb-sql.md, "Fresh seeds").
///
/// FNV-1a, not `DefaultHasher`: std does not promise the latter is stable
/// across Rust releases, and a recorded seed must replay the same script
/// after a toolchain upgrade.
pub fn stage_seed(verifier_seed: &str, stage: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in verifier_seed
        .as_bytes()
        .iter()
        .chain(&[0xff])
        .chain(&stage.to_le_bytes())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The result of one differential stage run.
#[derive(Debug, Clone)]
pub struct StageOutcome {
    pub seed: u64,
    pub passed: bool,
    /// Statements graded before the run stopped (all of them on a pass).
    pub checks: usize,
    /// Statements the oracle discarded; never sent to the candidate.
    pub discarded: usize,
    /// The failure kind, when the candidate answered wrongly.
    pub kind: Option<MismatchKind>,
    /// The repro (minimized when a reset was available, else the prefix).
    pub repro: Option<Repro>,
    /// One-line failure summary for logs.
    pub detail: String,
}

/// Wait until the candidate answers one probe statement at the protocol
/// level. A published port is not readiness: docker's proxy accepts while
/// the server inside is still starting (or just after it died), and the
/// first real write then gets a reset — the exact failure that made the
/// shrink reset untrusted (review finding 2). Any parseable answer counts,
/// including an error class: stage-0 candidates may reject `SELECT 1`.
pub fn wait_until_answering(addr: &str, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(mut c) = TcpCandidate::connect(addr, Duration::from_secs(2)) {
            if c.exec("SELECT 1").is_ok() {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("candidate at {addr} never answered a probe"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A candidate whose connection could not be established: every statement
/// is a transport error, so `shrink` treats trials as "does not reproduce"
/// instead of aborting the minimizer when the server dies mid-shrink.
struct Dead {
    err: String,
}

impl Candidate for Dead {
    fn exec(&mut self, _sql: &str) -> Result<Response, String> {
        Err(self.err.clone())
    }
}

/// Trial budget for a live-candidate shrink: every trial restarts the
/// candidate, so this is deliberately small (a failing stage is rare and a
/// coarse repro is better than a bogus one).
pub const SHRINK_CHECKS: usize = 60;

/// Run the cumulative differential suite of `passed` stages against the
/// server at `addr`, using `seed`. `Err` is a verifier fault (unmapped
/// oracle message, gradeable share under the floor); `Ok` distinguishes
/// pass from failure-with-repro. A server that cannot be reached is a
/// candidate failure, not a verifier fault: a model whose server never
/// binds (or died during an earlier stage) failed the ticket.
///
/// The highest stage in `passed` selects the suite; its scripted scenarios
/// ([`super::scenarios::for_stage`]) run after the differential script
/// passes.
///
/// `reset` restarts the candidate with a fresh database. It is required for
/// a sound shrink: a replay against left-over state answers `exists` and
/// the minimizer happily reports that as the bug (live cell 2026-10-16).
/// Without a reset — or when a reset does not reproduce the failure — the
/// repro is the original run's prefix: bigger, but always what actually
/// happened.
///
/// `restart` stops and starts the candidate *keeping its store*: stage 4+
/// suites include the persistence scenario, which needs it. `None` skips
/// that scenario (eval/replay and tests without container control).
pub fn run_stage(
    addr: &str,
    timeout: Duration,
    seed: u64,
    passed: &[u32],
    steps: usize,
    reset: Option<&mut dyn FnMut() -> Result<(), String>>,
    restart: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<StageOutcome, String> {
    run_stage_profiled(
        addr,
        timeout,
        seed,
        passed.iter().copied().max().unwrap_or(0),
        &Profile::for_passed(passed),
        steps,
        reset,
        restart,
    )
}

/// The cumulative suite at the variant each stage passed at: a lite pass
/// contributes its lite subset to the grammar (for example
/// `join-inner-only` keeps comma/inner joins and drops `LEFT JOIN`), so a
/// later ticket never demands what the lite prompt did not ask for.
#[allow(clippy::too_many_arguments)]
pub fn run_stage_with_variants(
    addr: &str,
    timeout: Duration,
    seed: u64,
    passed: &[u32],
    variants: &[(u32, Vec<String>)],
    steps: usize,
    reset: Option<&mut dyn FnMut() -> Result<(), String>>,
    restart: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<StageOutcome, String> {
    let mut profile = Profile::for_passed(passed);
    for (stage, flags) in variants {
        if passed.contains(stage) {
            profile.narrow(flags);
        }
    }
    run_stage_profiled(
        addr,
        timeout,
        seed,
        passed.iter().copied().max().unwrap_or(0),
        &profile,
        steps,
        reset,
        restart,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_stage_profiled(
    addr: &str,
    timeout: Duration,
    seed: u64,
    stage: u32,
    profile: &Profile,
    steps: usize,
    reset: Option<&mut dyn FnMut() -> Result<(), String>>,
    restart: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<StageOutcome, String> {
    let script = build_script(seed, profile, steps);
    if !script.unknown.is_empty() {
        return Err(format!(
            "unmapped oracle messages: {:?}",
            &script.unknown[..script.unknown.len().min(3)]
        ));
    }
    let sent = script.steps.len() + script.discarded;
    let gradeable = script.steps.len() as f64 / sent.max(1) as f64;
    if gradeable < GRADEABLE_FLOOR {
        return Err(format!(
            "only {:.0}% of the generated statements are gradeable (floor {:.0}%)",
            gradeable * 100.0,
            GRADEABLE_FLOOR * 100.0
        ));
    }

    let failed = |checks: usize, kind: MismatchKind, detail: String| StageOutcome {
        seed,
        passed: false,
        checks,
        discarded: script.discarded,
        kind: Some(kind),
        repro: None,
        detail,
    };
    if let Err(e) = wait_until_answering(addr, timeout * 3) {
        return Ok(failed(
            0,
            MismatchKind::Protocol,
            format!("server unreachable ({e})"),
        ));
    }
    let mut cand = match TcpCandidate::connect(addr, timeout) {
        Ok(c) => c,
        Err(e) => {
            return Ok(failed(
                0,
                MismatchKind::Protocol,
                format!("server unreachable ({e})"),
            ))
        }
    };
    match run_script(&mut cand, &script) {
        Verdict::Pass { checks } => {
            drop(cand);
            if let ScenarioResult::Fail(d) = super::scenarios::for_stage(stage, addr, timeout, seed)
            {
                return Ok(failed(checks, MismatchKind::Protocol, d));
            }
            // Stage 4's suite adds the restart scenario: the candidate must
            // come back on the same store it wrote.
            if stage >= 4 {
                if let Some(restart) = restart {
                    match super::scenarios::persistence(addr, timeout, seed, restart) {
                        ScenarioResult::Pass => {}
                        ScenarioResult::Fail(d) => {
                            return Ok(failed(checks, MismatchKind::Protocol, d))
                        }
                        ScenarioResult::Fault(e) => return Err(e),
                    }
                }
            }
            Ok(StageOutcome {
                seed,
                passed: true,
                checks,
                discarded: script.discarded,
                kind: None,
                repro: None,
                detail: String::new(),
            })
        }
        Verdict::Fail(f) => {
            let detail = format!("{:?}: {}", f.mismatch.kind, f.mismatch.detail);
            // Every shrink trial re-grades on a fresh oracle *and* a fresh
            // candidate, so the connection that just failed is dropped.
            drop(cand);
            let statements: Vec<Statement> = script
                .setup
                .iter()
                .chain(&script.steps)
                .map(|c| c.stmt.clone())
                .collect();
            // Always available and always truthful: the prefix the
            // candidate actually ran when it answered wrongly.
            let fallback = || Repro {
                statements: statements[..=f.index].to_vec(),
                kind: f.mismatch.kind,
                detail: f.mismatch.detail.clone(),
                expected: render_expected(&f.expected),
                got: render_response(&f.got),
                checks: 0,
            };
            let repro = match reset {
                Some(reset) => {
                    let mut make = || -> Box<dyn Candidate> {
                        match reset() {
                            Ok(()) => match TcpCandidate::connect(addr, timeout) {
                                Ok(c) => Box::new(c),
                                Err(e) => Box::new(Dead { err: e }),
                            },
                            Err(e) => Box::new(Dead {
                                err: format!("reset: {e}"),
                            }),
                        }
                    };
                    // Only trust the reset if a full replay on the reset
                    // state fails at the same statement with the same kind.
                    let trusted = match first_failure(&statements, &mut make) {
                        Some(replay) => {
                            replay.index == f.index && replay.mismatch.kind == f.mismatch.kind
                        }
                        None => false,
                    };
                    if trusted {
                        shrink_with_budget(&statements, &mut make, SHRINK_CHECKS)
                            .or_else(|| Some(fallback()))
                    } else {
                        Some(fallback())
                    }
                }
                None => Some(fallback()),
            };
            Ok(StageOutcome {
                seed,
                passed: false,
                checks: f.index + 1,
                discarded: script.discarded,
                kind: Some(f.mismatch.kind),
                repro,
                detail,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::compare::render_response;
    use super::super::oracle::{Oracle, OracleOutcome};
    use super::super::toy::{Fault, Toy};
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

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
                        if sock.write_all(text.as_bytes()).is_err()
                            || sock.write_all(b"\n").is_err()
                        {
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
}
