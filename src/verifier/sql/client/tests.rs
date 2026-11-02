use super::*;
use std::io::BufRead;
use std::net::TcpListener;
use std::thread;

/// A one-connection server running `serve` on the accepted socket;
/// returns its address.
fn server<F>(serve: F) -> std::net::SocketAddr
where
    F: FnOnce(TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        // Without nodelay the small response writes pay a delayed-ACK
        // stall (~40ms) per statement in these tests.
        sock.set_nodelay(true).unwrap();
        serve(sock);
    });
    addr
}

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn round_trips_one_response_per_statement() {
    let addr = server(|mut sock| {
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 {
            let reply: &[u8] = match line.trim_end() {
                "SELECT ok" => b"ROWS 1\nn\n5\nEND 1\n",
                "broken" => b"ERR no_column nope\n",
                _ => b"OK 2\n",
            };
            sock.write_all(reply).unwrap();
            line.clear();
        }
    });
    let mut c = TcpCandidate::connect(addr, TIMEOUT).unwrap();
    assert_eq!(c.exec("INSERT a").unwrap(), Response::Ok(2));
    match c.exec("SELECT ok").unwrap() {
        Response::Rows {
            ncols,
            headers,
            rows,
            end_count,
        } => {
            assert_eq!((ncols, end_count), (1, 1));
            assert_eq!(headers, vec!["n".to_string()]);
            assert_eq!(rows, vec![vec!["5".to_string()]]);
        }
        other => panic!("{other:?}"),
    }
    match c.exec("broken").unwrap() {
        Response::Err { class, .. } => assert_eq!(class, "no_column"),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.exec("UPDATE b").unwrap(), Response::Ok(2));
}

/// A response written one byte at a time (or otherwise split across
/// segments) must be reassembled.
#[test]
fn split_responses_are_reassembled() {
    let addr = server(|mut sock| {
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        for b in b"ROWS 2\na\tb\n1\t\\N\nEND 1\n" {
            sock.write_all(&[*b]).unwrap();
        }
    });
    let mut c = TcpCandidate::connect(addr, TIMEOUT).unwrap();
    match c.exec("SELECT t").unwrap() {
        Response::Rows {
            ncols,
            headers,
            rows,
            end_count,
        } => {
            assert_eq!((ncols, end_count), (2, 1));
            assert_eq!(headers, vec!["a".to_string(), "b".to_string()]);
            assert_eq!(rows, vec![vec!["1".to_string(), "\\N".to_string()]]);
        }
        other => panic!("{other:?}"),
    }
}

/// Bytes after the response stay buffered: the next `exec` returns the
/// next response without waiting for the server.
#[test]
fn leftover_bytes_stay_buffered() {
    let addr = server(|mut sock| {
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        // Two answers in one burst, then only drain the requests.
        sock.write_all(b"OK 1\nOK 2\n").unwrap();
        while reader.read_line(&mut line).unwrap() > 0 {
            line.clear();
        }
    });
    let mut c = TcpCandidate::connect(addr, TIMEOUT).unwrap();
    assert_eq!(c.exec("s1").unwrap(), Response::Ok(1));
    assert_eq!(c.exec("s2").unwrap(), Response::Ok(2));
}

/// A server that accepts, reads the statement, then closes without
/// answering is a transport error, not a hang.
#[test]
fn closed_connection_is_a_transport_error() {
    let addr = server(|sock| {
        let mut line = String::new();
        BufReader::new(sock.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        // Drop without answering.
    });
    let mut c = TcpCandidate::connect(addr, TIMEOUT).unwrap();
    let err = c.exec("SELECT 1").unwrap_err();
    assert!(err.contains("connection closed"), "{err}");
}

#[test]
fn a_silent_server_times_out() {
    let addr = server(|sock| {
        let mut line = String::new();
        BufReader::new(sock.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        // Keep the socket open without answering; the client must time
        // out rather than block forever.
        thread::sleep(Duration::from_secs(10));
        let _ = sock.shutdown(std::net::Shutdown::Both);
    });
    let mut c = TcpCandidate::connect(addr, Duration::from_millis(300)).unwrap();
    let err = c.exec("SELECT 1").unwrap_err();
    // F1: the timeout names the wait and the statement, not an errno.
    assert!(err.contains("no answer within"), "{err}");
    assert!(err.contains("statement \"SELECT 1\""), "{err}");
}

/// End to end over a real socket: a server that answers from its own
/// oracle (wire-encoding the outcomes) passes a generated script
/// through the TCP candidate. This is the seam M6's reference adapter
/// plugs into.
#[test]
fn tcp_candidate_passes_a_generated_script() {
    use super::super::compare::render_expected;
    use super::super::generator::{build_script, Profile};
    use super::super::oracle::{Oracle, OracleOutcome};
    use super::super::run::{run_script, Verdict};

    let script = build_script(7, &Profile::through(3), 120);
    let addr = server(move |mut sock| {
        let oracle = Oracle::new().unwrap();
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 {
            let out = oracle.exec(line.trim_end());
            assert!(
                !matches!(out, OracleOutcome::Discard(_) | OracleOutcome::Unknown(_)),
                "ungradeable answer: {out:?}"
            );
            sock.write_all(format!("{}\n", render_expected(&out)).as_bytes())
                .unwrap();
            line.clear();
        }
    });
    let mut c = TcpCandidate::connect(addr, Duration::from_secs(10)).unwrap();
    match run_script(&mut c, &script) {
        Verdict::Pass { checks } => assert!(checks > 100, "{checks} checks"),
        Verdict::Fail(f) => panic!("{:?} answered wrongly: {:?}", f.stmt.sql, f.mismatch),
    }
}
