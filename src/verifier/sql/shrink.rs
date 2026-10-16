//! Delta-debug a failing statement sequence down to a minimal repro.
//!
//! The repair round shows the model a bug report, not a 150-statement log:
//! "run these N statements on a fresh database; statement N answered X, the
//! reference answers Y". Shrinking works on whole statements (ddmin), and
//! every candidate sequence is re-graded from scratch: a fresh oracle gives
//! the expected answers for exactly that subsequence, and a fresh candidate
//! (via `make`) gives the actual ones, so removing a statement can never
//! leave a stale expectation behind.

use super::compare::{render_expected, render_response, Mismatch, MismatchKind};
use super::generator::Statement;
use super::oracle::{Oracle, OracleOutcome};
use super::run::{grade, Candidate};
use super::wire::Response;

/// Upper bound on candidate re-runs (each one replays a sequence).
const MAX_CHECKS: usize = 3000;

#[derive(Debug, Clone)]
pub struct Found {
    /// Index into the statement slice that was run.
    pub index: usize,
    pub mismatch: Mismatch,
    pub expected: OracleOutcome,
    pub got: Result<Response, String>,
}

/// Replay `stmts` on a fresh oracle and a fresh candidate; return the first
/// mismatch. Statements the oracle cannot grade (a discard or an unmapped
/// message) are skipped, never sent: they fail atomically on the oracle, so
/// both sides stay in step.
pub fn first_failure(
    stmts: &[Statement],
    make: &mut dyn FnMut() -> Box<dyn Candidate>,
) -> Option<Found> {
    let oracle = Oracle::new().ok()?;
    let mut cand = make();
    for (index, st) in stmts.iter().enumerate() {
        let expected = oracle.exec(&st.sql);
        if matches!(
            expected,
            OracleOutcome::Discard(_) | OracleOutcome::Unknown(_)
        ) {
            continue;
        }
        let got = cand.exec(&st.sql);
        if let Err(mismatch) = grade(&expected, &got, st.ordered) {
            return Some(Found {
                index,
                mismatch,
                expected,
                got,
            });
        }
    }
    None
}

/// A minimal failing sequence, ready to render.
#[derive(Debug, Clone)]
pub struct Repro {
    /// Run in order on a fresh database; the LAST statement is the one that
    /// answered wrongly.
    pub statements: Vec<Statement>,
    pub kind: MismatchKind,
    pub detail: String,
    pub expected: String,
    pub got: String,
    /// How many candidate sequences the minimizer replayed.
    pub checks: usize,
}

impl Repro {
    /// The model-facing bug report.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "Minimal reproduction. Start from an empty database and send these \
             statements in order, one per line:\n\n",
        );
        for (i, s) in self.statements.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", i + 1, s.sql.replace('\t', "<TAB>")));
        }
        out.push_str(&format!(
            "\nStatement {} answered wrongly ({:?}: {}).\n\nExpected:\n{}\n\nGot:\n{}\n",
            self.statements.len(),
            self.kind,
            self.detail,
            indent(&self.expected),
            indent(&self.got),
        ));
        out
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Minimize a failing statement sequence. `None` when `stmts` does not fail
/// (nothing to shrink).
pub fn shrink(stmts: &[Statement], make: &mut dyn FnMut() -> Box<dyn Candidate>) -> Option<Repro> {
    let first = first_failure(stmts, make)?;
    let kind = first.mismatch.kind;
    let mut checks = 1usize;
    // The failing statement stays last; everything before it is reducible.
    let mut last = stmts[first.index].clone();
    let mut cur: Vec<Statement> = stmts[..first.index].to_vec();

    let mut n = 2usize;
    while !cur.is_empty() && checks < MAX_CHECKS {
        let size = cur.len().div_ceil(n);
        let mut reduced = false;
        let mut start = 0usize;
        while start < cur.len() && checks < MAX_CHECKS {
            let end = (start + size).min(cur.len());
            let mut trial: Vec<Statement> = cur[..start].to_vec();
            trial.extend_from_slice(&cur[end..]);
            trial.push(last.clone());
            checks += 1;
            if let Some(f) = first_failure(&trial, make) {
                if f.mismatch.kind == kind {
                    // Keep the same failure kind; the failing statement may
                    // now come earlier, which shrinks the tail too.
                    last = trial[f.index].clone();
                    trial.truncate(f.index);
                    cur = trial;
                    reduced = true;
                    break;
                }
            }
            start = end;
        }
        if reduced {
            n = n.saturating_sub(1).max(2);
        } else if n >= cur.len() {
            break;
        } else {
            n = (n * 2).min(cur.len());
        }
    }

    let mut statements = cur;
    statements.push(last);
    let fin = first_failure(&statements, make)?;
    checks += 1;
    // The final run may stop earlier than the last statement only if a
    // prefix already fails; keep exactly up to the failing one.
    statements.truncate(fin.index + 1);
    Some(Repro {
        statements,
        kind: fin.mismatch.kind,
        detail: fin.mismatch.detail.clone(),
        expected: render_expected(&fin.expected),
        got: render_response(&fin.got),
        checks,
    })
}
