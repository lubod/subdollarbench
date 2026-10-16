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

/// The verdict of a stage's scripted scenarios.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenarioResult {
    Pass,
    /// The candidate misbehaved; the text is the model-facing detail.
    Fail(String),
    /// The verifier could not build or run the scenario (never the
    /// candidate's fault).
    Fault(String),
}

/// Run the scripted scenarios of `stage`'s suite. Stages without scenarios
/// pass trivially. `seed` namespaces every table the scenarios create.
pub fn for_stage(stage: u32, addr: &str, timeout: Duration, seed: u64) -> ScenarioResult {
    match stage {
        0 => stage0(addr, timeout, seed),
        _ => ScenarioResult::Pass,
    }
}

/// Stage 0 (docs/PROPOSAL-sdb-sql.md, "0. Genesis", S): a pipelined script
/// answered in order, 8 interleaved clients that see each other's rows,
/// and hostile input (garbage bytes, an unterminated line, a 2 MiB line)
/// that must not kill the server.
pub fn stage0(addr: &str, timeout: Duration, seed: u64) -> ScenarioResult {
    use super::generator::{build_script, Profile};
    // A seed distinct from the differential script's, so the namespaced
    // schemas never collide on the long-lived server.
    let pseed = seed ^ 0x5CE7_A510_0000_0001;

    // 1. Pipelining: every line sent before the first answer is read.
    let script = build_script(pseed, &Profile::for_passed(&[0]), 40);
    if !script.unknown.is_empty() {
        return ScenarioResult::Fault(format!(
            "pipelining script: unmapped oracle messages {:?}",
            &script.unknown[..script.unknown.len().min(3)]
        ));
    }
    let checked: Vec<_> = script.setup.iter().chain(&script.steps).collect();
    let lines: Vec<String> = checked.iter().map(|c| c.stmt.sql.clone()).collect();
    let got = match pipeline(addr, &lines, timeout) {
        Ok(g) => g,
        Err(e) => {
            return ScenarioResult::Fail(format!(
                "pipelining: {} statements sent before reading, then: {e}",
                lines.len()
            ))
        }
    };
    for (i, (c, reply)) in checked.iter().zip(&got).enumerate() {
        if let Err(m) = super::compare::compare(&c.expected, reply, c.stmt.ordered) {
            return ScenarioResult::Fail(format!(
                "pipelining: answer {} of {} ({:?}) is wrong: {:?}: {}",
                i + 1,
                lines.len(),
                c.stmt.sql,
                m.kind,
                m.detail
            ));
        }
    }

    // 2. Eight interleaved clients see each other's committed rows.
    let table = format!("c{pseed:x}_mc");
    if let Err(e) = concurrent_clients(addr, timeout, &table) {
        return ScenarioResult::Fail(format!("8 concurrent clients: {e}"));
    }

    // 3. Hostile input on throwaway connections; after each one a fresh
    // connection must still answer.
    let mut garbage: Vec<u8> = (0u8..=255).rev().collect();
    garbage.extend_from_slice(b"\x00\x1b[2J\xc3\x28\n");
    let big: Vec<u8> = std::iter::repeat_n(b'x', 2 << 20)
        .chain(std::iter::once(b'\n'))
        .collect();
    for (what, bytes) in [
        ("garbage bytes", garbage),
        ("an unterminated line", b"SELECT * FROM".to_vec()),
        ("a 2 MiB line", big),
    ] {
        send_and_drop(addr, timeout, &bytes);
        if let Err(e) = still_answers(addr, timeout, &table) {
            return ScenarioResult::Fail(format!(
                "after {what} on another connection, a fresh connection {e}"
            ));
        }
    }
    ScenarioResult::Pass
}

/// Client 0 creates `table`; 8 clients each insert one row at the same
/// moment; then every client must read all 8 rows.
fn concurrent_clients(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    use super::client::TcpCandidate;
    use super::run::Candidate;
    use std::sync::{Arc, Barrier};
    const N: usize = 8;
    let mut c0 = TcpCandidate::connect(addr, timeout)?;
    match c0.exec(&format!("CREATE TABLE {table} (k INTEGER, v TEXT)"))? {
        Response::Ok(0) => {}
        other => return Err(format!("CREATE TABLE answered {other:?}, expected OK 0")),
    }
    drop(c0);
    let barrier = Arc::new(Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let (addr, table, barrier) = (addr.to_string(), table.to_string(), barrier.clone());
            std::thread::spawn(move || -> Result<(), String> {
                // Every thread reaches both barriers, failed or not, so one
                // broken client cannot deadlock the others.
                let mut c = TcpCandidate::connect(&addr, timeout);
                barrier.wait();
                let ins = match &mut c {
                    Ok(c) => c.exec(&format!("INSERT INTO {table} VALUES ({i}, 'w{i}')")),
                    Err(e) => Err(e.clone()),
                };
                barrier.wait();
                match ins? {
                    Response::Ok(1) => {}
                    other => return Err(format!("client {i}: INSERT answered {other:?}")),
                }
                let mut c = c?;
                match c.exec(&format!("SELECT k FROM {table}"))? {
                    Response::Rows { rows, .. } => {
                        let mut ks: Vec<String> = rows.into_iter().flatten().collect();
                        ks.sort_by_key(|k| k.parse::<i64>().unwrap_or(i64::MAX));
                        let want: Vec<String> = (0..N).map(|k| k.to_string()).collect();
                        if ks != want {
                            return Err(format!(
                                "client {i} sees keys {ks:?} after all 8 inserts, expected {want:?}"
                            ));
                        }
                        Ok(())
                    }
                    other => Err(format!("client {i}: SELECT answered {other:?}")),
                }
            })
        })
        .collect();
    for h in handles {
        h.join()
            .map_err(|_| "a client thread panicked".to_string())??;
    }
    Ok(())
}

/// Write `bytes` on a fresh connection, half-close, drain briefly, drop.
/// Transport errors are expected (the server may close early) and ignored.
fn send_and_drop(addr: &str, timeout: Duration, bytes: &[u8]) {
    use std::io::Read;
    let Ok(mut s) = TcpStream::connect(addr) else {
        return;
    };
    let _ = s.set_write_timeout(Some(timeout));
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = s.write_all(bytes);
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut sink = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        match s.read(&mut sink) {
            Ok(n) if n > 0 => continue,
            _ => break,
        }
    }
}

/// A fresh connection answers a SELECT on the scenario table with its rows.
fn still_answers(addr: &str, timeout: Duration, table: &str) -> Result<(), String> {
    use super::client::TcpCandidate;
    use super::run::Candidate;
    let mut c = TcpCandidate::connect(addr, timeout).map_err(|e| format!("failed: {e}"))?;
    match c.exec(&format!("SELECT k FROM {table}")) {
        Ok(Response::Rows { end_count: 8, .. }) => Ok(()),
        Ok(other) => Err(format!("answered {other:?}, expected the 8 rows")),
        Err(e) => Err(format!("failed: {e}")),
    }
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
        let checked: Vec<crate::verifier::sql::generator::Checked> =
            script.setup.iter().chain(&script.steps).cloned().collect();
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

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// A correct server: one database, connections served in parallel.
        Good,
        /// Serves one connection at a time.
        Serial,
        /// Stops answering anyone after receiving a line over 1 MiB.
        DiesOnLongLine,
    }

    fn oracle_server(mode: Mode) -> String {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let oracle = Arc::new(Mutex::new(Oracle::new().unwrap()));
        let dead = Arc::new(AtomicBool::new(false));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                if dead.load(Ordering::SeqCst) {
                    drop(sock);
                    continue;
                }
                let (oracle, dead) = (oracle.clone(), dead.clone());
                let serve = move || {
                    let mut sock = sock;
                    sock.set_nodelay(true).unwrap();
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        if mode == Mode::DiesOnLongLine && line.len() > 1 << 20 {
                            dead.store(true, Ordering::SeqCst);
                            return;
                        }
                        let out = oracle.lock().unwrap().exec(line.trim_end());
                        let text = if out.is_discard() {
                            "ERR syntax".to_string()
                        } else {
                            super::super::compare::render_expected(&out)
                        };
                        if sock.write_all(format!("{text}\n").as_bytes()).is_err() {
                            return;
                        }
                        line.clear();
                    }
                };
                if mode == Mode::Serial {
                    serve();
                } else {
                    thread::spawn(serve);
                }
            }
        });
        addr
    }

    #[test]
    fn stage0_scenarios_pass_against_a_correct_server() {
        let addr = oracle_server(Mode::Good);
        for seed in [1u64, 2, 3] {
            assert_eq!(
                stage0(&addr, Duration::from_secs(10), seed),
                ScenarioResult::Pass,
                "seed {seed}"
            );
        }
    }

    #[test]
    fn stage0_catches_a_one_client_at_a_time_server() {
        let addr = oracle_server(Mode::Serial);
        match stage0(&addr, Duration::from_millis(400), 4) {
            ScenarioResult::Fail(d) => assert!(d.contains("8 concurrent clients"), "{d}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stage0_catches_a_server_killed_by_a_long_line() {
        let addr = oracle_server(Mode::DiesOnLongLine);
        match stage0(&addr, Duration::from_secs(2), 5) {
            ScenarioResult::Fail(d) => assert!(d.contains("after a 2 MiB line"), "{d}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn later_stages_have_no_scenarios_yet() {
        // Nothing listens here; a stage without scenarios never connects.
        assert_eq!(
            for_stage(3, "127.0.0.1:1", Duration::from_millis(100), 0),
            ScenarioResult::Pass
        );
    }
}
