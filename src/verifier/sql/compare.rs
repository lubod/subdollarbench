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
mod tests {
    use super::*;
    use crate::verifier::sql::oracle::ErrClass;

    fn rows(headers: &[&str], data: &[&[Value]]) -> OracleOutcome {
        OracleOutcome::Rows {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            rows: data.iter().map(|r| r.to_vec()).collect(),
        }
    }

    fn resp(headers: &[&str], data: &[&[&str]]) -> Response {
        Response::Rows {
            ncols: headers.len(),
            headers: headers.iter().map(|s| s.to_string()).collect(),
            rows: data
                .iter()
                .map(|r| r.iter().map(|c| c.to_string()).collect())
                .collect(),
            end_count: data.len() as u64,
        }
    }

    fn kind_of(r: Result<(), Mismatch>) -> MismatchKind {
        r.unwrap_err().kind
    }

    #[test]
    fn affected_counts_compare_exactly() {
        let e = OracleOutcome::Affected(3);
        assert!(compare(&e, &Response::Ok(3), false).is_ok());
        assert_eq!(
            kind_of(compare(&e, &Response::Ok(2), false)),
            MismatchKind::AffectedCount
        );
    }

    #[test]
    fn error_class_compares_token_not_text() {
        let e = OracleOutcome::Error(ErrClass::NoTable);
        let ok = Response::Err {
            class: "no_table".into(),
            text: "whatever the candidate says".into(),
        };
        assert!(compare(&e, &ok, false).is_ok());
        let wrong = Response::Err {
            class: "syntax".into(),
            text: String::new(),
        };
        assert_eq!(
            kind_of(compare(&e, &wrong, false)),
            MismatchKind::ErrorClass
        );
    }

    #[test]
    fn response_kind_mismatches() {
        let e = OracleOutcome::Affected(0);
        let r = resp(&["a"], &[]);
        assert_eq!(kind_of(compare(&e, &r, false)), MismatchKind::Kind);
        let e = OracleOutcome::Error(ErrClass::Syntax);
        assert_eq!(
            kind_of(compare(&e, &Response::Ok(0), false)),
            MismatchKind::Kind
        );
    }

    #[test]
    fn rows_multiset_ignores_order_but_ordered_does_not() {
        let e = rows(&["a"], &[&[Value::Int(1)], &[Value::Int(2)]]);
        let swapped = resp(&["a"], &[&["2"], &["1"]]);
        assert!(compare(&e, &swapped, false).is_ok());
        assert_eq!(kind_of(compare(&e, &swapped, true)), MismatchKind::Order);
        let wrong = resp(&["a"], &[&["1"], &["3"]]);
        assert_eq!(kind_of(compare(&e, &wrong, true)), MismatchKind::Cell);
        assert_eq!(kind_of(compare(&e, &wrong, false)), MismatchKind::Cell);
    }

    #[test]
    fn duplicates_are_counted_in_the_multiset() {
        let e = rows(&["a"], &[&[Value::Int(1)], &[Value::Int(1)]]);
        let r = resp(&["a"], &[&["1"], &["2"]]);
        assert_eq!(kind_of(compare(&e, &r, false)), MismatchKind::Cell);
    }

    #[test]
    fn headers_and_counts() {
        let e = rows(&["a", "b"], &[&[Value::Int(1), Value::Null]]);
        assert!(compare(&e, &resp(&["a", "b"], &[&["1", "\\N"]]), true).is_ok());
        assert_eq!(
            kind_of(compare(&e, &resp(&["a", "c"], &[&["1", "\\N"]]), true)),
            MismatchKind::Headers
        );
        assert_eq!(
            kind_of(compare(&e, &resp(&["a", "b"], &[]), true)),
            MismatchKind::RowCount
        );
    }

    #[test]
    fn wire_forms_are_exact() {
        // NULL vs the text "\N", the empty string, signs and leading zeros.
        let e = rows(
            &["a", "b", "c"],
            &[&[Value::Null, Value::Text(String::new()), Value::Int(5)]],
        );
        assert!(compare(&e, &resp(&["a", "b", "c"], &[&["\\N", "", "5"]]), true).is_ok());
        for bad in [
            ["", "", "5"],
            ["\\N", "\\N", "5"],
            ["\\N", "", "+5"],
            ["\\N", "", "05"],
        ] {
            assert_eq!(
                kind_of(compare(&e, &resp(&["a", "b", "c"], &[&bad]), true)),
                MismatchKind::Cell,
                "{bad:?}"
            );
        }
        let esc = rows(&["t"], &[&[Value::Text("a\tb\\".into())]]);
        assert!(compare(&esc, &resp(&["t"], &[&["a\\tb\\\\"]]), true).is_ok());
    }

    #[test]
    fn framing_contradictions_are_protocol_errors() {
        let e = rows(&["a"], &[&[Value::Int(1)]]);
        let mut r = resp(&["a"], &[&["1"]]);
        if let Response::Rows { end_count, .. } = &mut r {
            *end_count = 5;
        }
        assert_eq!(kind_of(compare(&e, &r, true)), MismatchKind::Protocol);
        let mut r = resp(&["a"], &[&["1"]]);
        if let Response::Rows { ncols, .. } = &mut r {
            *ncols = 2;
        }
        assert_eq!(kind_of(compare(&e, &r, true)), MismatchKind::Protocol);
        let ragged = Response::Rows {
            ncols: 1,
            headers: vec!["a".into()],
            rows: vec![vec!["1".into(), "2".into()]],
            end_count: 1,
        };
        assert_eq!(kind_of(compare(&e, &ragged, true)), MismatchKind::Protocol);
    }

    #[test]
    fn ungradeable_expectations_are_internal_errors() {
        let d = OracleOutcome::Discard("x".into());
        assert_eq!(
            kind_of(compare(&d, &Response::Ok(0), false)),
            MismatchKind::Internal
        );
        let u = OracleOutcome::Unknown("y".into());
        assert_eq!(
            kind_of(compare(&u, &Response::Ok(0), false)),
            MismatchKind::Internal
        );
    }
}
