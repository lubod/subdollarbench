use super::*;
use std::io::Cursor;

fn parse(s: &str) -> Result<Response, String> {
    read_response(&mut Cursor::new(s.as_bytes()))
}

#[test]
fn text_escaping_round_trips() {
    for t in [
        "",
        "plain",
        "a\tb",
        "back\\slash",
        "x\\ty",
        "line\nbreak",
        "\\N",
    ] {
        let enc = encode_text(t);
        assert!(!enc.contains('\t') && !enc.contains('\n'), "{enc:?}");
        assert_eq!(decode_text(&enc).unwrap(), t, "{t:?}");
    }
    assert!(decode_text("\\q").is_err());
    assert!(decode_text("trail\\").is_err());
}

#[test]
fn values_encode_to_their_wire_form() {
    assert_eq!(encode_value(&Value::Null), "\\N");
    assert_eq!(encode_value(&Value::Int(-7)), "-7");
    assert_eq!(encode_value(&Value::Text(String::new())), "");
    // The TEXT string `\N` is distinguishable from NULL.
    assert_eq!(encode_value(&Value::Text("\\N".into())), "\\\\N");
}

#[test]
fn parses_ok_err_and_rows() {
    assert_eq!(parse("OK 3\n").unwrap(), Response::Ok(3));
    assert_eq!(
        parse("ERR no_table no such table: x\n").unwrap(),
        Response::Err {
            class: "no_table".into(),
            text: "no such table: x".into()
        }
    );
    assert_eq!(
        parse("ERR syntax\n").unwrap(),
        Response::Err {
            class: "syntax".into(),
            text: String::new()
        }
    );
    let r = parse("ROWS 2\na\tb\n1\t\\N\n\t x\nEND 2\n").unwrap();
    assert_eq!(
        r,
        Response::Rows {
            ncols: 2,
            headers: vec!["a".into(), "b".into()],
            rows: vec![vec!["1".into(), "\\N".into()], vec!["".into(), " x".into()]],
            end_count: 2
        }
    );
}

#[test]
fn empty_result_and_empty_string_cells() {
    match parse("ROWS 1\nc\nEND 0\n").unwrap() {
        Response::Rows {
            rows, end_count, ..
        } => {
            assert!(rows.is_empty());
            assert_eq!(end_count, 0);
        }
        other => panic!("{other:?}"),
    }
    // A one-column row holding the empty string is an empty line.
    match parse("ROWS 1\nc\n\nEND 1\n").unwrap() {
        Response::Rows { rows, .. } => assert_eq!(rows, vec![vec![String::new()]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn timeout_names_the_step_and_wait() {
    struct Never;
    impl std::io::Read for Never {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Resource temporarily unavailable (os error 11)",
            ))
        }
    }
    let mut r = std::io::BufReader::new(Never);
    let e = read_response_ctx(
        &mut r,
        Some((Duration::from_secs(5), "the hostile-input step")),
    )
    .unwrap_err();
    assert_eq!(
        e,
        "no answer within 5s to the hostile-input step: the server did not reply"
    );
}

#[test]
fn pipelined_responses_read_in_order() {
    let mut c = Cursor::new(b"OK 1\nROWS 1\nn\n5\nEND 1\nERR busy\n".to_vec());
    assert_eq!(read_response(&mut c).unwrap(), Response::Ok(1));
    assert!(matches!(
        read_response(&mut c).unwrap(),
        Response::Rows { .. }
    ));
    assert!(matches!(
        read_response(&mut c).unwrap(),
        Response::Err { .. }
    ));
    assert!(read_response(&mut c).is_err(), "EOF is an error");
}

#[test]
fn protocol_violations_are_errors() {
    assert!(parse("").is_err());
    assert!(parse("HELLO\n").is_err());
    assert!(parse("OK x\n").is_err());
    assert!(parse("ROWS x\n").is_err());
    assert!(parse("ROWS 1\nc\n1\n").is_err(), "missing END");
    // Only an exact `END <digits>` terminates; lookalikes are data.
    match parse("ROWS 1\nc\nEND\nEND 1x\nEND 2\n").unwrap() {
        Response::Rows { rows, .. } => assert_eq!(rows.len(), 2),
        other => panic!("{other:?}"),
    }
}
