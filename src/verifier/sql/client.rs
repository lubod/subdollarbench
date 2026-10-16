//! TCP client for the sdb-sql text protocol: a [`Candidate`] backed by one
//! connection to a server.
//!
//! One statement per line, one response per `exec` ([`read_response`]). The
//! reader keeps its buffer across calls, so a response split over several
//! TCP segments is reassembled and any bytes after the response stay
//! buffered for the next call.
//!
//! Transport failures (connect/write/read errors, timeouts, framing the
//! reader cannot parse) come back as `Err(String)` and grade as a protocol
//! mismatch ([`Candidate`]). A failed call leaves the connection in an
//! unknown state: the driver should drop the candidate and connect again.

use super::run::Candidate;
use super::wire::{read_response, Response};
use std::io::{BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A [`Candidate`] speaking the text protocol over one TCP connection.
pub struct TcpCandidate {
    reader: BufReader<TcpStream>,
}

impl TcpCandidate {
    /// Connect and set read/write timeouts. `timeout` should cover the
    /// slowest single-statement answer the stage allows, not the whole
    /// script; the caller owns the retry policy.
    pub fn connect(addr: impl ToSocketAddrs, timeout: Duration) -> Result<Self, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("set read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| format!("set write timeout: {e}"))?;
        // Statements are strictly request/response; Nagle would only add
        // latency.
        stream
            .set_nodelay(true)
            .map_err(|e| format!("set nodelay: {e}"))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }
}

impl TcpCandidate {
    /// Write one statement line **without reading** its response. The crash
    /// probes need the kill to land while the statement is in flight, which
    /// `exec` would defeat (it blocks until the answer).
    pub fn send(&mut self, sql: &str) -> Result<(), String> {
        debug_assert!(
            !sql.contains('\n') && !sql.contains('\r'),
            "one statement per line: {sql:?}"
        );
        let stream = self.reader.get_mut();
        stream
            .write_all(sql.as_bytes())
            .and_then(|()| stream.write_all(b"\n"))
            .map_err(|e| format!("write: {e}"))
    }

    /// Read one response without sending anything (the in-flight probe).
    pub fn recv(&mut self) -> Result<Response, String> {
        read_response(&mut self.reader)
    }

    /// The read timeout for subsequent reads; the in-flight probe wants a
    /// very short one.
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("set read timeout: {e}"))
    }
}

impl Candidate for TcpCandidate {
    fn exec(&mut self, sql: &str) -> Result<Response, String> {
        // The protocol is one statement per line; the generator never
        // emits a newline, so a hit here is a verifier bug.
        debug_assert!(
            !sql.contains('\n') && !sql.contains('\r'),
            "one statement per line: {sql:?}"
        );
        let stream = self.reader.get_mut();
        stream
            .write_all(sql.as_bytes())
            .and_then(|()| stream.write_all(b"\n"))
            .map_err(|e| format!("write: {e}"))?;
        read_response(&mut self.reader)
    }
}

#[cfg(test)]
mod tests {
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
        assert!(err.starts_with("read:"), "{err}");
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
}
