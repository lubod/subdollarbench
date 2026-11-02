//! Sentinel datasets: proof that data written in an earlier ticket still
//! answers correctly after later tickets land (docs/PROPOSAL-sdb-sql.md,
//! "Upgrade check").
//!
//! A sentinel is generated once: its writes (`CREATE TABLE`, `INSERT`) run
//! on the candidate when that ticket passes, and its *ordered* reads are
//! recorded with the oracle's answers from exactly that state. Before a
//! later ticket's suite the reads run again — reads only, never the writes
//! — and any drift is an `upgrade` regression of the stage that wrote the
//! data.
//!
//! When the graded grammar has `ORDER BY` (ticket 2 passed), only reads
//! that order by every output column are kept, so the recorded wire frame
//! compares byte for byte. Without it, plain reads are kept and compare as
//! a multiset of rows: a ladder that failed ticket 2 is never asked to
//! sort (rules `.17`: a failed ticket is not required).

use super::client::TcpCandidate;
use super::compare::{render_expected, render_response};
use super::generator::{build_script, Kind, Profile};
use super::oracle::{Oracle, OracleOutcome};
use super::run::Candidate;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SentQuery {
    pub sql: String,
    /// The exact wire frame a correct server answers.
    pub expected: String,
    /// The read orders by every output column, so `expected` compares byte
    /// for byte; otherwise its rows compare in any order. Sentinels written
    /// before this field existed were all ordered.
    #[serde(default = "ordered_by_default")]
    pub ordered: bool,
}

fn ordered_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sentinel {
    pub stage: u32,
    pub seed: u64,
    /// The dataset, in order. Run once, when the owning ticket passed.
    pub writes: Vec<String>,
    /// Expected `OK`/`ROWS` frames for the writes (sanity when writing).
    pub write_expected: Vec<String>,
    /// Reads over the written data, with answers recorded at write time.
    pub queries: Vec<SentQuery>,
}

/// Step statements per sentinel script (setup on top).
pub const SENTINEL_STEPS: usize = 40;

/// Build the sentinel for a passing `stage` from a seeded script over
/// `profile` (the grammar the ladder actually passed, never the declared
/// one): creates and inserts are the dataset, selects are the reads.
/// Statements that are not kept are not executed either, so the oracle
/// state matches exactly what the candidate will hold.
/// R36 F3: an empty dataset is a seed fault, not a candidate fault. Try
/// the next seeds in sequence; a seed that builds today keeps its exact
/// dataset because it succeeds on the first attempt. The returned
/// `Sentinel.seed` is the seed actually used.
pub const SENTINEL_SEED_ATTEMPTS: u64 = 16;
const EMPTY_DATASET: &str = "sentinel has no dataset or no ordered reads";

pub fn build(seed: u64, stage: u32, profile: &Profile) -> Result<Sentinel, String> {
    let mut last = String::new();
    for offset in 0..SENTINEL_SEED_ATTEMPTS {
        let candidate = seed.wrapping_add(offset);
        match build_one(candidate, stage, profile) {
            Ok(sent) => return Ok(sent),
            Err(e) if e == EMPTY_DATASET => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(format!(
        "{last} (after {SENTINEL_SEED_ATTEMPTS} seeds from {seed})"
    ))
}

fn build_one(seed: u64, stage: u32, profile: &Profile) -> Result<Sentinel, String> {
    let script = build_script(seed, profile, SENTINEL_STEPS);
    if !script.unknown.is_empty() {
        return Err(format!(
            "sentinel script: unmapped oracle messages {:?}",
            &script.unknown[..script.unknown.len().min(2)]
        ));
    }
    let oracle = Oracle::new()?;
    let mut sent = Sentinel {
        stage,
        seed,
        writes: Vec::new(),
        write_expected: Vec::new(),
        queries: Vec::new(),
    };
    // With `ORDER BY` in the grammar, keep only totally ordered reads (the
    // set this always kept); without it no read is ordered, so keep them all
    // and compare their rows as a multiset.
    let unordered_reads = !profile.order_limit;
    let mut pending_reads: Vec<(String, bool)> = Vec::new();
    for c in script.setup.iter().chain(&script.steps) {
        match c.stmt.kind {
            Kind::Create | Kind::Insert => {
                let out = oracle.exec(&c.stmt.sql);
                if out.is_discard() || matches!(out, OracleOutcome::Unknown(_)) {
                    continue;
                }
                sent.writes.push(c.stmt.sql.clone());
                sent.write_expected.push(render_expected(&out));
            }
            Kind::Select if c.stmt.ordered || unordered_reads => {
                pending_reads.push((c.stmt.sql.clone(), c.stmt.ordered))
            }
            _ => {}
        }
    }
    // Reads are recorded against the *final* dataset state: the sentinel
    // writer runs the whole dataset first and the reads after it, so the
    // script's original interleaving is not replayed.
    for (sql, ordered) in pending_reads {
        let out = oracle.exec(&sql);
        if out.is_discard() || matches!(out, OracleOutcome::Unknown(_)) {
            continue;
        }
        sent.queries.push(SentQuery {
            sql,
            expected: render_expected(&out),
            ordered,
        });
    }
    if sent.writes.is_empty() || sent.queries.is_empty() {
        return Err(EMPTY_DATASET.to_string());
    }
    Ok(sent)
}

fn frame(got: &Result<super::wire::Response, String>) -> Result<String, String> {
    match got {
        Ok(_) => Ok(render_response(got)),
        Err(e) => Err(e.clone()),
    }
}

/// Write the dataset onto the candidate and check the reads immediately
/// (the data must be readable right after it was written, or the sentinel
/// is not usable later). Any write failure is loud: the ticket just passed
/// its suite, so this is a verifier or candidate fault worth surfacing.
pub fn write(addr: &str, timeout: Duration, sent: &Sentinel) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    for (sql, want) in sent.writes.iter().zip(&sent.write_expected) {
        let got = c.exec(sql).map_err(|e| format!("{sql:?}: {e}"))?;
        let frame = frame(&Ok(got))?;
        if !same_answer(&frame, want, true) {
            return Err(format!(
                "write {sql:?} answered {frame:?}, expected {want:?}"
            ));
        }
    }
    check_with(&mut c, sent)
}

/// Re-check a sentinel against a running candidate: reads only.
///
/// Errors are prefixed so the caller can tell them apart: `unreachable:`
/// means the candidate never answered (the suite will fail it anyway),
/// `drift:` means it answered and the recorded data is gone — the upgrade
/// regression.
pub fn check(addr: &str, timeout: Duration, sent: &Sentinel) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout).map_err(|e| format!("unreachable: {e}"))?;
    check_with(&mut c, sent)
}

fn check_with(c: &mut TcpCandidate, sent: &Sentinel) -> Result<(), String> {
    for (answered, q) in sent.queries.iter().enumerate() {
        let got = c.exec(&q.sql).map_err(|e| {
            // An answer, then a broken connection, is a regression — not
            // "the suite will fail it anyway". Only a store that never
            // answered is "unreachable".
            if answered == 0 {
                format!("unreachable: {:?}: {e}", q.sql)
            } else {
                format!(
                    "drift: transport error after {answered} answered read(s): {:?}: {e}",
                    q.sql
                )
            }
        })?;
        let frame = frame(&Ok(got))?;
        if !same_answer(&frame, &q.expected, q.ordered) {
            return Err(format!(
                "drift: read {:?} answered {frame:?}, expected {:?}{}",
                q.sql,
                q.expected,
                if q.ordered {
                    ""
                } else {
                    " (rows in any order)"
                }
            ));
        }
    }
    Ok(())
}

/// Whether `got` answers like `want`: byte for byte for an ordered read;
/// for an unordered one, the same `ROWS` header, column names and `END`
/// line with the same rows in any order. Anything but a `ROWS` frame (an
/// `OK`, an `ERR`, no usable response) must match exactly.
fn same_answer(got: &str, want: &str, ordered: bool) -> bool {
    if got == want {
        return true;
    }
    // Error answers are graded by class: the text is free-form and the
    // reference's wording differs. The stored expectations carry the class
    // only, while feedback renders the candidate's raw text (round-14 N5).
    if let (Some(g), Some(w)) = (err_class(got), err_class(want)) {
        return g == w;
    }
    !ordered && matches!((row_multiset(got), row_multiset(want)), (Some(a), Some(b)) if a == b)
}

/// The class token of an `ERR <class> <text>` frame.
fn err_class(frame: &str) -> Option<&str> {
    frame.strip_prefix("ERR ")?.split_whitespace().next()
}

/// A `ROWS` frame as (its fixed lines, its rows sorted). Rows are one line
/// each on the wire, so sorting lines sorts rows.
fn row_multiset(frame: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    if !frame.starts_with("ROWS ") {
        return None;
    }
    let lines: Vec<&str> = frame.split('\n').collect();
    if lines.len() < 3 {
        return None;
    }
    let (rows, end) = lines[2..].split_at(lines.len() - 3);
    let mut rows = rows.to_vec();
    rows.sort_unstable();
    Some(([&lines[..2], end].concat(), rows))
}

pub fn save(path: &Path, sent: &Sentinel) -> Result<(), String> {
    let json = serde_json::to_string_pretty(sent).map_err(|e| format!("sentinel json: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Load every `sentinel-<stage>.json` in `dir`, ordered by stage. Files
/// that do not parse are skipped with a message: a corrupt sentinel must
/// not wedge every later ticket.
pub fn load_all(dir: &Path) -> Vec<Sentinel> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !(name.starts_with("sentinel-") && name.ends_with(".json")) {
            continue;
        }
        match std::fs::read_to_string(e.path())
            .ok()
            .and_then(|t| serde_json::from_str::<Sentinel>(&t).ok())
        {
            Some(s) => out.push(s),
            None => eprintln!("sdb-sql: unreadable sentinel {}", e.path().display()),
        }
    }
    out.sort_by_key(|s| s.stage);
    out
}

#[cfg(test)]
mod tests;
