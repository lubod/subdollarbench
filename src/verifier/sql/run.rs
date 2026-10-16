//! Replay a [`Script`] against a candidate and report the first mismatch.
//!
//! [`Candidate`] is the seam between the verifier and whatever answers
//! statements: the in-test toy engines today, a TCP client against the
//! model's server tomorrow.

use super::compare::{compare, Mismatch, MismatchKind};
use super::generator::{Script, Statement};
use super::oracle::OracleOutcome;
use super::wire::Response;

/// Something that answers one statement line with one response.
pub trait Candidate {
    /// `Err` is a transport failure (closed connection, timeout, framing
    /// the reader could not parse); it grades as a protocol mismatch.
    fn exec(&mut self, sql: &str) -> Result<Response, String>;
}

#[derive(Debug, Clone)]
pub struct Failure {
    /// Position in `setup` followed by `steps`.
    pub index: usize,
    pub stmt: Statement,
    pub expected: OracleOutcome,
    pub got: Result<Response, String>,
    pub mismatch: Mismatch,
}

#[derive(Debug, Clone)]
pub enum Verdict {
    Pass { checks: usize },
    Fail(Box<Failure>),
}

/// Grade one response (a transport error is a protocol mismatch).
pub fn grade(
    expected: &OracleOutcome,
    got: &Result<Response, String>,
    ordered: bool,
) -> Result<(), Mismatch> {
    match got {
        Ok(r) => compare(expected, r, ordered),
        Err(e) => Err(Mismatch {
            kind: MismatchKind::Protocol,
            detail: e.clone(),
        }),
    }
}

/// Run every statement of the script in order, stopping at the first
/// mismatch (later answers would depend on a state the candidate already
/// got wrong).
pub fn run_script(cand: &mut dyn Candidate, script: &Script) -> Verdict {
    let mut checks = 0usize;
    for (index, c) in script.setup.iter().chain(&script.steps).enumerate() {
        let got = cand.exec(&c.stmt.sql);
        if let Err(mismatch) = grade(&c.expected, &got, c.stmt.ordered) {
            return Verdict::Fail(Box::new(Failure {
                index,
                stmt: c.stmt.clone(),
                expected: c.expected.clone(),
                got,
                mismatch,
            }));
        }
        checks += 1;
    }
    Verdict::Pass { checks }
}
