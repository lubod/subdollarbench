//! The sdb-sql text protocol (docs/PROPOSAL-sdb-sql.md, "Text wire
//! protocol"): value encoding and the response reader.
//!
//! Request: one statement per line. Responses:
//!
//! ```text
//! OK <n>                  DML rows affected; DDL/BEGIN/COMMIT/ROLLBACK: OK 0
//! ROWS <ncols>            then one header line, the data lines, END <nrows>
//! ERR <class> <text>      one line; the connection stays usable
//! ```
//!
//! Cells are tab-separated. INTEGER is decimal; TEXT escapes `\` `\t` `\n`;
//! NULL is `\N`; the empty string is an empty field.
//!
//! The reader keeps cells RAW (still escaped): the wire carries no type
//! tags, so the comparison encodes the oracle's expected value and compares
//! strings. That also makes `+5`, `007` and `-0` mismatches, as intended.
//!
//! Framing note: the first line that is exactly `END <digits>` terminates a
//! result. The generator never emits TEXT beginning with `END`, so a data
//! line cannot be mistaken for it.

use super::oracle::Value;
use std::io::{BufRead, ErrorKind};
use std::time::Duration;

/// One decoded server response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ok(u64),
    Rows {
        /// The `<ncols>` the `ROWS` line declared.
        ncols: usize,
        headers: Vec<String>,
        /// Raw (still escaped) cells, one `Vec` per data line.
        rows: Vec<Vec<String>>,
        /// The `<nrows>` the `END` line declared.
        end_count: u64,
    },
    Err {
        class: String,
        text: String,
    },
}

/// Escape a TEXT value for the wire.
pub fn encode_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Inverse of [`encode_text`]; `\N` (NULL) is not text and is rejected.
pub fn decode_text(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(ch) = it.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match it.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(other) => return Err(format!("bad escape \\{other}")),
            None => return Err("dangling backslash".to_string()),
        }
    }
    Ok(out)
}

/// The exact wire form of an expected value.
pub fn encode_value(v: &Value) -> String {
    match v {
        Value::Null => "\\N".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Text(t) => encode_text(t),
    }
}

/// F1: turn a socket read failure into the words the candidate can act on.
/// A timeout is not a mysterious errno: it says how long the verifier waited
/// and which statement/step was waiting.
pub fn describe_read_error(e: &std::io::Error, timeout: Duration, what: &str) -> String {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        let secs = timeout.as_secs_f64();
        let n = if (secs.fract()).abs() < f64::EPSILON {
            format!("{}", secs as u64)
        } else {
            format!("{secs:.1}")
        };
        format!("no answer within {n}s to {what}: the server did not reply")
    } else {
        format!("read: {e}")
    }
}

/// Read one line without its `\n`. `Ok(None)` is a clean EOF.
fn read_line<R: BufRead>(
    r: &mut R,
    timeout: Option<(Duration, &str)>,
) -> Result<Option<String>, String> {
    let mut buf = String::new();
    match r.read_line(&mut buf) {
        Ok(0) => Ok(None),
        Ok(_) => {
            if buf.ends_with('\n') {
                buf.pop();
            }
            Ok(Some(buf))
        }
        Err(e) => Err(match timeout {
            Some((d, what)) => describe_read_error(&e, d, what),
            None => format!("read: {e}"),
        }),
    }
}

/// Is this line exactly `END <digits>`? Returns the count.
fn end_marker(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("END ")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// Read exactly one response. Errors are protocol violations or EOF; a
/// timeout surfaces as an I/O error from the underlying reader. The
/// no-context form is for in-memory readers and tests.
pub fn read_response<R: BufRead>(r: &mut R) -> Result<Response, String> {
    read_response_ctx(r, None)
}

/// [`read_response`] with the read timeout and a human-readable statement
/// or security step, so a timeout names what was waiting (F1).
pub fn read_response_ctx<R: BufRead>(
    r: &mut R,
    timeout: Option<(Duration, &str)>,
) -> Result<Response, String> {
    let first =
        read_line(r, timeout)?.ok_or_else(|| "connection closed before a response".to_string())?;
    if let Some(n) = first.strip_prefix("OK ") {
        let n: u64 = n
            .parse()
            .map_err(|_| format!("bad OK line {first:?}: count is not a number"))?;
        return Ok(Response::Ok(n));
    }
    if let Some(rest) = first.strip_prefix("ERR ") {
        let (class, text) = match rest.split_once(' ') {
            Some((c, t)) => (c.to_string(), t.to_string()),
            None => (rest.to_string(), String::new()),
        };
        return Ok(Response::Err { class, text });
    }
    if let Some(n) = first.strip_prefix("ROWS ") {
        let ncols: usize = n
            .parse()
            .map_err(|_| format!("bad ROWS line {first:?}: column count is not a number"))?;
        let header = read_line(r, timeout)?
            .ok_or_else(|| "connection closed before the header line".to_string())?;
        let headers: Vec<String> = header.split('\t').map(str::to_string).collect();
        let mut rows = Vec::new();
        loop {
            let line = read_line(r, timeout)?
                .ok_or_else(|| "connection closed before the END line".to_string())?;
            if let Some(end_count) = end_marker(&line) {
                return Ok(Response::Rows {
                    ncols,
                    headers,
                    rows,
                    end_count,
                });
            }
            rows.push(line.split('\t').map(str::to_string).collect());
        }
    }
    Err(format!(
        "unrecognized response line {first:?} (expected OK, ROWS or ERR)"
    ))
}

#[cfg(test)]
mod tests;
