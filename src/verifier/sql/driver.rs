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
/// The highest stage whose suite the differential generator covers. The
/// soak derives from it, so it cannot fall behind a ticket again.
pub const MAX_DIFFERENTIAL_STAGE: u32 = 13;

pub fn differential_supported(stage: u32) -> bool {
    // Stage 12 is scenario-only: it sits below the ceiling but has no
    // grammar of its own, so its suite is the earlier differential profile
    // plus the concurrency checks.
    stage <= MAX_DIFFERENTIAL_STAGE && !scenario_only(stage)
}

/// Stages whose contract is a scripted scenario rather than new differential
/// grammar. Ticket 12 (concurrency) is the first; later tickets extend this
/// as their scenarios land.
pub fn scenario_only(stage: u32) -> bool {
    matches!(stage, 12 | 14 | 15 | 16 | 17 | 18 | 19 | 20)
}

/// Whether the verifier can grade this stage at all: the differential
/// ladder through [`MAX_DIFFERENTIAL_STAGE`], plus the scenario-only
/// tickets built so far.
pub fn stage_supported(stage: u32) -> bool {
    differential_supported(stage) || scenario_only(stage)
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
    /// R37 F5: the persistence sentinel seed actually used, when the suite
    /// ran the stage-4 restart scenario. Harness evidence, not candidate
    /// feedback.
    pub sentinel_seed: Option<u64>,
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
/// `restart` stops and starts the candidate *keeping its store*: a suite
/// that includes ticket 4 runs the persistence scenario, which needs it.
/// `None` skips that scenario (eval/replay and tests without container
/// control).
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
        persistence_profile(passed, &[]).as_ref(),
        steps,
        reset,
        restart,
    )
}

/// The grammar a suite of `passed` stages grades: their cumulative profile,
/// narrowed by the lite variant each of them passed at.
pub fn effective_profile(passed: &[u32], variants: &[(u32, Vec<String>)]) -> Profile {
    let mut profile = Profile::for_passed(passed);
    for (stage, flags) in variants {
        if passed.contains(stage) {
            profile.narrow(flags);
        }
    }
    profile
}

/// The grammar of ticket 4's persistence dataset: tickets 0-4, minus the
/// ones this ladder did not pass (review round 14, G1: rules `.17` tells
/// the model a failed ticket is not required, so the dataset must not use
/// it). `None` when ticket 4 itself is not in `passed`: its scenario is
/// then not part of the suite. With 0-4 all passed this is exactly
/// `Profile::through(4)`, the profile the dataset always used.
pub fn persistence_profile(passed: &[u32], variants: &[(u32, Vec<String>)]) -> Option<Profile> {
    if !passed.contains(&4) {
        return None;
    }
    let early: Vec<u32> = passed.iter().copied().filter(|&s| s <= 4).collect();
    Some(effective_profile(&early, variants))
}

/// The grammar of the upgrade sentinel a passing suite leaves behind: the
/// suite itself. `None` unless ticket 4 is in it: the sentinel is re-read
/// after restarts, so it rests on ticket 4's contract.
pub fn sentinel_profile(passed: &[u32], variants: &[(u32, Vec<String>)]) -> Option<Profile> {
    passed
        .contains(&4)
        .then(|| effective_profile(passed, variants))
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
    run_stage_profiled(
        addr,
        timeout,
        seed,
        passed.iter().copied().max().unwrap_or(0),
        &effective_profile(passed, variants),
        persistence_profile(passed, variants).as_ref(),
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
    persistence: Option<&Profile>,
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
        sentinel_seed: None,
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
            // A suite that includes ticket 4 adds the restart scenario: the
            // candidate must come back on the same store it wrote. A suite
            // without it (ticket 4 failed or skipped) does not: gating on
            // the highest stage used to grade ticket 9 on persistence.
            let mut sentinel_seed = None;
            if let (Some(pprofile), Some(restart)) = (persistence, restart) {
                match super::scenarios::persistence_with_seed(
                    addr,
                    timeout,
                    seed,
                    pprofile,
                    restart,
                    &mut sentinel_seed,
                ) {
                    ScenarioResult::Pass => {}
                    ScenarioResult::Fail(d) => {
                        return Ok(failed(checks, MismatchKind::Protocol, d))
                    }
                    ScenarioResult::Fault(e) => return Err(e),
                }
            }
            Ok(StageOutcome {
                seed,
                sentinel_seed,
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
                sentinel_seed: None,
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
mod tests;
