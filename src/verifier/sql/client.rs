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
use super::wire::{read_response_ctx, Response};
use std::io::{BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A [`Candidate`] speaking the text protocol over one TCP connection.
pub struct TcpCandidate {
    reader: BufReader<TcpStream>,
    timeout: Duration,
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
            timeout,
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
        read_response_ctx(
            &mut self.reader,
            Some((self.timeout, "the previous statement")),
        )
    }

    /// The read timeout for subsequent reads; the in-flight probe wants a
    /// very short one.
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("set read timeout: {e}"))?;
        self.timeout = timeout;
        Ok(())
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
        let short: String = sql.trim().chars().take(80).collect();
        let what = format!("statement {short:?}");
        read_response_ctx(&mut self.reader, Some((self.timeout, &what)))
    }
}

#[cfg(test)]
mod tests;
