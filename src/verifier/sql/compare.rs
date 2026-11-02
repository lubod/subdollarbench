//! Compare the oracle's expected outcome with the candidate's response.
//!
//! Rules (docs/PROPOSAL-sdb-sql.md):
//! - `OK <n>`: the affected count must match exactly.
//! - `ROWS`: headers match exactly; each result must declare the column
//!   count and row count it actually sent; rows compare as a sequence when
//!   the statement orders by every output column, else as a multiset.
//! - `ERR`: the class token must match; the free text is never compared.
//!
//! Cells compare in wire form (see `wire`): the expected value is encoded
//! and compared with the candidate's raw cell.

use super::oracle::{OracleOutcome, Value};
use super::wire::{encode_value, Response};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MismatchKind {
    /// The harness was asked to compare something that is not gradeable
    /// (a discard or an unknown oracle message): a verifier bug.
    Internal,
    /// The framing contradicts itself (declared counts, ragged rows).
    Protocol,
    /// OK vs ROWS vs ERR.
    Kind,
    AffectedCount,
    ErrorClass,
    Headers,
    RowCount,
    Cell,
    /// Same rows, wrong order.
    Order,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub kind: MismatchKind,
    pub detail: String,
}

fn mm(kind: MismatchKind, detail: impl Into<String>) -> Result<(), Mismatch> {
    Err(Mismatch {
        kind,
        detail: detail.into(),
    })
}

fn describe(r: &Response) -> String {
    match r {
        Response::Ok(n) => format!("OK {n}"),
        Response::Rows { rows, .. } => format!("ROWS ({} rows)", rows.len()),
        Response::Err { class, .. } => format!("ERR {class}"),
    }
}

fn describe_expected(o: &OracleOutcome) -> String {
    match o {
        OracleOutcome::Affected(n) => format!("OK {n}"),
        OracleOutcome::Rows { rows, .. } => format!("ROWS ({} rows)", rows.len()),
        OracleOutcome::Error(c) => format!("ERR {}", c.token()),
        OracleOutcome::Discard(r) => format!("discard ({r})"),
        OracleOutcome::Unknown(m) => format!("unknown ({m})"),
    }
}

/// The exact bytes a correct server sends for `expected`, for repair
/// feedback ("this is what you should have answered").
pub fn render_expected(expected: &OracleOutcome) -> String {
    match expected {
        OracleOutcome::Affected(n) => format!("OK {n}"),
        OracleOutcome::Error(c) => format!("ERR {}", c.token()),
        OracleOutcome::Rows { headers, rows } => {
            let mut out = format!("ROWS {}\n{}\n", headers.len(), headers.join("\t"));
            for r in rows {
                out.push_str(&wire_row(r).join("\t"));
                out.push('\n');
            }
            out.push_str(&format!("END {}", rows.len()));
            out
        }
        OracleOutcome::Discard(r) => format!("(discarded: {r})"),
        OracleOutcome::Unknown(m) => format!("(unknown: {m})"),
    }
}

/// Truncate a candidate-supplied error to `max` characters (char-safe) and
/// mark the cut, so a traceback cannot dominate the feedback.
fn cap_error_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// What the candidate actually sent, in the same shape as
/// [`render_expected`].
pub fn render_response(got: &Result<Response, String>) -> String {
    match got {
        Err(e) => format!("(no usable response: {e})"),
        Ok(Response::Ok(n)) => format!("OK {n}"),
        // N5 (round 14): the candidate's own words name the bug
        // ("internal error: 'list' object has no attribute 'lower'"); the
        // class alone made every repair round repeat the same crash.
        Ok(Response::Err { class, text }) => {
            // N5 has no length cap (round-15 L5): a one-line traceback can be
            // thousands of characters and would swamp the feedback/repro.
            let text = cap_error_text(text.trim(), 300);
            if text.is_empty() {
                format!("ERR {class}")
            } else {
                format!("ERR {class} {text}")
            }
        }
        Ok(Response::Rows {
            ncols,
            headers,
            rows,
            end_count,
        }) => {
            let mut out = format!("ROWS {ncols}\n{}\n", headers.join("\t"));
            for r in rows {
                out.push_str(&r.join("\t"));
                out.push('\n');
            }
            out.push_str(&format!("END {end_count}"));
            out
        }
    }
}

fn wire_row(row: &[Value]) -> Vec<String> {
    row.iter().map(encode_value).collect()
}

fn show(row: &[String]) -> String {
    format!("[{}]", row.join(" | "))
}

/// Grade one response. `ordered` says the statement ordered by every output
/// column, so the sequence must match; otherwise rows compare as multisets.
pub fn compare(expected: &OracleOutcome, got: &Response, ordered: bool) -> Result<(), Mismatch> {
    match (expected, got) {
        (OracleOutcome::Discard(_) | OracleOutcome::Unknown(_), _) => mm(
            MismatchKind::Internal,
            format!("not gradeable: {}", describe_expected(expected)),
        ),
        (OracleOutcome::Affected(want), Response::Ok(have)) => {
            if want == have {
                Ok(())
            } else {
                mm(
                    MismatchKind::AffectedCount,
                    format!("expected OK {want}, got OK {have}"),
                )
            }
        }
        (OracleOutcome::Error(want), Response::Err { class, .. }) => {
            if want.token() == class {
                Ok(())
            } else {
                mm(
                    MismatchKind::ErrorClass,
                    format!("expected ERR {}, got ERR {class}", want.token()),
                )
            }
        }
        (
            OracleOutcome::Rows {
                headers: want_h,
                rows: want_rows,
            },
            Response::Rows {
                ncols,
                headers,
                rows,
                end_count,
            },
        ) => {
            if *ncols != headers.len() {
                return mm(
                    MismatchKind::Protocol,
                    format!(
                        "ROWS declared {ncols} columns but the header line has {}",
                        headers.len()
                    ),
                );
            }
            if *end_count != rows.len() as u64 {
                return mm(
                    MismatchKind::Protocol,
                    format!("END declared {end_count} rows but {} were sent", rows.len()),
                );
            }
            if let Some(bad) = rows.iter().find(|r| r.len() != headers.len()) {
                return mm(
                    MismatchKind::Protocol,
                    format!(
                        "a row has {} cells but the header has {}: {}",
                        bad.len(),
                        headers.len(),
                        show(bad)
                    ),
                );
            }
            if headers != want_h {
                return mm(
                    MismatchKind::Headers,
                    format!("expected header {want_h:?}, got {headers:?}"),
                );
            }
            let want: Vec<Vec<String>> = want_rows.iter().map(|r| wire_row(r)).collect();
            if want.len() != rows.len() {
                return mm(
                    MismatchKind::RowCount,
                    format!("expected {} rows, got {}", want.len(), rows.len()),
                );
            }
            if ordered {
                if let Some(i) = (0..want.len()).find(|&i| want[i] != rows[i]) {
                    // Same multiset in the wrong order, or different data?
                    let mut a = want.clone();
                    let mut b = rows.clone();
                    a.sort();
                    b.sort();
                    let kind = if a == b {
                        MismatchKind::Order
                    } else {
                        MismatchKind::Cell
                    };
                    return mm(
                        kind,
                        format!(
                            "row {i}: expected {}, got {}",
                            show(&want[i]),
                            show(&rows[i])
                        ),
                    );
                }
                Ok(())
            } else {
                let mut a = want;
                let mut b = rows.clone();
                a.sort();
                b.sort();
                match (0..a.len()).find(|&i| a[i] != b[i]) {
                    None => Ok(()),
                    Some(i) => mm(
                        MismatchKind::Cell,
                        format!(
                            "result multisets differ: expected {} (sorted position {i}), got {}",
                            show(&a[i]),
                            show(&b[i])
                        ),
                    ),
                }
            }
        }
        _ => mm(
            MismatchKind::Kind,
            format!(
                "expected {}, got {}",
                describe_expected(expected),
                describe(got)
            ),
        ),
    }
}

#[cfg(test)]
mod tests;
