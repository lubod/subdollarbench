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

#[test]
fn candidate_error_text_is_capped_char_safely() {
    let long = "é".repeat(1000);
    let rendered = render_response(&Ok(Response::Err {
        class: "syntax".into(),
        text: long.clone(),
    }));
    // "ERR syntax " + 300 characters + the ellipsis.
    assert!(
        rendered.chars().count() <= 312,
        "{}",
        rendered.chars().count()
    );
    assert!(rendered.ends_with('…'));
    let short = render_response(&Ok(Response::Err {
        class: "syntax".into(),
        text: " near \"x\"".into(),
    }));
    assert_eq!(short, "ERR syntax near \"x\"");
}
