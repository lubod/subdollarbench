//! R45: the perf score (rules `.43`). The correctness ladder is unchanged;
//! this is a second axis, measured at the end of every phase on that
//! phase's final server.
//!
//! The candidate and the SQLite reference run the same workload in
//! alternating rounds, each in a container with the sandbox defaults
//! (2 CPUs, 2 GiB). Answers are checked against the same deterministic
//! data, so a fast wrong answer cannot buy a score. Per workload `r` is
//! `reference / candidate` for latency and `candidate / reference` for
//! throughput, clipped to [0.1, 100]. The latency workloads score the
//! median of the paired per-query ratios (the two servers are interleaved
//! inside each round, so host drift hits both); the throughput workloads
//! score the median of the per-round ratios. The perf score is the
//! geometric mean of the workload `r` values.
//!
//! The caller owns the bench lock and the servers' lifetime. [`run_pair`]
//! only talks TCP; [`run_workspace`] is the harness entrypoint that starts
//! the reference and a workspace in Docker and measures both.

use super::client::TcpCandidate;
use super::run::Candidate;
use super::wire::Response;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Lower/upper clip for one workload ratio. The upper clip keeps a fast
/// engine from being flattened on W5; the lower keeps one missed workload
/// from zeroing the whole score.
pub const CLIP_MIN: f64 = 0.1;
pub const CLIP_MAX: f64 = 100.0;
pub const WRONG_FLOOR: f64 = 0.1;

/// The number of client rows and the fixed table prefix seed. The sizes
/// are the contract's; tests pass a smaller [`PerfConfig`].
pub const PERF_SEED: u64 = 45;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkloadResult {
    pub id: String,
    /// `latency` or `throughput`.
    pub metric: String,
    /// The headline reference number (p50 ms, or commits/ops per second).
    pub reference: f64,
    /// The headline candidate number.
    pub candidate: f64,
    /// The median round ratio after clipping.
    pub ratio: f64,
    pub correct: bool,
    /// Every measured raw number (p50/p99, both client counts, ...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub numbers: BTreeMap<String, f64>,
    /// Why a side failed (transport, deadline, wrong answer count).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// True when the harness (the reference or our setup), not the
    /// candidate, failed. The block then has no score at all.
    #[serde(default, skip_serializing_if = "is_false")]
    pub harness_fault: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PerfScore {
    pub workloads: Vec<WorkloadResult>,
    /// `Some` only when the candidate was measured. A harness failure
    /// leaves no score, so the points tie-break skips the block instead of
    /// reading a floor that belongs to us, not the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// A harness-side failure (the reference or the setup), for the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PerfScore {
    pub fn from_workloads(workloads: Vec<WorkloadResult>) -> Self {
        let harness_fault = workloads.iter().any(|w| w.harness_fault);
        let ratios: Vec<f64> = workloads
            .iter()
            .map(|w| {
                if w.correct {
                    clip(w.ratio)
                } else {
                    WRONG_FLOOR
                }
            })
            .collect();
        let score = if harness_fault {
            None
        } else {
            Some(geometric_mean(&ratios).unwrap_or(WRONG_FLOOR))
        };
        Self {
            workloads,
            score,
            error: None,
        }
    }

    /// The candidate failed to build, start, answer or be correct: the
    /// workload floors and the block still has a score.
    pub fn floored(phase: u32, error: impl Into<String>) -> Self {
        let workloads = workloads_for_phase(phase)
            .iter()
            .map(|id| flawed(id, "perf run did not start"))
            .collect();
        let mut out = Self::from_workloads(workloads);
        out.error = Some(error.into());
        out
    }

    /// The harness (reference or setup) failed: no candidate score.
    pub fn harness_fault(phase: u32, error: impl Into<String>) -> Self {
        let workloads = workloads_for_phase(phase)
            .iter()
            .map(|id| harness_flawed(id, "harness fault"))
            .collect();
        let mut out = Self::from_workloads(workloads);
        out.error = Some(error.into());
        out
    }
}

pub fn clip(r: f64) -> f64 {
    if !r.is_finite() || r <= 0.0 {
        WRONG_FLOOR
    } else {
        r.clamp(CLIP_MIN, CLIP_MAX)
    }
}

pub fn geometric_mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let n = values.len() as f64;
    let sum: f64 = values.iter().map(|v| v.max(1e-12).ln()).sum();
    Some((sum / n).exp())
}

pub fn ratio(reference: f64, candidate: f64, throughput: bool) -> f64 {
    if reference <= 0.0 || candidate <= 0.0 || !reference.is_finite() || !candidate.is_finite() {
        return WRONG_FLOOR;
    }
    clip(if throughput {
        candidate / reference
    } else {
        reference / candidate
    })
}

/// Workloads a phase runs: W1/W4 start in phase 1; W3 in 2; W2/W5 in 3;
/// phase 4 runs every workload.
pub fn workloads_for_phase(phase: u32) -> &'static [&'static str] {
    match phase {
        1 => &["W1", "W4"],
        2 => &["W1", "W3", "W4"],
        3 | 4 => &["W1", "W2", "W3", "W4", "W5"],
        _ => &[],
    }
}

pub fn phase_of_workload(id: &str) -> Option<u32> {
    match id {
        "W1" | "W4" => Some(1),
        "W3" => Some(2),
        "W2" | "W5" => Some(3),
        _ => None,
    }
}

fn metric_of(id: &str) -> &'static str {
    match id {
        "W4" | "W5" => "throughput",
        _ => "latency",
    }
}

/// Human contract shown in the prompt and help.
pub const CONTRACT: &str = "Performance is a second axis next to points: points sort first, and a tie in points is broken by the perf score. At the end of every phase the harness runs the same workloads against your server and the SQLite reference, alternating rounds, each in a container with 2 CPUs and 2 GiB. W1 point lookup by key, 1 client, 100k rows (p50 and p99); W2 range + ORDER BY LIMIT, 1 client, 100k rows (p50); W3 two-table join + GROUP BY, 50k x 5k rows (p50); W4 durable autocommit single-row INSERT, 1 and 8 clients (commits/s; durable means each acknowledged commit is on disk before OK, and the harness floors W4 if the 1-client window shows fewer disk flushes than commits); W5 mixed 80/20 read/write, 16 clients, 30 s (throughput and p99). Per-workload r is reference/candidate for latency and candidate/reference for throughput, clipped to [0.1, 100]. W1 scores point-lookup latency (the median paired p50 ratio; p99 is reported); W4 combines its 1-client and 8-client rates with a geometric mean; W5 scores throughput (p99 is reported). The perf score is the geometric mean of the r values over the phase's workloads (1.0 = the reference, 2.0 = twice as fast). A wrong answer scores that workload at the floor (0.1), so speed never buys a wrong result. The perf run is harness time: it never spends the candidate budget and does not count toward the wall cap. Phase 1 measures W1 and W4; phase 2 adds W3; phase 3 adds W2 and W5; phase 4 runs all five.";

/// Sizes and durations. The default is the published contract; tests use
/// [`PerfConfig::small_for_tests`].
#[derive(Debug, Clone)]
pub struct PerfConfig {
    pub rounds: usize,
    pub seed: u64,
    pub rows_w1: u64,
    pub rows_w2: u64,
    pub rows_w3_a: u64,
    pub rows_w3_b: u64,
    pub rows_w5: u64,
    pub lookups: usize,
    pub ranges: usize,
    pub joins: usize,
    pub w4_seconds: f64,
    pub w5_seconds: f64,
    pub clients_w5: usize,
    pub timeout: Duration,
    pub workload_deadline: Duration,
}

impl Default for PerfConfig {
    fn default() -> Self {
        Self {
            rounds: 5,
            seed: PERF_SEED,
            rows_w1: 100_000,
            rows_w2: 100_000,
            rows_w3_a: 50_000,
            rows_w3_b: 5_000,
            rows_w5: 10_000,
            lookups: 500,
            ranges: 100,
            joins: 20,
            w4_seconds: 2.0,
            w5_seconds: 30.0,
            clients_w5: 16,
            timeout: Duration::from_secs(30),
            workload_deadline: Duration::from_secs(240),
        }
    }
}

impl PerfConfig {
    /// Small enough for the fake-engine unit tests. The ratios are the
    /// same; only the sizes and durations shrink.
    pub fn small_for_tests() -> Self {
        Self {
            rounds: 2,
            rows_w1: 2_000,
            rows_w2: 2_000,
            rows_w3_a: 500,
            rows_w3_b: 100,
            lookups: 40,
            ranges: 10,
            joins: 3,
            w4_seconds: 0.15,
            w5_seconds: 0.4,
            clients_w5: 4,
            timeout: Duration::from_secs(5),
            workload_deadline: Duration::from_secs(30),
            ..Self::default()
        }
    }

    /// The F5 path check: the full phase-4 shape (five workloads, five
    /// alternating rounds) with small data and a 5 s W5. Enough samples
    /// for the 1.0 ± 0.1 reference check without the 30 s workload.
    pub fn fixture_for_tests() -> Self {
        Self {
            rounds: 5,
            rows_w1: 2_000,
            rows_w2: 2_000,
            rows_w3_a: 500,
            rows_w3_b: 100,
            rows_w5: 2_000,
            lookups: 500,
            ranges: 100,
            joins: 20,
            w4_seconds: 0.5,
            w5_seconds: 5.0,
            clients_w5: 8,
            timeout: Duration::from_secs(10),
            workload_deadline: Duration::from_secs(120),
            ..Self::default()
        }
    }
}

/// Run the phase's workloads against two live servers. The caller owns the
/// bench lock; this function is synchronous and can take minutes.
pub fn run_pair(
    reference_addr: &str,
    candidate_addr: &str,
    phase: u32,
    cfg: &PerfConfig,
) -> PerfScore {
    run_pair_with_flush(reference_addr, candidate_addr, phase, cfg, None, None)
}

/// [`run_pair`] with the device flush counters for W4 (round 49 F2). The
/// counters are snapshotted around each side's 1-client window; a wrong
/// reference or an unreadable counter is a harness fault, a candidate with
/// fewer flushes than commits floors W4.
pub fn run_pair_with_flush(
    reference_addr: &str,
    candidate_addr: &str,
    phase: u32,
    cfg: &PerfConfig,
    reference_flush: Option<&dyn FlushCounter>,
    candidate_flush: Option<&dyn FlushCounter>,
) -> PerfScore {
    let nonce = nonce();
    let mut workloads = Vec::new();
    let mut error: Option<String> = None;
    for id in workloads_for_phase(phase) {
        let prefix = format!("p{}{}{:x}", cfg.seed, id.to_ascii_lowercase(), nonce);
        let (workload, harness_error) = match *id {
            "W1" => run_w1(reference_addr, candidate_addr, &prefix, cfg),
            "W2" => run_w2(reference_addr, candidate_addr, &prefix, cfg),
            "W3" => run_w3(reference_addr, candidate_addr, &prefix, cfg),
            "W4" => run_w4(
                reference_addr,
                candidate_addr,
                &prefix,
                cfg,
                reference_flush,
                candidate_flush,
            ),
            "W5" => run_w5(reference_addr, candidate_addr, &prefix, cfg),
            other => (
                flawed(other, "unknown workload"),
                Some(format!("unknown workload {other}")),
            ),
        };
        let fault = workload.harness_fault;
        if fault {
            if error.is_none() {
                error = workload.note.clone().or(harness_error);
            }
            workloads.push(workload);
            break;
        }
        if error.is_none() {
            error = harness_error;
        }
        workloads.push(workload);
    }
    let mut score = PerfScore::from_workloads(workloads);
    score.error = error;
    score
}

fn nonce() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn round6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1e6).round() / 1e6
    } else {
        0.0
    }
}

/// The upper median; an empty slice is 0.
fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

/// Nearest-rank percentile; an empty slice is 0.
fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (p * values.len() as f64).ceil() as usize;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn flawed(id: &str, note: impl Into<String>) -> WorkloadResult {
    WorkloadResult {
        id: id.to_string(),
        metric: metric_of(id).to_string(),
        reference: 0.0,
        candidate: 0.0,
        ratio: WRONG_FLOOR,
        correct: false,
        numbers: BTreeMap::new(),
        note: Some(note.into()),
        harness_fault: false,
    }
}

/// The harness failed for this workload: no candidate score.
fn harness_flawed(id: &str, note: impl Into<String>) -> WorkloadResult {
    let mut out = flawed(id, note);
    out.harness_fault = true;
    out
}

fn result_with(
    id: &str,
    reference: f64,
    candidate: f64,
    ratio: f64,
    correct: bool,
    numbers: BTreeMap<String, f64>,
    note: Option<String>,
) -> WorkloadResult {
    WorkloadResult {
        id: id.to_string(),
        metric: metric_of(id).to_string(),
        reference: round6(reference),
        candidate: round6(candidate),
        ratio: clip(ratio),
        correct,
        numbers,
        note,
        harness_fault: false,
    }
}

fn connect(addr: &str, timeout: Duration) -> Result<TcpCandidate, String> {
    TcpCandidate::connect(addr, timeout)
}

/// R49: a `busy` is the dialect's transient concurrency answer, not a
/// wrong result. W4/W5 run many clients against one writer; retry a few
/// times before treating a busy as a failure.
fn exec_busy(c: &mut TcpCandidate, sql: &str) -> Result<Response, String> {
    let mut last = c.exec(sql)?;
    for _ in 0..4 {
        match &last {
            Response::Err { class, .. } if class == "busy" => {
                std::thread::sleep(Duration::from_millis(2));
                last = c.exec(sql)?;
            }
            _ => break,
        }
    }
    Ok(last)
}

fn create_table(c: &mut TcpCandidate, sql: &str) -> Result<(), String> {
    match c.exec(sql)? {
        Response::Ok(0) => Ok(()),
        Response::Err { class, text } if class == "exists" => {
            Err(format!("{sql}: table already exists ({text})"))
        }
        other => Err(format!("{sql}: expected OK 0, got {other:?}")),
    }
}

fn insert_rows(c: &mut TcpCandidate, sql: &str, expected: u64) -> Result<(), String> {
    match c.exec(sql)? {
        Response::Ok(n) if n == expected => Ok(()),
        other => Err(format!("{sql}: expected OK {expected}, got {other:?}")),
    }
}

/// `(k INTEGER, v TEXT)` with `v = 'r<k>'` for every key in `0..rows`.
fn prepare_kv(addr: &str, table: &str, rows: u64, cfg: &PerfConfig) -> Result<(), String> {
    let mut c = connect(addr, cfg.timeout)?;
    create_table(&mut c, &format!("CREATE TABLE {table} (k INTEGER, v TEXT)"))?;
    let batch = 500u64;
    let mut k = 0u64;
    while k < rows {
        let end = (k + batch).min(rows);
        let vals: Vec<String> = (k..end).map(|i| format!("({i},'r{i}')")).collect();
        let sql = format!("INSERT INTO {table} VALUES {}", vals.join(","));
        insert_rows(&mut c, &sql, end - k)?;
        k = end;
    }
    Ok(())
}

fn prepare_empty(addr: &str, table: &str, cfg: &PerfConfig) -> Result<(), String> {
    let mut c = connect(addr, cfg.timeout)?;
    create_table(
        &mut c,
        &format!("CREATE TABLE {table} (id INTEGER, v TEXT)"),
    )
}

fn prepare_join(addr: &str, prefix: &str, cfg: &PerfConfig) -> Result<(), String> {
    let mut c = connect(addr, cfg.timeout)?;
    create_table(
        &mut c,
        &format!("CREATE TABLE {prefix}a (id INTEGER, g INTEGER, v TEXT)"),
    )?;
    create_table(
        &mut c,
        &format!("CREATE TABLE {prefix}b (id INTEGER, aid INTEGER, w TEXT)"),
    )?;
    let batch = 500u64;
    let mut k = 0u64;
    while k < cfg.rows_w3_a {
        let end = (k + batch).min(cfg.rows_w3_a);
        let vals: Vec<String> = (k..end)
            .map(|i| format!("({i},{},'a{i}')", i % 50))
            .collect();
        let sql = format!("INSERT INTO {prefix}a VALUES {}", vals.join(","));
        insert_rows(&mut c, &sql, end - k)?;
        k = end;
    }
    let mut k = 0u64;
    while k < cfg.rows_w3_b {
        let end = (k + batch).min(cfg.rows_w3_b);
        let vals: Vec<String> = (k..end).map(|i| format!("({i},{i},'b{i}')")).collect();
        let sql = format!("INSERT INTO {prefix}b VALUES {}", vals.join(","));
        insert_rows(&mut c, &sql, end - k)?;
        k = end;
    }
    Ok(())
}

/// The expected `(g, COUNT(*), SUM(b.id))` rows for the W3 join. The first
/// `rows_b` ids are `aid = id`, so group `g` gets every id congruent to
/// `g` modulo 50.
fn join_expected(rows_b: u64) -> Vec<(i64, i64, i64)> {
    let mut out = vec![(0i64, 0i64, 0i64); 50];
    for id in 0..rows_b {
        let g = (id % 50) as usize;
        out[g].0 = g as i64;
        out[g].1 += 1;
        out[g].2 += id as i64;
    }
    out.into_iter().filter(|(_, count, _)| *count > 0).collect()
}

/// The last few inserted rows must read back exactly. COUNT(*) is ticket 7
/// (and W4 covers tickets 4-6), so the check uses point lookups only.
fn sample_rows(
    addr: &str,
    table: &str,
    key_col: &str,
    next_id: u64,
    count: u64,
    cfg: &PerfConfig,
) -> bool {
    let mut c = match connect(addr, cfg.timeout) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let first = next_id.saturating_sub(count).max(1);
    for id in first..next_id {
        let want = format!("r{id}");
        let sql = format!("SELECT v FROM {table} WHERE {key_col} = {id}");
        match c.exec(&sql) {
            Ok(r) if !wrong_row(&r, &want) => {}
            _ => return false,
        }
    }
    true
}

fn wrong_row(r: &Response, want: &str) -> bool {
    !matches!(
        r,
        Response::Rows { rows, .. }
            if rows.len() == 1 && rows[0].first() == Some(&want.to_string())
    )
}

// ---------------------------------------------------------------- latency

fn timed(c: &mut TcpCandidate, sql: &str) -> Result<(f64, Response), String> {
    let t0 = Instant::now();
    let r = c.exec(sql)?;
    Ok((ms(t0.elapsed()), r))
}

struct PairedRound {
    reference: Vec<f64>,
    candidate: Vec<f64>,
    /// Per-query reference/candidate latency ratios.
    ratios: Vec<f64>,
    /// The reference's answers are the harness's responsibility.
    reference_correct: bool,
    candidate_correct: bool,
}

/// Run one round of a latency workload against both servers with the two
/// queries interleaved. Alternating the order inside the round (and the
/// rounds themselves) makes host drift hit both sides together; the score
/// uses the median paired ratio, not the ratio of two half-round medians.
fn paired_round<F, G>(
    reference_addr: &str,
    candidate_addr: &str,
    cfg: &PerfConfig,
    deadline: Instant,
    count: usize,
    mut sql_for: F,
    mut check: G,
) -> Result<PairedRound, String>
where
    F: FnMut(usize) -> String,
    G: FnMut(usize, &Response) -> bool,
{
    let mut r = connect(reference_addr, cfg.timeout)?;
    let mut c = connect(candidate_addr, cfg.timeout)?;
    let mut out = PairedRound {
        reference: Vec::with_capacity(count),
        candidate: Vec::with_capacity(count),
        ratios: Vec::with_capacity(count),
        reference_correct: true,
        candidate_correct: true,
    };
    for i in 0..count {
        if Instant::now() >= deadline {
            return Err(format!("workload deadline after {i} queries"));
        }
        let sql = sql_for(i);
        let (rt, rr, ct, cr) = if i % 2 == 0 {
            let (rt, rr) = timed(&mut r, &sql).map_err(|e| format!("reference: {e}"))?;
            let (ct, cr) = timed(&mut c, &sql).map_err(|e| format!("candidate: {e}"))?;
            (rt, rr, ct, cr)
        } else {
            let (ct, cr) = timed(&mut c, &sql).map_err(|e| format!("candidate: {e}"))?;
            let (rt, rr) = timed(&mut r, &sql).map_err(|e| format!("reference: {e}"))?;
            (rt, rr, ct, cr)
        };
        out.reference_correct &= check(i, &rr);
        out.candidate_correct &= check(i, &cr);
        out.reference.push(rt);
        out.candidate.push(ct);
        if ct > 0.0 {
            out.ratios.push(rt / ct);
        }
    }
    Ok(out)
}

/// A workload whose score is a latency ratio. `rounds` paired rounds are
/// accumulated; the headline numbers are the p50/p99 over every sample and
/// the ratio is the median of the per-query ratios.
#[allow(clippy::too_many_arguments)] // the workload id, two servers, setup, config, count, SQL, check
fn run_latency<F, G>(
    id: &str,
    reference_addr: &str,
    candidate_addr: &str,
    setup: impl Fn(&str) -> Result<(), String>,
    cfg: &PerfConfig,
    count: usize,
    mut sql_for: F,
    mut check: G,
) -> (WorkloadResult, Option<String>)
where
    F: FnMut(usize) -> String,
    G: FnMut(usize, &Response) -> bool,
{
    if let Err(e) = setup(reference_addr) {
        let note = format!("reference setup: {e}");
        return (harness_flawed(id, &note), Some(note));
    }
    if let Err(e) = setup(candidate_addr) {
        return (flawed(id, format!("candidate setup: {e}")), None);
    }
    let deadline = Instant::now() + cfg.workload_deadline;
    let mut ref_lat = Vec::new();
    let mut cand_lat = Vec::new();
    let mut ratios = Vec::new();
    let mut correct = true;
    let mut note = None;
    for _ in 0..cfg.rounds {
        let out = match paired_round(
            reference_addr,
            candidate_addr,
            cfg,
            deadline,
            count,
            &mut sql_for,
            &mut check,
        ) {
            Ok(v) if !v.reference_correct => {
                let note = format!("the reference answered wrong in {id}");
                return (harness_flawed(id, &note), Some(note));
            }
            Ok(v) => v,
            Err(e) if e.starts_with("reference:") => {
                return (harness_flawed(id, e.clone()), Some(e));
            }
            Err(e) => {
                correct = false;
                note = Some(e);
                break;
            }
        };
        correct &= out.candidate_correct;
        ref_lat.extend(out.reference);
        cand_lat.extend(out.candidate);
        ratios.extend(out.ratios);
    }
    let ref50 = percentile(&mut ref_lat, 0.50);
    let ref99 = percentile(&mut ref_lat, 0.99);
    let cand50 = percentile(&mut cand_lat, 0.50);
    let cand99 = percentile(&mut cand_lat, 0.99);
    let ratio = median(&mut ratios);
    let mut numbers = BTreeMap::new();
    numbers.insert("reference_p50_ms".into(), round6(ref50));
    numbers.insert("reference_p99_ms".into(), round6(ref99));
    numbers.insert("candidate_p50_ms".into(), round6(cand50));
    numbers.insert("candidate_p99_ms".into(), round6(cand99));
    (
        result_with(
            id,
            ref50,
            cand50,
            ratio,
            correct && !ratios.is_empty() && !ref_lat.is_empty() && !cand_lat.is_empty(),
            numbers,
            note,
        ),
        None,
    )
}

// ---------------------------------------------------------------- W1

fn run_w1(
    reference_addr: &str,
    candidate_addr: &str,
    table: &str,
    cfg: &PerfConfig,
) -> (WorkloadResult, Option<String>) {
    let rows = cfg.rows_w1.max(1);
    run_latency(
        "W1",
        reference_addr,
        candidate_addr,
        |addr| prepare_kv(addr, table, cfg.rows_w1, cfg),
        cfg,
        cfg.lookups,
        |i| {
            let k = (i as u64 * 7919) % rows;
            format!("SELECT v FROM {table} WHERE k = {k}")
        },
        |i, r| {
            let k = (i as u64 * 7919) % rows;
            let want = format!("r{k}");
            !wrong_row(r, &want)
        },
    )
}

// ---------------------------------------------------------------- W2

fn run_w2(
    reference_addr: &str,
    candidate_addr: &str,
    table: &str,
    cfg: &PerfConfig,
) -> (WorkloadResult, Option<String>) {
    const WINDOW: u64 = 100;
    const LIMIT: u64 = 20;
    let span = cfg.rows_w2.saturating_sub(WINDOW + 1).max(1);
    run_latency(
        "W2",
        reference_addr,
        candidate_addr,
        |addr| prepare_kv(addr, table, cfg.rows_w2, cfg),
        cfg,
        cfg.ranges,
        |i| {
            let a = (i as u64 * 1993) % span;
            format!(
                "SELECT v FROM {table} WHERE k >= {a} AND k < {} ORDER BY k LIMIT {LIMIT}",
                a + WINDOW
            )
        },
        |i, r| {
            let a = (i as u64 * 1993) % span;
            let first = format!("r{a}");
            let last = format!("r{}", a + LIMIT - 1);
            matches!(
                r,
                Response::Rows { rows, .. }
                    if rows.len() == LIMIT as usize
                        && rows[0].first() == Some(&first)
                        && rows.last().and_then(|row| row.first()) == Some(&last)
            )
        },
    )
}

// ---------------------------------------------------------------- W3

fn run_w3(
    reference_addr: &str,
    candidate_addr: &str,
    prefix: &str,
    cfg: &PerfConfig,
) -> (WorkloadResult, Option<String>) {
    let expected = join_expected(cfg.rows_w3_b);
    let sql = format!(
        "SELECT a.g, COUNT(*), SUM(b.id) FROM {prefix}a a JOIN {prefix}b b ON a.id = b.aid \
         GROUP BY a.g ORDER BY a.g"
    );
    run_latency(
        "W3",
        reference_addr,
        candidate_addr,
        |addr| prepare_join(addr, prefix, cfg),
        cfg,
        cfg.joins,
        |_| sql.clone(),
        |_, r| {
            let parsed: Option<Vec<(i64, i64, i64)>> = match r {
                Response::Rows { rows, .. } => rows
                    .iter()
                    .map(|row| {
                        Some((
                            row.first()?.parse().ok()?,
                            row.get(1)?.parse().ok()?,
                            row.get(2)?.parse().ok()?,
                        ))
                    })
                    .collect(),
                _ => None,
            };
            parsed.as_deref() == Some(expected.as_slice())
        },
    )
}

// ---------------------------------------------------------------- W4

/// R49 F2: the device's completed flush-request counter (sysfs field 16).
/// The unit tests inject a deterministic source; a real run reads the
/// data dir's backing disk.
pub trait FlushCounter: Send + Sync {
    fn flushes(&self) -> Result<u64, String>;
}

/// The sysfs sources for a data dir. The direct `st_dev` counter can stay
/// at zero under LVM/dm while the physical disk counts the flushes, so the
/// disk(s) under the device are the primary sources; the direct stat is
/// the fallback. `flushes` returns the largest readable cumulative value;
/// the W4 window compares two snapshots, and other host activity only
/// adds flushes (the spec's 0.9 threshold tolerates it).
pub struct DeviceFlushCounter {
    paths: Vec<PathBuf>,
}

fn stat_flushes(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().nth(15)?.parse().ok()
}

impl DeviceFlushCounter {
    pub fn for_dir(dir: &Path) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(dir)
            .map_err(|e| format!("stat {}: {e}", dir.display()))?
            .dev();
        let (major, minor) = (libc::major(dev), libc::minor(dev));
        let link = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
        let mut paths = Vec::new();
        if let Ok(real) = std::fs::canonicalize(&link) {
            // A partition's parent is the whole disk; an LVM/dm device's
            // slaves are partitions whose parent is the disk.
            if let Some(parent) = real.parent() {
                paths.push(parent.join("stat"));
            }
            if let Ok(entries) = std::fs::read_dir(real.join("slaves")) {
                for e in entries.flatten() {
                    if let Ok(slave) = std::fs::canonicalize(e.path()) {
                        if let Some(parent) = slave.parent() {
                            paths.push(parent.join("stat"));
                        }
                        paths.push(slave.join("stat"));
                    }
                }
            }
        }
        paths.push(link.join("stat"));
        paths.retain(|p| p.exists());
        if paths.is_empty() {
            return Err(format!("no sysfs flush counter for {major}:{minor}"));
        }
        Ok(Self { paths })
    }
}

impl FlushCounter for DeviceFlushCounter {
    fn flushes(&self) -> Result<u64, String> {
        let mut best: Option<u64> = None;
        for path in &self.paths {
            if let Some(v) = stat_flushes(path) {
                best = Some(best.map_or(v, |b| b.max(v)));
            }
        }
        best.ok_or_else(|| "no readable flush counter".to_string())
    }
}

struct Throughput {
    inserted: u64,
    next_id: u64,
    elapsed: f64,
    correct: bool,
    latencies: Vec<f64>,
    /// Flush requests during the 1-client window; None for the 8-client
    /// window and when no counter is injected.
    flushes: Option<u64>,
}

fn w4_side(
    addr: &str,
    table: &str,
    clients: usize,
    start_id: u64,
    cfg: &PerfConfig,
    flush: Option<&dyn FlushCounter>,
) -> Result<Throughput, String> {
    let before = flush
        .map(|f| f.flushes().map_err(|e| format!("flush: {e}")))
        .transpose()?;
    let barrier = Arc::new(Barrier::new(clients + 1));
    let next = Arc::new(AtomicU64::new(start_id));
    let run_for = Duration::from_secs_f64(cfg.w4_seconds);
    let mut handles = Vec::with_capacity(clients);
    for _ in 0..clients {
        let addr = addr.to_string();
        let table = table.to_string();
        let barrier = barrier.clone();
        let next = next.clone();
        let timeout = cfg.timeout;
        handles.push(std::thread::spawn(
            move || -> Result<(u64, bool, f64, Vec<f64>), String> {
                let mut c = connect(&addr, timeout)?;
                barrier.wait();
                let start = Instant::now();
                let deadline = start + run_for;
                let mut inserted = 0u64;
                let mut ok = true;
                let mut lat = Vec::new();
                while Instant::now() < deadline {
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    let sql = format!("INSERT INTO {table} VALUES ({id}, 'r{id}')");
                    let t0 = Instant::now();
                    let r = exec_busy(&mut c, &sql)?;
                    lat.push(ms(t0.elapsed()));
                    match r {
                        Response::Ok(1) => inserted += 1,
                        _ => ok = false,
                    }
                }
                Ok((inserted, ok, start.elapsed().as_secs_f64(), lat))
            },
        ));
    }
    barrier.wait();
    let mut inserted = 0u64;
    let mut correct = true;
    let mut elapsed = 0.0f64;
    let mut latencies = Vec::new();
    for h in handles {
        let (n, ok, t, lat) = h
            .join()
            .map_err(|_| "insert client panicked".to_string())??;
        inserted += n;
        correct &= ok;
        elapsed = elapsed.max(t);
        latencies.extend(lat);
    }
    let next_id = next.load(Ordering::Relaxed);
    if !sample_rows(addr, table, "id", next_id, 5, cfg) {
        correct = false;
    }
    let flushes = match (before, flush) {
        (Some(before), Some(f)) => Some(
            f.flushes()
                .map_err(|e| format!("flush: {e}"))?
                .saturating_sub(before),
        ),
        _ => None,
    };
    Ok(Throughput {
        inserted,
        next_id,
        elapsed: elapsed.max(1e-6),
        correct,
        latencies,
        flushes,
    })
}

fn throughput_of(t: &Throughput) -> f64 {
    t.inserted as f64 / t.elapsed
}

fn run_w4(
    reference_addr: &str,
    candidate_addr: &str,
    table: &str,
    cfg: &PerfConfig,
    reference_flush: Option<&dyn FlushCounter>,
    candidate_flush: Option<&dyn FlushCounter>,
) -> (WorkloadResult, Option<String>) {
    if let Err(e) = prepare_empty(reference_addr, table, cfg) {
        let note = format!("reference setup: {e}");
        return (harness_flawed("W4", &note), Some(note));
    }
    if let Err(e) = prepare_empty(candidate_addr, table, cfg) {
        return (flawed("W4", format!("candidate setup: {e}")), None);
    }
    let mut ratios = Vec::new();
    let mut ref1s = Vec::new();
    let mut ref8s = Vec::new();
    let mut cand1s = Vec::new();
    let mut cand8s = Vec::new();
    let mut ref99s = Vec::new();
    let mut cand99s = Vec::new();
    let mut correct = true;
    let mut note = None;
    let mut ref_next = 1u64;
    let mut cand_next = 1u64;
    let mut ref1_flushes = None;
    let mut cand1_flushes = None;
    let mut ref1_commits = None;
    let mut cand1_commits = None;
    for _ in 0..cfg.rounds {
        let mut round_ref = [0.0f64; 2];
        let mut round_cand = [0.0f64; 2];
        for (idx, clients) in [1usize, 8].into_iter().enumerate() {
            let ref_flush = if clients == 1 { reference_flush } else { None };
            let r = match w4_side(reference_addr, table, clients, ref_next, cfg, ref_flush) {
                Ok(v) => v,
                Err(e) => {
                    let note = format!("reference: {e}");
                    return (harness_flawed("W4", &note), Some(note));
                }
            };
            ref_next = r.next_id;
            if !r.correct {
                let note = "the reference insert check failed".to_string();
                return (harness_flawed("W4", &note), Some(note));
            }
            if clients == 1 {
                ref1_flushes = r.flushes;
                ref1_commits = Some(r.inserted);
                if let Some(flushes) = r.flushes {
                    if r.inserted > 0 && (flushes as f64) < 0.9 * r.inserted as f64 {
                        let note = format!(
                            "the reference is not durable: {flushes} flushes for {} commits",
                            r.inserted
                        );
                        return (harness_flawed("W4", &note), Some(note));
                    }
                }
            }
            round_ref[idx] = throughput_of(&r);
            let mut ref_lat = r.latencies;
            ref99s.push(percentile(&mut ref_lat, 0.99));
            let cand_flush = if clients == 1 { candidate_flush } else { None };
            let c = match w4_side(candidate_addr, table, clients, cand_next, cfg, cand_flush) {
                Ok(v) => v,
                Err(e) if e.starts_with("flush:") => {
                    return (harness_flawed("W4", e.clone()), Some(e));
                }
                Err(e) => {
                    correct = false;
                    note = Some(format!("candidate: {e}"));
                    break;
                }
            };
            cand_next = c.next_id;
            if !c.correct {
                correct = false;
                note = Some("the candidate insert check failed".into());
                break;
            }
            if clients == 1 {
                cand1_flushes = c.flushes;
                cand1_commits = Some(c.inserted);
                if let Some(flushes) = c.flushes {
                    if c.inserted > 0 && (flushes as f64) < 0.9 * c.inserted as f64 {
                        correct = false;
                        note = Some(format!(
                            "not durable: {flushes} flushes for {} commits",
                            c.inserted
                        ));
                        break;
                    }
                }
            }
            round_cand[idx] = throughput_of(&c);
            let mut cand_lat = c.latencies;
            cand99s.push(percentile(&mut cand_lat, 0.99));
        }
        if round_ref[0] > 0.0 && round_ref[1] > 0.0 && round_cand[0] > 0.0 && round_cand[1] > 0.0 {
            let r = (ratio(round_ref[0], round_cand[0], true)
                * ratio(round_ref[1], round_cand[1], true))
            .sqrt();
            ratios.push(r);
        } else {
            correct = false;
        }
        ref1s.push(round_ref[0]);
        ref8s.push(round_ref[1]);
        cand1s.push(round_cand[0]);
        cand8s.push(round_cand[1]);
        if !correct {
            break;
        }
    }
    let ref1 = median(&mut ref1s);
    let ref8 = median(&mut ref8s);
    let cand1 = median(&mut cand1s);
    let cand8 = median(&mut cand8s);
    let ratio = median(&mut ratios);
    let ref99 = median(&mut ref99s);
    let cand99 = median(&mut cand99s);
    let mut numbers = BTreeMap::new();
    numbers.insert("reference_p99_ms".into(), round6(ref99));
    numbers.insert("candidate_p99_ms".into(), round6(cand99));
    numbers.insert("reference_1_client_per_s".into(), round6(ref1));
    numbers.insert("reference_8_clients_per_s".into(), round6(ref8));
    numbers.insert("candidate_1_client_per_s".into(), round6(cand1));
    numbers.insert("candidate_8_clients_per_s".into(), round6(cand8));
    if let Some(v) = ref1_commits {
        numbers.insert("reference_1_client_commits".into(), v as f64);
    }
    if let Some(v) = cand1_commits {
        numbers.insert("candidate_1_client_commits".into(), v as f64);
    }
    if let Some(v) = ref1_flushes {
        numbers.insert("reference_1_client_flushes".into(), v as f64);
    }
    if let Some(v) = cand1_flushes {
        numbers.insert("candidate_1_client_flushes".into(), v as f64);
    }
    (
        result_with(
            "W4",
            ref8,
            cand8,
            ratio,
            correct && !ratios.is_empty(),
            numbers,
            note,
        ),
        None,
    )
}

// ---------------------------------------------------------------- W5

struct Mixed {
    ops: u64,
    writes: u64,
    next_id: u64,
    elapsed: f64,
    correct: bool,
    latencies: Vec<f64>,
}

fn w5_side(addr: &str, table: &str, start_id: u64, cfg: &PerfConfig) -> Result<Mixed, String> {
    let clients = cfg.clients_w5.max(1);
    let barrier = Arc::new(Barrier::new(clients + 1));
    let ops = Arc::new(AtomicU64::new(0));
    let next = Arc::new(AtomicU64::new(start_id));
    let run_for = Duration::from_secs_f64(cfg.w5_seconds);
    let mut handles = Vec::with_capacity(clients);
    for _ in 0..clients {
        let addr = addr.to_string();
        let table = table.to_string();
        let barrier = barrier.clone();
        let ops = ops.clone();
        let next = next.clone();
        let timeout = cfg.timeout;
        let rows = cfg.rows_w5.max(1);
        handles.push(std::thread::spawn(
            move || -> Result<(u64, u64, bool, f64, Vec<f64>), String> {
                let mut c = connect(&addr, timeout)?;
                barrier.wait();
                let start = Instant::now();
                let deadline = start + run_for;
                let mut n = 0u64;
                let mut writes = 0u64;
                let mut ok = true;
                let mut lat = Vec::new();
                while Instant::now() < deadline {
                    let i = ops.fetch_add(1, Ordering::Relaxed);
                    if i.is_multiple_of(5) {
                        let id = next.fetch_add(1, Ordering::Relaxed);
                        let sql = format!("INSERT INTO {table} VALUES ({id}, 'r{id}')");
                        let t0 = Instant::now();
                        let r = exec_busy(&mut c, &sql)?;
                        lat.push(ms(t0.elapsed()));
                        match r {
                            Response::Ok(1) => {
                                n += 1;
                                writes += 1;
                            }
                            _ => ok = false,
                        }
                    } else {
                        let k = i.wrapping_mul(7919) % rows;
                        let want = format!("r{k}");
                        let sql = format!("SELECT v FROM {table} WHERE k = {k}");
                        let t0 = Instant::now();
                        let r = exec_busy(&mut c, &sql)?;
                        lat.push(ms(t0.elapsed()));
                        if wrong_row(&r, &want) {
                            ok = false;
                        }
                        n += 1;
                    }
                }
                Ok((n, writes, ok, start.elapsed().as_secs_f64(), lat))
            },
        ));
    }
    barrier.wait();
    let mut total_ops = 0u64;
    let mut writes = 0u64;
    let mut correct = true;
    let mut elapsed = 0.0f64;
    let mut latencies = Vec::new();
    for h in handles {
        let (n, w, ok, t, lat) = h
            .join()
            .map_err(|_| "mixed client panicked".to_string())??;
        total_ops += n;
        writes += w;
        correct &= ok;
        elapsed = elapsed.max(t);
        latencies.extend(lat);
    }
    let next_id = next.load(Ordering::Relaxed);
    if !sample_rows(addr, table, "k", next_id, 5, cfg) {
        correct = false;
    }
    Ok(Mixed {
        ops: total_ops,
        writes,
        next_id,
        elapsed: elapsed.max(1e-6),
        correct,
        latencies,
    })
}

fn run_w5(
    reference_addr: &str,
    candidate_addr: &str,
    table: &str,
    cfg: &PerfConfig,
) -> (WorkloadResult, Option<String>) {
    if let Err(e) = prepare_kv(reference_addr, table, cfg.rows_w5, cfg) {
        let note = format!("reference setup: {e}");
        return (harness_flawed("W5", &note), Some(note));
    }
    if let Err(e) = prepare_kv(candidate_addr, table, cfg.rows_w5, cfg) {
        return (flawed("W5", format!("candidate setup: {e}")), None);
    }
    // W5's own duration is most of the budget: rounds * 2 sides * the
    // 30 s window, plus setup and the final sample checks. The generic
    // 240 s cap would cut the fifth round.
    let needed = Duration::from_secs_f64(cfg.w5_seconds * 2.0 * cfg.rounds as f64 + 60.0);
    let deadline = Instant::now() + cfg.workload_deadline.max(needed);
    let mut ratios = Vec::new();
    let mut ref_tps = Vec::new();
    let mut ref99s = Vec::new();
    let mut cand_tps = Vec::new();
    let mut cand99s = Vec::new();
    let mut ref_writes = 0u64;
    let mut cand_writes = 0u64;
    let mut correct = true;
    let mut note = None;
    let mut ref_next = cfg.rows_w5 + 1;
    let mut cand_next = cfg.rows_w5 + 1;
    for round in 0..cfg.rounds {
        if Instant::now() >= deadline {
            correct = false;
            note = Some(format!("workload deadline after {round} rounds"));
            break;
        }
        let r = match w5_side(reference_addr, table, ref_next, cfg) {
            Ok(v) => v,
            Err(e) => {
                let note = format!("reference: {e}");
                return (harness_flawed("W5", &note), Some(note));
            }
        };
        if !r.correct {
            let note = "the reference mixed-workload check failed".to_string();
            return (harness_flawed("W5", &note), Some(note));
        }
        ref_next = r.next_id;
        ref_writes += r.writes;
        let c = match w5_side(candidate_addr, table, cand_next, cfg) {
            Ok(v) => v,
            Err(e) => {
                correct = false;
                note = Some(format!("candidate: {e}"));
                break;
            }
        };
        cand_next = c.next_id;
        cand_writes += c.writes;
        if !c.correct {
            correct = false;
            note = Some("the candidate mixed-workload check failed".into());
            break;
        }
        let mut rl = r.latencies.clone();
        let mut cl = c.latencies.clone();
        let ref99 = percentile(&mut rl, 0.99);
        let cand99 = percentile(&mut cl, 0.99);
        let ref_tp = r.ops as f64 / r.elapsed;
        let cand_tp = c.ops as f64 / c.elapsed;
        // W5 scores throughput; p99 is published but not scored (the
        // reference's own p99 moves with checkpoint timing).
        if ref_tp > 0.0 && cand_tp > 0.0 {
            ratios.push(ratio(ref_tp, cand_tp, true));
        } else {
            correct = false;
        }
        ref_tps.push(ref_tp);
        ref99s.push(ref99);
        cand_tps.push(cand_tp);
        cand99s.push(cand99);
    }
    let ref_tp = median(&mut ref_tps);
    let ref99 = median(&mut ref99s);
    let cand_tp = median(&mut cand_tps);
    let cand99 = median(&mut cand99s);
    let ratio = median(&mut ratios);
    let mut numbers = BTreeMap::new();
    numbers.insert("reference_ops_per_s".into(), round6(ref_tp));
    numbers.insert("reference_p99_ms".into(), round6(ref99));
    numbers.insert("candidate_ops_per_s".into(), round6(cand_tp));
    numbers.insert("candidate_p99_ms".into(), round6(cand99));
    numbers.insert("reference_writes".into(), ref_writes as f64);
    numbers.insert("candidate_writes".into(), cand_writes as f64);
    (
        result_with(
            "W5",
            ref_tp,
            cand_tp,
            ratio,
            correct && !ratios.is_empty(),
            numbers,
            note,
        ),
        None,
    )
}

// ---------------------------------------------------------------- F5

fn free_port() -> std::io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    l.local_addr().map(|a| a.port())
}

fn sql_extras(data: &Path, port: u16) -> crate::sandbox::docker::CandidateRunExtras {
    let mut extras = crate::sandbox::docker::CandidateRunExtras::default();
    extras
        .binds
        .push((data.display().to_string(), "/sdb-data".to_string()));
    extras.launch_args = vec![
        "--port".to_string(),
        port.to_string(),
        "--data".to_string(),
        "/sdb-data".to_string(),
    ];
    extras.env.push(("SDB_PORT".to_string(), port.to_string()));
    extras
        .env
        .push(("SDB_DATA_DIR".to_string(), "/sdb-data".to_string()));
    extras
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The default perf data root: `<runs>/.perf` (the runs disk, never /tmp).
pub fn perf_root() -> PathBuf {
    crate::report::archive::RunArchiver::resolve_runs_dir().join(".perf")
}

/// R49 F1: tmpfs and ramfs make `fsync` free, so W4 would measure memory.
/// The magic values are Linux's `TMPFS_MAGIC` and `RAMFS_MAGIC`.
fn is_memory_fs(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let f_type = st.f_type as u64;
    Ok(f_type == 0x0102_1994 || f_type == 0x8584_58f6)
}

/// Start the reference and `workspace` in the sandbox (2 CPUs, 2 GiB),
/// run `phase`'s workloads and stop both. The caller owns the bench lock.
/// This is the F5 entrypoint and the phase-end runner.
pub fn run_workspace(workspace: &Path, phase: u32, cfg: &PerfConfig) -> anyhow::Result<PerfScore> {
    run_workspace_at(workspace, phase, cfg, &perf_root())
}

/// [`run_workspace`] with an explicit data root, so tests can inject a
/// memory filesystem without touching the real runs directory.
pub fn run_workspace_at(
    workspace: &Path,
    phase: u32,
    cfg: &PerfConfig,
    root: &Path,
) -> anyhow::Result<PerfScore> {
    use crate::sandbox::docker::{RunGuard, SandboxManager};
    use anyhow::Context;

    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("workspace {}", workspace.display()))?;
    if !workspace.join("start.sh").exists() && !workspace.join("Dockerfile").exists() {
        // The workspace is the candidate; a missing entrypoint is its
        // failure, so the phase floors instead of losing its score.
        return Ok(PerfScore::floored(
            phase,
            format!(
                "workspace {} has no start.sh or Dockerfile",
                workspace.display()
            ),
        ));
    }
    std::fs::create_dir_all(root).context("create the perf data root")?;
    if is_memory_fs(root)? {
        return Ok(PerfScore::harness_fault(
            phase,
            format!(
                "perf data root {} is in memory (tmpfs/ramfs); W4 needs a disk",
                root.display()
            ),
        ));
    }
    let nonce = nonce();
    let run_root = root.join(format!("{}-{nonce:x}", std::process::id()));
    let ref_ws = run_root.join("reference");
    let ref_data = run_root.join("ref-data");
    let cand_data = run_root.join("cand-data");
    let _cleanup = Cleanup(run_root.clone());
    std::fs::create_dir_all(&ref_data).context("create the reference data dir")?;
    std::fs::create_dir_all(&cand_data).context("create the candidate data dir")?;
    // R49 F2: the flush counter is read from the data dir's backing disk.
    // An unreadable counter is a harness fault, not a candidate score.
    let ref_flush = match DeviceFlushCounter::for_dir(&ref_data) {
        Ok(c) => c,
        Err(e) => {
            return Ok(PerfScore::harness_fault(
                phase,
                format!("reference flush counter: {e}"),
            ))
        }
    };
    let cand_flush = match DeviceFlushCounter::for_dir(&cand_data) {
        Ok(c) => c,
        Err(e) => {
            return Ok(PerfScore::harness_fault(
                phase,
                format!("candidate flush counter: {e}"),
            ))
        }
    };
    let ref_port = free_port().context("reserve the reference port")?;
    let cand_port = free_port().context("reserve the candidate port")?;
    super::ref_server::write_reference(&ref_ws, ref_port).context("write the reference server")?;

    let ref_sb = SandboxManager::with_id(&format!("perf-ref-{}-{nonce:x}", std::process::id()));
    let cand_sb = SandboxManager::with_id(&format!("perf-cand-{}-{nonce:x}", std::process::id()));
    // The guards exist before the containers do, so a failed start is
    // reaped too.
    let ref_guard = RunGuard::new(ref_sb);
    let cand_guard = RunGuard::new(cand_sb);
    ref_guard
        .manager()
        .start_candidate_in_docker_with(&ref_ws, ref_port, &sql_extras(&ref_data, ref_port))
        .context("start the reference container")?;
    if let Err(e) = cand_guard.manager().start_candidate_in_docker_with(
        &workspace,
        cand_port,
        &sql_extras(&cand_data, cand_port),
    ) {
        return Ok(PerfScore::floored(
            phase,
            format!("candidate container: {e:#}"),
        ));
    }
    let ref_addr = format!("127.0.0.1:{ref_port}");
    let cand_addr = format!("127.0.0.1:{cand_port}");
    super::driver::wait_until_answering(&ref_addr, Duration::from_secs(30))
        .map_err(anyhow::Error::msg)
        .context("the reference did not answer")?;
    if let Err(e) = super::driver::wait_until_answering(&cand_addr, Duration::from_secs(30)) {
        return Ok(PerfScore::floored(
            phase,
            format!("the candidate did not answer: {e}"),
        ));
    }
    Ok(run_pair_with_flush(
        &ref_addr,
        &cand_addr,
        phase,
        cfg,
        Some(&ref_flush),
        Some(&cand_flush),
    ))
}

/// The human line(s) for the CLI and the campaign log.
pub fn render(score: &PerfScore) -> String {
    let mut out = match score.score {
        Some(value) => format!("perf score {value:.3} (1.0 = the SQLite reference)\n"),
        None => format!(
            "perf score: harness fault ({})\n",
            score.error.as_deref().unwrap_or("no score")
        ),
    };
    for w in &score.workloads {
        out.push_str(&format!(
            "  {} {:10} ratio {:6.3}  correct {}\n",
            w.id,
            w.metric,
            w.ratio,
            if w.correct { "yes" } else { "NO" }
        ));
        for (k, v) in &w.numbers {
            out.push_str(&format!("      {k}: {v:.6}\n"));
        }
    }
    if let Some(e) = &score.error {
        out.push_str(&format!("  error: {e}\n"));
    }
    out
}

/// The CLI's measurement half: a finished workspace is measured with every
/// workload (phase 4). Split out so the F5 live test exercises the same
/// path with a smaller config.
pub fn cli_with(workspace: &Path, cfg: &PerfConfig) -> anyhow::Result<PerfScore> {
    run_workspace(workspace, 4, cfg)
}

/// `subdollarbench perfscore --workspace <dir>` (F5).
pub fn cli(workspace: &str, json: bool) -> anyhow::Result<()> {
    let score = cli_with(Path::new(workspace), &PerfConfig::default())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&score)?);
    } else {
        print!("{}", render(&score));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;

    #[test]
    fn r45_f2_score_math() {
        // Hand: 2.0 latency, 0.5 throughput -> GM 1.0. Clip extremes.
        assert!((ratio(10.0, 5.0, false) - 2.0).abs() < 1e-12);
        assert!((ratio(5.0, 10.0, true) - 2.0).abs() < 1e-12);
        assert_eq!(ratio(10.0, 0.01, false), CLIP_MAX);
        assert!((ratio(0.01, 10.0, false) - CLIP_MIN).abs() < 1e-12);
        let gm = geometric_mean(&[2.0, 0.5, 1.0]).unwrap();
        assert!((gm - 1.0).abs() < 1e-12, "{gm}");
        let score = PerfScore::from_workloads(vec![
            WorkloadResult {
                id: "W1".into(),
                metric: "latency".into(),
                reference: 10.0,
                candidate: 5.0,
                ratio: 2.0,
                correct: true,
                numbers: BTreeMap::new(),
                note: None,
                harness_fault: false,
            },
            WorkloadResult {
                id: "W4".into(),
                metric: "throughput".into(),
                reference: 5.0,
                candidate: 10.0,
                ratio: 2.0,
                correct: true,
                numbers: BTreeMap::new(),
                note: None,
                harness_fault: false,
            },
        ]);
        assert!((score.score.expect("a score") - 2.0).abs() < 1e-12);
        let wrong = PerfScore::from_workloads(vec![WorkloadResult {
            id: "W1".into(),
            metric: "latency".into(),
            reference: 10.0,
            candidate: 0.1,
            ratio: 100.0,
            correct: false,
            numbers: BTreeMap::new(),
            note: None,
            harness_fault: false,
        }]);
        assert!((wrong.score.expect("a floor score") - WRONG_FLOOR).abs() < 1e-12);
    }

    #[test]
    fn r45_f2_phase_workloads() {
        assert_eq!(workloads_for_phase(1), ["W1", "W4"]);
        assert_eq!(workloads_for_phase(2), ["W1", "W3", "W4"]);
        assert_eq!(workloads_for_phase(3), ["W1", "W2", "W3", "W4", "W5"]);
        assert_eq!(phase_of_workload("W5"), Some(3));
    }

    /// A tiny in-process engine that stores `(id, v)` rows. It answers the
    /// W1/W4 shapes correctly unless `wrong`, and sleeps `delay` before
    /// every statement (the slow-candidate test).
    #[derive(Default)]
    struct FakeStore {
        tables: HashMap<String, Vec<(i64, String)>>,
    }

    fn fake_engine(delay: Duration, wrong: bool) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let store = Arc::new(Mutex::new(FakeStore::default()));
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let store = store.clone();
                std::thread::spawn(move || serve_fake(sock, store, delay, wrong));
            }
        });
        addr
    }

    fn answer_fake(store: &mut FakeStore, sql: &str, wrong: bool) -> String {
        if let Some(rest) = sql.strip_prefix("CREATE TABLE ") {
            let table = rest.split_whitespace().next().unwrap_or("").to_string();
            if store.tables.contains_key(&table) {
                return "ERR exists table already exists".to_string();
            }
            store.tables.insert(table, Vec::new());
            return "OK 0".to_string();
        }
        if let Some(rest) = sql.strip_prefix("INSERT INTO ") {
            let table = rest.split_whitespace().next().unwrap_or("").to_string();
            let vals = rest.split("VALUES").nth(1).unwrap_or("");
            let tuples: Vec<&str> = vals.split("),(").collect();
            let rows = store.tables.entry(table).or_default();
            for tuple in &tuples {
                let t = tuple.trim().trim_start_matches('(').trim_end_matches(')');
                let mut parts = t.splitn(2, ',');
                let id: i64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
                let v = parts
                    .next()
                    .unwrap_or("")
                    .split('\'')
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                rows.push((id, v));
            }
            return format!("OK {}", tuples.len());
        }
        if let Some(rest) = sql.strip_prefix("SELECT COUNT(*) FROM ") {
            let table = rest.trim();
            let n = store.tables.get(table).map(|r| r.len()).unwrap_or(0) as u64;
            let shown = if wrong { n + 1 } else { n };
            return format!("ROWS 1\nCOUNT(*)\n{shown}\nEND 1");
        }
        if let Some(rest) = sql.strip_prefix("SELECT v FROM ") {
            let table = rest.split_whitespace().next().unwrap_or("");
            if let Some(k) = rest
                .split("k = ")
                .nth(1)
                .or_else(|| rest.split("id = ").nth(1))
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<i64>().ok())
            {
                let v = store
                    .tables
                    .get(table)
                    .and_then(|rows| rows.iter().find(|(id, _)| *id == k))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| format!("r{k}"));
                let v = if wrong { "WRONG".to_string() } else { v };
                return format!("ROWS 1\nv\n{v}\nEND 1");
            }
            if let Some(a) = rest
                .split("k >= ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<i64>().ok())
            {
                let mut out = String::from("ROWS 1\nv\n");
                let n = if wrong { 1 } else { 20 };
                for j in 0..n {
                    out.push_str(&format!("r{}\n", if wrong { 0 } else { a + j }));
                }
                out.push_str(&format!("END {n}"));
                return out;
            }
            return "ROWS 1\nv\nWRONG\nEND 1".to_string();
        }
        if sql.contains("JOIN") {
            let mut out = String::from("ROWS 3\ng\tcount\tsum\n");
            // The unit tests use the small config (100 b rows); the real
            // reference is a separate process and never goes through here.
            let rows = if wrong {
                vec![(0i64, 1i64, 1i64)]
            } else {
                join_expected(100)
            };
            for (g, count, sum) in &rows {
                out.push_str(&format!("{g}\t{count}\t{sum}\n"));
            }
            out.push_str(&format!("END {}", rows.len()));
            return out;
        }
        "ERR syntax fake".to_string()
    }

    fn serve_fake(sock: TcpStream, store: Arc<Mutex<FakeStore>>, delay: Duration, wrong: bool) {
        sock.set_nodelay(true).unwrap();
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut writer = sock;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let sql = line.trim_end();
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let reply = {
                let mut s = store.lock().unwrap();
                answer_fake(&mut s, sql, wrong)
            };
            if writer
                .write_all(reply.as_bytes())
                .and_then(|()| writer.write_all(b"\n"))
                .is_err()
            {
                return;
            }
        }
    }

    /// The reference fake is fast; the candidate sleeps 2 ms per statement.
    /// W1 and W4 must see that as a score below 0.5.
    #[test]
    fn r45_f1_slow_candidate_scores_low() {
        let reference = fake_engine(Duration::ZERO, false);
        let candidate = fake_engine(Duration::from_millis(2), false);
        let cfg = PerfConfig::small_for_tests();
        let score = run_pair(&reference.to_string(), &candidate.to_string(), 1, &cfg);
        for id in ["W1", "W4"] {
            let w = score
                .workloads
                .iter()
                .find(|w| w.id == id)
                .unwrap_or_else(|| panic!("missing {id}"));
            assert!(
                w.ratio < 0.5,
                "{id} ratio {:.3} (correct {}): {:?}",
                w.ratio,
                w.correct,
                w.numbers
            );
        }
    }

    /// R49 F2: a deterministic flush source for the W4 tests. The first
    /// snapshot is 0; every later one is `after`.
    struct StepFlush {
        after: u64,
        calls: AtomicU64,
    }

    impl StepFlush {
        fn new(after: u64) -> Self {
            Self {
                after,
                calls: AtomicU64::new(0),
            }
        }
    }

    impl FlushCounter for StepFlush {
        fn flushes(&self) -> Result<u64, String> {
            Ok(if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                0
            } else {
                self.after
            })
        }
    }

    struct BrokenFlush;

    impl FlushCounter for BrokenFlush {
        fn flushes(&self) -> Result<u64, String> {
            Err("no counter on this host".to_string())
        }
    }

    /// R49 F2: a candidate with fewer flushes than 0.9 x its acknowledged
    /// 1-client commits floors W4; the reference passing keeps the phase
    /// scored.
    #[test]
    fn r49_f2_flush_check_floors_unsynced() {
        let reference = fake_engine(Duration::ZERO, false);
        let candidate = fake_engine(Duration::ZERO, false);
        let cfg = PerfConfig::small_for_tests();
        let ref_flush = StepFlush::new(10_000_000);
        let cand_flush = StepFlush::new(1);
        let score = run_pair_with_flush(
            &reference.to_string(),
            &candidate.to_string(),
            1,
            &cfg,
            Some(&ref_flush),
            Some(&cand_flush),
        );
        let w4 = score
            .workloads
            .iter()
            .find(|w| w.id == "W4")
            .expect("phase 1 runs W4");
        assert!(!w4.correct, "the unsynced candidate must not pass W4");
        let note = w4.note.as_deref().unwrap_or("");
        assert!(note.contains("not durable"), "{note}");
        assert!(
            score.score.is_some(),
            "a candidate floor keeps the phase scored"
        );
    }

    /// R49 F2: an unreadable counter is a harness fault, not a floor.
    #[test]
    fn r49_f2_unreadable_counter_is_harness_fault() {
        let reference = fake_engine(Duration::ZERO, false);
        let candidate = fake_engine(Duration::ZERO, false);
        let cfg = PerfConfig::small_for_tests();
        let score = run_pair_with_flush(
            &reference.to_string(),
            &candidate.to_string(),
            1,
            &cfg,
            Some(&BrokenFlush),
            Some(&StepFlush::new(10_000_000)),
        );
        assert!(
            score.score.is_none(),
            "an unreadable counter must give no score: {:?}",
            score.score
        );
        let err = score.error.as_deref().unwrap_or("");
        assert!(err.contains("flush"), "{err}");
    }

    /// R49 F1: a perf data root on tmpfs/ramfs is a harness fault (no
    /// score), because `fsync` would be free and W4 would measure memory.
    #[test]
    fn r49_f1_perf_data_not_in_memory() {
        let shm = std::path::PathBuf::from("/dev/shm").join(format!(
            "sdb-perf-mem-{}-{}",
            std::process::id(),
            nonce()
        ));
        let ws = std::env::temp_dir().join(format!("sdb-perf-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("start.sh"), "#!/bin/sh\n").unwrap();
        let score = run_workspace_at(&ws, 4, &PerfConfig::small_for_tests(), &shm)
            .expect("the memory check must not need Docker");
        assert!(
            score.score.is_none(),
            "a tmpfs data root must not score: {:?}",
            score.score
        );
        let err = score.error.as_deref().unwrap_or("");
        assert!(err.contains("memory"), "{err}");
        let _ = std::fs::remove_dir_all(&shm);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// R45-2: a reference that never answers is a harness failure, not a
    /// candidate floor. The block carries no score at all.
    #[test]
    fn r45_r2_reference_failure_is_not_a_score() {
        let candidate = fake_engine(Duration::ZERO, false);
        let cfg = PerfConfig::small_for_tests();
        let score = run_pair("127.0.0.1:1", &candidate.to_string(), 1, &cfg);
        assert!(
            score.score.is_none(),
            "a dead reference must not score the candidate: {:?}",
            score.score
        );
        let err = score.error.as_deref().unwrap_or("");
        assert!(err.contains("reference"), "reason: {err}");
        assert!(
            score.workloads.iter().any(|w| w.harness_fault),
            "the failed workload must say who failed: {:?}",
            score.workloads
        );
    }

    /// R45-2: a reference that answers wrong is our bug, not the
    /// candidate's; it leaves no score.
    #[test]
    fn r45_r2_wrong_reference_is_not_a_score() {
        let reference = fake_engine(Duration::ZERO, true);
        let candidate = fake_engine(Duration::ZERO, false);
        let cfg = PerfConfig::small_for_tests();
        let score = run_pair(&reference.to_string(), &candidate.to_string(), 3, &cfg);
        assert!(
            score.score.is_none(),
            "a wrong reference must not score the candidate: {:?}",
            score.score
        );
        let err = score.error.as_deref().unwrap_or("");
        assert!(err.contains("reference"), "reason: {err}");
    }

    /// A fast engine that answers one wrong value per workload scores the
    /// floor on every workload, whatever its speed.
    #[test]
    fn r45_f1_wrong_answer_is_floor() {
        let reference = fake_engine(Duration::ZERO, false);
        let candidate = fake_engine(Duration::ZERO, true);
        let cfg = PerfConfig::small_for_tests();
        let score = run_pair(&reference.to_string(), &candidate.to_string(), 3, &cfg);
        assert_eq!(score.workloads.len(), 5);
        for w in &score.workloads {
            assert!(!w.correct, "{} was graded correct", w.id);
        }
        let value = score
            .score
            .expect("a wrong candidate still gets a floored score");
        assert!((value - WRONG_FLOOR).abs() < 1e-12, "score {value:.3}");
    }
}
