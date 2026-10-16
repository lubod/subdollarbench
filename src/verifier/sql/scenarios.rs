//! Scripted protocol scenarios: behavior the differential generator cannot
//! express (pipelining now; crash, isolation, perf, memory, ENOSPC,
//! hostile input, replication and the PG wire later).
//!
//! Scenarios speak to the candidate directly over a socket, not through
//! [`Candidate`](super::run::Candidate): the trait is strictly
//! request/response, while pipelining needs several statements in flight
//! before the first response is read.

use super::wire::{read_response, Response};
use std::io::{BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Send every line first, then read one response per line (the harness-side
/// pipelining client). Responses must come back in the order sent
/// (docs/PROPOSAL-sdb-sql.md, "Text wire protocol").
///
/// A server that cannot handle pipelined input will stall waiting for a
/// read that only happens after its first response; the read timeout turns
/// that into a transport error rather than a hang.
pub fn pipeline(
    addr: impl ToSocketAddrs,
    lines: &[String],
    timeout: Duration,
) -> Result<Vec<Response>, String> {
    let stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("set write timeout: {e}"))?;
    let mut reader = BufReader::new(stream);
    {
        let stream = reader.get_mut();
        for line in lines {
            debug_assert!(
                !line.contains('\n') && !line.contains('\r'),
                "one statement per line: {line:?}"
            );
            stream
                .write_all(line.as_bytes())
                .and_then(|()| stream.write_all(b"\n"))
                .map_err(|e| format!("write: {e}"))?;
        }
    }
    let mut out = Vec::with_capacity(lines.len());
    for _ in lines {
        out.push(read_response(&mut reader)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::compare::compare;
    use super::super::generator::{build_script, Profile};
    use super::super::oracle::Oracle;
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::thread;

    /// A server that reads `n` statements *before* answering any of them —
    /// it only makes progress if the client really pipelines — then answers
    /// in the received order.
    fn burst_server<F>(n: usize, answer: F) -> std::net::SocketAddr
    where
        F: FnOnce(Vec<String>) -> Vec<String> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_nodelay(true).unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut lines = Vec::new();
            let mut line = String::new();
            while lines.len() < n {
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                lines.push(line.trim_end().to_string());
                line.clear();
            }
            for reply in answer(lines) {
                sock.write_all(reply.as_bytes()).unwrap();
                sock.write_all(b"\n").unwrap();
            }
        });
        addr
    }

    #[test]
    fn pipelined_statements_are_answered_in_order() {
        let addr = burst_server(3, |lines| {
            assert_eq!(lines, vec!["one", "two", "three"]);
            vec![
                "OK 1".to_string(),
                "ROWS 1\nv\n7\nEND 1".to_string(),
                "ERR no_table gone".to_string(),
            ]
        });
        let lines: Vec<String> = ["one", "two", "three"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let got = pipeline(addr, &lines, Duration::from_secs(5)).unwrap();
        assert_eq!(got[0], Response::Ok(1));
        match &got[1] {
            Response::Rows { rows, .. } => assert_eq!(*rows, vec![vec!["7".to_string()]]),
            other => panic!("{other:?}"),
        }
        match &got[2] {
            Response::Err { class, .. } => assert_eq!(class, "no_table"),
            other => panic!("{other:?}"),
        }
    }

    /// Pipelining a real generated script: the reference answers every line
    /// in order, and every answer grades clean against the oracle.
    #[test]
    fn pipelined_script_answers_match_the_oracle() {
        let script = build_script(3, &Profile::through(3), 40);
        let checked: Vec<crate::verifier::sql::generator::Checked> = script
            .setup
            .iter()
            .chain(&script.steps)
            .cloned()
            .collect();
        let lines: Vec<String> = checked.iter().map(|c| c.stmt.sql.clone()).collect();
        let ordered: Vec<bool> = checked.iter().map(|c| c.stmt.ordered).collect();
        let sent = lines.clone();
        let addr = burst_server(lines.len(), move |got| {
            assert_eq!(got, sent, "the server received the wrong lines");
            let oracle = Oracle::new().unwrap();
            checked
                .iter()
                .map(|c| {
                    let out = oracle.exec(&c.stmt.sql);
                    assert!(!out.is_discard(), "ungradeable: {out:?}");
                    super::super::compare::render_expected(&out)
                })
                .collect()
        });
        let got = pipeline(addr, &lines, Duration::from_secs(10)).unwrap();
        assert_eq!(got.len(), lines.len());
        let oracle = Oracle::new().unwrap();
        for ((reply, line), ordered) in got.iter().zip(&lines).zip(&ordered) {
            let expected = oracle.exec(line);
            compare(&expected, reply, *ordered).expect("pipelined answer must grade clean");
        }
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(20));
            drop(sock);
        });
        let lines = vec!["SELECT 1".to_string()];
        let err = pipeline(addr, &lines, Duration::from_millis(300)).unwrap_err();
        assert!(err.starts_with("read:"), "{err}");
    }
}
