//! One stage run: build a differential script, replay it against the
//! candidate over the wire, shrink the first failure, and enforce the
//! verifier's own health floors.
//!
//! The floors are the same ones the self-tests assert (docs/PROPOSAL-sdb-sql.md,
//! "per-stage comparable-check floors"): an unmapped oracle message or a
//! gradeable share below [`GRADEABLE_FLOOR`] means the generator is at
//! fault, not the candidate, so the run fails as a verifier error.

use super::client::TcpCandidate;
use super::generator::{build_script, Profile, Statement};
use super::run::{run_script, Candidate, Verdict};
use super::shrink::{shrink, Repro};
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
/// for later stages are not built yet).
pub fn differential_supported(stage: u32) -> bool {
    stage <= 3
}

/// A deterministic seed for (verification, stage): every verification of a
/// ticket uses a fresh seed, so a repair cannot overfit to the reported
/// example (docs/PROPOSAL-sdb-sql.md, "Fresh seeds").
pub fn stage_seed(verifier_seed: &str, stage: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    verifier_seed.hash(&mut h);
    stage.hash(&mut h);
    h.finish()
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
    /// The minimized failure, when the candidate answered wrongly.
    pub repro: Option<Repro>,
    /// One-line failure summary for logs.
    pub detail: String,
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

/// Run the cumulative differential suite of `passed` stages against the
/// server at `addr`, using `seed`. `Err` is a verifier fault (unmapped
/// oracle message, gradeable share under the floor, or no connection);
/// `Ok` distinguishes pass from failure-with-repro.
pub fn run_stage(
    addr: &str,
    timeout: Duration,
    seed: u64,
    passed: &[u32],
    steps: usize,
) -> Result<StageOutcome, String> {
    let script = build_script(seed, &Profile::for_passed(passed), steps);
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

    let mut cand = TcpCandidate::connect(addr, timeout)?;
    match run_script(&mut cand, &script) {
        Verdict::Pass { checks } => Ok(StageOutcome {
            seed,
            passed: true,
            checks,
            discarded: script.discarded,
            repro: None,
            detail: String::new(),
        }),
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
            let mut make = || -> Box<dyn Candidate> {
                match TcpCandidate::connect(addr, timeout) {
                    Ok(c) => Box::new(c),
                    Err(e) => Box::new(Dead { err: e }),
                }
            };
            let repro = shrink(&statements, &mut make);
            Ok(StageOutcome {
                seed,
                passed: false,
                checks: f.index + 1,
                discarded: script.discarded,
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
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                thread::spawn(move || {
                    let oracle = Oracle::new().unwrap();
                    let mut sock = sock;
                    sock.set_nodelay(true).unwrap();
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap() > 0 {
                        let out = oracle.exec(line.trim_end());
                        assert!(
                            !matches!(out, OracleOutcome::Discard(_) | OracleOutcome::Unknown(_)),
                            "ungradeable answer: {out:?}"
                        );
                        let text = super::super::compare::render_expected(&out);
                        sock.write_all(text.as_bytes()).unwrap();
                        sock.write_all(b"\n").unwrap();
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

    #[test]
    fn stages_0_to_3_pass_against_an_oracle_server() {
        let addr = oracle_server();
        let addr = addr.to_string();
        for stage in 0..=3u32 {
            for seed in [7u64, 8] {
                let passed: Vec<u32> = (0..=stage).collect();
                let out = run_stage(&addr, Duration::from_secs(10), seed, &passed, 150)
                    .unwrap_or_else(|e| panic!("stage {stage} seed {seed}: {e}"));
                assert!(
                    out.passed,
                    "stage {stage} seed {seed}: {}",
                    out.detail
                );
                assert!(out.checks > 100, "stage {stage}: {} checks", out.checks);
                assert!(out.repro.is_none());
            }
        }
    }

    #[test]
    fn a_broken_server_fails_with_a_minimal_repro() {
        let addr = toy_server(Some(Fault::NullsLast)).to_string();
        let mut found = None;
        for seed in 0..60u64 {
            let out = run_stage(&addr, Duration::from_secs(10), seed, &[0, 1, 2, 3], 80).unwrap();
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
        assert!(text.contains("Expected:") && text.contains("Got:"), "{text}");
        // The repro is self-contained and still fails on the same server.
        let mut mk = || -> Box<dyn Candidate> { Box::new(Toy::new(Some(Fault::NullsLast))) };
        assert!(super::super::shrink::first_failure(&repro.statements, &mut mk).is_some());
    }

    #[test]
    fn a_dead_server_is_a_verifier_visible_transport_failure() {
        // Grab a port, then close it: connect is refused.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let err = run_stage(&addr, Duration::from_millis(500), 1, &[0], 10).unwrap_err();
        assert!(err.starts_with("connect:"), "{err}");
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
    }
}
