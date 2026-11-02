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

/// Stage 4 (docs/PROPOSAL-sdb-sql.md, "4. Persistence"): write a seeded
/// dataset, restart the candidate on the same `--data`, read it back. The
/// restart is the pipeline's job (it owns the container), so it arrives as
/// a callback. Data that does not survive is a candidate failure; a
/// restart the harness cannot even attempt is reported with it.
///
/// Every suite that includes ticket 4 runs this. `profile` is the grammar
/// of the tickets 0-4 the ladder passed
/// ([`super::driver::persistence_profile`]): a failed ticket is not
/// required, so the dataset never uses it.
pub fn persistence(
    addr: &str,
    timeout: Duration,
    seed: u64,
    profile: &super::generator::Profile,
    restart: &mut dyn FnMut() -> Result<(), String>,
) -> ScenarioResult {
    let mut used_seed = None;
    persistence_with_seed(addr, timeout, seed, profile, restart, &mut used_seed)
}

/// R37 F5: the same path, reporting the sentinel seed it actually used.
/// The caller records it in the stage outcome; it is never part of the
/// candidate-visible failure detail.
pub fn persistence_with_seed(
    addr: &str,
    timeout: Duration,
    seed: u64,
    profile: &super::generator::Profile,
    restart: &mut dyn FnMut() -> Result<(), String>,
    used_seed: &mut Option<u64>,
) -> ScenarioResult {
    let pseed = seed ^ 0x5E27_A510_0000_0004;
    let sent = match super::sentinel::build(pseed, 4, profile) {
        Ok(s) => s,
        Err(e) => return ScenarioResult::Fault(format!("persistence dataset: {e}")),
    };
    *used_seed = Some(sent.seed);
    if let Err(e) = super::sentinel::write(addr, timeout, &sent) {
        return ScenarioResult::Fail(format!("persistence: writing the dataset: {e}"));
    }
    if let Err(e) = restart() {
        return ScenarioResult::Fail(format!("persistence: restart failed: {e}"));
    }
    if let Err(e) = super::sentinel::check(addr, timeout, &sent) {
        return ScenarioResult::Fail(format!("persistence: after a clean restart: {e}"));
    }
    ScenarioResult::Pass
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
mod tests;
