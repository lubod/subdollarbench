//! Sentinel datasets: proof that data written in an earlier ticket still
//! answers correctly after later tickets land (docs/PROPOSAL-sdb-sql.md,
//! "Upgrade check").
//!
//! A sentinel is generated once: its writes (`CREATE TABLE`, `INSERT`) run
//! on the candidate when that ticket passes, and its *ordered* reads are
//! recorded with the oracle's answers from exactly that state. Before a
//! later ticket's suite the reads run again — reads only, never the writes
//! — and any drift is an `upgrade` regression of the stage that wrote the
//! data.
//!
//! When the graded grammar has `ORDER BY` (ticket 2 passed), only reads
//! that order by every output column are kept, so the recorded wire frame
//! compares byte for byte. Without it, plain reads are kept and compare as
//! a multiset of rows: a ladder that failed ticket 2 is never asked to
//! sort (rules `.17`: a failed ticket is not required).

use super::client::TcpCandidate;
use super::compare::{render_expected, render_response};
use super::generator::{build_script, Kind, Profile};
use super::oracle::{Oracle, OracleOutcome};
use super::run::Candidate;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SentQuery {
    pub sql: String,
    /// The exact wire frame a correct server answers.
    pub expected: String,
    /// The read orders by every output column, so `expected` compares byte
    /// for byte; otherwise its rows compare in any order. Sentinels written
    /// before this field existed were all ordered.
    #[serde(default = "ordered_by_default")]
    pub ordered: bool,
}

fn ordered_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sentinel {
    pub stage: u32,
    pub seed: u64,
    /// The dataset, in order. Run once, when the owning ticket passed.
    pub writes: Vec<String>,
    /// Expected `OK`/`ROWS` frames for the writes (sanity when writing).
    pub write_expected: Vec<String>,
    /// Reads over the written data, with answers recorded at write time.
    pub queries: Vec<SentQuery>,
}

/// Step statements per sentinel script (setup on top).
pub const SENTINEL_STEPS: usize = 40;

/// Build the sentinel for a passing `stage` from a seeded script over
/// `profile` (the grammar the ladder actually passed, never the declared
/// one): creates and inserts are the dataset, selects are the reads.
/// Statements that are not kept are not executed either, so the oracle
/// state matches exactly what the candidate will hold.
pub fn build(seed: u64, stage: u32, profile: &Profile) -> Result<Sentinel, String> {
    let script = build_script(seed, profile, SENTINEL_STEPS);
    if !script.unknown.is_empty() {
        return Err(format!(
            "sentinel script: unmapped oracle messages {:?}",
            &script.unknown[..script.unknown.len().min(2)]
        ));
    }
    let oracle = Oracle::new()?;
    let mut sent = Sentinel {
        stage,
        seed,
        writes: Vec::new(),
        write_expected: Vec::new(),
        queries: Vec::new(),
    };
    // With `ORDER BY` in the grammar, keep only totally ordered reads (the
    // set this always kept); without it no read is ordered, so keep them all
    // and compare their rows as a multiset.
    let unordered_reads = !profile.order_limit;
    let mut pending_reads: Vec<(String, bool)> = Vec::new();
    for c in script.setup.iter().chain(&script.steps) {
        match c.stmt.kind {
            Kind::Create | Kind::Insert => {
                let out = oracle.exec(&c.stmt.sql);
                if out.is_discard() || matches!(out, OracleOutcome::Unknown(_)) {
                    continue;
                }
                sent.writes.push(c.stmt.sql.clone());
                sent.write_expected.push(render_expected(&out));
            }
            Kind::Select if c.stmt.ordered || unordered_reads => {
                pending_reads.push((c.stmt.sql.clone(), c.stmt.ordered))
            }
            _ => {}
        }
    }
    // Reads are recorded against the *final* dataset state: the sentinel
    // writer runs the whole dataset first and the reads after it, so the
    // script's original interleaving is not replayed.
    for (sql, ordered) in pending_reads {
        let out = oracle.exec(&sql);
        if out.is_discard() || matches!(out, OracleOutcome::Unknown(_)) {
            continue;
        }
        sent.queries.push(SentQuery {
            sql,
            expected: render_expected(&out),
            ordered,
        });
    }
    if sent.writes.is_empty() || sent.queries.is_empty() {
        return Err("sentinel has no dataset or no ordered reads".to_string());
    }
    Ok(sent)
}

fn frame(got: &Result<super::wire::Response, String>) -> Result<String, String> {
    match got {
        Ok(_) => Ok(render_response(got)),
        Err(e) => Err(e.clone()),
    }
}

/// Write the dataset onto the candidate and check the reads immediately
/// (the data must be readable right after it was written, or the sentinel
/// is not usable later). Any write failure is loud: the ticket just passed
/// its suite, so this is a verifier or candidate fault worth surfacing.
pub fn write(addr: &str, timeout: Duration, sent: &Sentinel) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout)?;
    for (sql, want) in sent.writes.iter().zip(&sent.write_expected) {
        let got = c.exec(sql).map_err(|e| format!("{sql:?}: {e}"))?;
        let frame = frame(&Ok(got))?;
        if !same_answer(&frame, want, true) {
            return Err(format!(
                "write {sql:?} answered {frame:?}, expected {want:?}"
            ));
        }
    }
    check_with(&mut c, sent)
}

/// Re-check a sentinel against a running candidate: reads only.
///
/// Errors are prefixed so the caller can tell them apart: `unreachable:`
/// means the candidate never answered (the suite will fail it anyway),
/// `drift:` means it answered and the recorded data is gone — the upgrade
/// regression.
pub fn check(addr: &str, timeout: Duration, sent: &Sentinel) -> Result<(), String> {
    let mut c = TcpCandidate::connect(addr, timeout).map_err(|e| format!("unreachable: {e}"))?;
    check_with(&mut c, sent)
}

fn check_with(c: &mut TcpCandidate, sent: &Sentinel) -> Result<(), String> {
    for (answered, q) in sent.queries.iter().enumerate() {
        let got = c.exec(&q.sql).map_err(|e| {
            // An answer, then a broken connection, is a regression — not
            // "the suite will fail it anyway". Only a store that never
            // answered is "unreachable".
            if answered == 0 {
                format!("unreachable: {:?}: {e}", q.sql)
            } else {
                format!(
                    "drift: transport error after {answered} answered read(s): {:?}: {e}",
                    q.sql
                )
            }
        })?;
        let frame = frame(&Ok(got))?;
        if !same_answer(&frame, &q.expected, q.ordered) {
            return Err(format!(
                "drift: read {:?} answered {frame:?}, expected {:?}{}",
                q.sql,
                q.expected,
                if q.ordered {
                    ""
                } else {
                    " (rows in any order)"
                }
            ));
        }
    }
    Ok(())
}

/// Whether `got` answers like `want`: byte for byte for an ordered read;
/// for an unordered one, the same `ROWS` header, column names and `END`
/// line with the same rows in any order. Anything but a `ROWS` frame (an
/// `OK`, an `ERR`, no usable response) must match exactly.
fn same_answer(got: &str, want: &str, ordered: bool) -> bool {
    if got == want {
        return true;
    }
    // Error answers are graded by class: the text is free-form and the
    // reference's wording differs. The stored expectations carry the class
    // only, while feedback renders the candidate's raw text (round-14 N5).
    if let (Some(g), Some(w)) = (err_class(got), err_class(want)) {
        return g == w;
    }
    !ordered && matches!((row_multiset(got), row_multiset(want)), (Some(a), Some(b)) if a == b)
}

/// The class token of an `ERR <class> <text>` frame.
fn err_class(frame: &str) -> Option<&str> {
    frame.strip_prefix("ERR ")?.split_whitespace().next()
}

/// A `ROWS` frame as (its fixed lines, its rows sorted). Rows are one line
/// each on the wire, so sorting lines sorts rows.
fn row_multiset(frame: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    if !frame.starts_with("ROWS ") {
        return None;
    }
    let lines: Vec<&str> = frame.split('\n').collect();
    if lines.len() < 3 {
        return None;
    }
    let (rows, end) = lines[2..].split_at(lines.len() - 3);
    let mut rows = rows.to_vec();
    rows.sort_unstable();
    Some(([&lines[..2], end].concat(), rows))
}

pub fn save(path: &Path, sent: &Sentinel) -> Result<(), String> {
    let json = serde_json::to_string_pretty(sent).map_err(|e| format!("sentinel json: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Load every `sentinel-<stage>.json` in `dir`, ordered by stage. Files
/// that do not parse are skipped with a message: a corrupt sentinel must
/// not wedge every later ticket.
pub fn load_all(dir: &Path) -> Vec<Sentinel> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !(name.starts_with("sentinel-") && name.ends_with(".json")) {
            continue;
        }
        match std::fs::read_to_string(e.path())
            .ok()
            .and_then(|t| serde_json::from_str::<Sentinel>(&t).ok())
        {
            Some(s) => out.push(s),
            None => eprintln!("sdb-sql: unreadable sentinel {}", e.path().display()),
        }
    }
    out.sort_by_key(|s| s.stage);
    out
}

#[cfg(test)]
mod tests {
    use super::super::oracle::Oracle;
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// One oracle-backed server with a shared database, like a real
    /// candidate: the data survives connections and (in the restart tests)
    /// only if the server keeps it.
    fn oracle_server() -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Oracle>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let oracle = std::sync::Arc::new(std::sync::Mutex::new(Oracle::new().unwrap()));
        let shared = oracle.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                let oracle = oracle.clone();
                std::thread::spawn(move || {
                    let mut sock = sock;
                    sock.set_nodelay(true).unwrap();
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        let out = oracle.lock().unwrap().exec(line.trim_end());
                        let text = if out.is_discard() {
                            "ERR syntax".to_string()
                        } else {
                            render_expected(&out)
                        };
                        if sock.write_all(format!("{text}\n").as_bytes()).is_err() {
                            return;
                        }
                        line.clear();
                    }
                });
            }
        });
        (addr, shared)
    }

    #[test]
    fn sentinel_round_trips_against_a_fresh_store() {
        let sent = build(11, 4, &Profile::through(4)).expect("sentinel builds");
        assert!(!sent.writes.is_empty() && !sent.queries.is_empty());
        let (addr, _db) = oracle_server();
        let addr = addr.to_string();
        write(&addr, Duration::from_secs(5), &sent).expect("write");
        check(&addr, Duration::from_secs(5), &sent).expect("check after write");
    }

    #[test]
    fn sentinel_check_catches_a_lost_store() {
        let sent = build(12, 4, &Profile::through(4)).expect("sentinel builds");
        // A fresh server never saw the writes: the reads must fail.
        let (addr, _db) = oracle_server();
        let err = check(&addr.to_string(), Duration::from_secs(5), &sent).unwrap_err();
        assert!(err.starts_with("drift:"), "{err}");
    }

    /// A store that answers the first read and then breaks is *drifting*,
    /// not "not checkable": swallowing it as unreachable used to hide the
    /// regression behind the new tables' suite.
    #[test]
    fn check_after_an_answer_is_drift_not_unreachable() {
        let sent = build(16, 4, &Profile::through(4)).unwrap();
        assert!(
            sent.queries.len() >= 2,
            "the fixture needs two reads to answer one and break on the next"
        );
        let expected = sent.queries[0].expected.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(sock) = stream else { break };
                let expected = expected.clone();
                std::thread::spawn(move || {
                    let mut sock = sock;
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    if matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        // The first read answers exactly as recorded...
                        let _ = sock.write_all(format!("{expected}\n").as_bytes());
                    }
                    // ...then the connection vanishes: the next read is
                    // unanswered on a store that demonstrably answered.
                });
            }
        });
        let err = check(&addr, Duration::from_secs(5), &sent).unwrap_err();
        assert!(err.starts_with("drift:"), "{err}");
        assert!(err.contains("after 1 answered read"), "{err}");
    }

    #[test]
    fn sentinel_is_deterministic_and_seed_namespaced() {
        let a = build(13, 4, &Profile::through(4)).unwrap();
        let b = build(13, 4, &Profile::through(4)).unwrap();
        let c = build(14, 4, &Profile::through(4)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a.writes, c.writes);
        // Reads are ordered: the recorded frame compares byte for byte.
        assert!(a
            .queries
            .iter()
            .all(|q| q.sql.to_lowercase().contains("order by")));
    }

    /// Review round 14, G1: a ladder that failed tickets 1-3 is graded on
    /// tickets 0 and 4 only, so ticket 4's dataset must not need a column
    /// list, `WHERE`, `ORDER BY` or `LIMIT` — and its reads still prove the
    /// data is there, as a multiset of rows.
    #[test]
    fn sentinel_without_tickets_1_to_3_uses_only_their_grammar() {
        let p = Profile::for_passed(&[0, 4]);
        for seed in 0..20u64 {
            let sent = build(seed, 4, &p).expect("sentinel builds without ORDER BY");
            assert!(!sent.writes.is_empty() && !sent.queries.is_empty());
            for w in &sent.writes {
                let w = w.to_lowercase();
                if let (Some(into), Some(values)) = (w.find("into"), w.find("values")) {
                    assert!(!w[into..values].contains('('), "column list: {w}");
                }
            }
            for q in &sent.queries {
                assert!(!q.ordered, "{}", q.sql);
                let sql = q.sql.to_lowercase();
                for kw in ["order", "limit", "where", "distinct"] {
                    assert!(!sql.contains(kw), "{kw} in {sql}");
                }
            }
        }
        // And it round-trips against a correct server.
        let sent = build(3, 4, &p).unwrap();
        let (addr, _db) = oracle_server();
        let addr = addr.to_string();
        write(&addr, Duration::from_secs(5), &sent).expect("write");
        check(&addr, Duration::from_secs(5), &sent).expect("check after write");
        // A fresh store still drifts.
        let (fresh, _db) = oracle_server();
        let err = check(&fresh.to_string(), Duration::from_secs(5), &sent).unwrap_err();
        assert!(
            err.starts_with("drift:") && err.contains("rows in any order"),
            "{err}"
        );
    }

    /// With ticket 2 passed the sentinel is exactly what it always was: only
    /// totally ordered reads, compared byte for byte.
    #[test]
    fn sentinel_with_order_by_keeps_only_ordered_reads() {
        let sent = build(13, 4, &Profile::through(4)).unwrap();
        assert!(sent.queries.iter().all(|q| q.ordered));
        // A sentinel saved before `ordered` existed loads as ordered.
        let mut json = serde_json::to_value(&sent).unwrap();
        for q in json["queries"].as_array_mut().unwrap() {
            q.as_object_mut().unwrap().remove("ordered");
        }
        let old: Sentinel = serde_json::from_value(json).unwrap();
        assert_eq!(old, sent);
    }

    #[test]
    fn unordered_reads_compare_rows_as_a_multiset() {
        let want = "ROWS 2\na\tb\n1\tx\n2\t\\N\n1\tx\nEND 3";
        let shuffled = "ROWS 2\na\tb\n2\t\\N\n1\tx\n1\tx\nEND 3";
        assert!(same_answer(shuffled, want, false));
        assert!(
            !same_answer(shuffled, want, true),
            "ordered reads stay byte for byte"
        );
        // A lost duplicate, a changed value, header or count is drift.
        for bad in [
            "ROWS 2\na\tb\n2\t\\N\n1\tx\nEND 2",
            "ROWS 2\na\tb\n2\t\\N\n1\tx\n1\ty\nEND 3",
            "ROWS 2\na\tc\n2\t\\N\n1\tx\n1\tx\nEND 3",
            "ROWS 2\na\tb\n2\t\\N\n1\tx\n1\tx\nEND 4",
            "ERR syntax",
        ] {
            assert!(!same_answer(bad, want, false), "{bad:?}");
        }
        assert!(same_answer("ROWS 1\na\nEND 0", "ROWS 1\na\nEND 0", false));
        assert!(same_answer("ERR syntax", "ERR syntax", false));
    }

    #[test]
    fn load_all_skips_junk_and_sorts() {
        let dir = std::env::temp_dir().join(format!("sdb-sentinel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        save(
            &dir.join("sentinel-4.json"),
            &build(15, 4, &Profile::through(4)).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("sentinel-x.json"), "{not json").unwrap();
        std::fs::write(dir.join("other.json"), "{}").unwrap();
        let all = load_all(&dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].stage, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
