//! Queue engine: dispatch truth, durability mirror, slots, breaker.
//!
//! Pure move from `server.rs` (2026-10-05 split): zero behavior change.
//! `server.rs` keeps handlers + guards; this module owns the machinery
//! handlers call into.

use super::server::{workdir_for_run, worker_count, ActiveRunInfo, AppState};
use crate::config::TaskType;
use crate::pipeline::{BenchmarkConfig, BenchmarkPipeline, PipelineLogger};
use crate::report::leaderboard::compute_pass_at_k;
use crate::report::{assess_stability, BenchmarkRunResult, LeaderboardManager, Stability, Store};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;
/// Read-through from the engine in GUI-proxy mode.
/// Returns Some(response) when proxied; None in single-process mode.
pub async fn proxy_read(state: &AppState, path: &str, query: &str) -> Option<Response> {
    if state.engine_url.is_empty() {
        return None;
    }
    let url = format!(
        "{}{}{}",
        state.engine_url.trim_end_matches('/'),
        path,
        query
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await.unwrap_or_default();
            let mut builder = axum::http::Response::builder().status(status);
            for key in ["x-console-total", "content-type"] {
                if let Some(v) = headers.get(key) {
                    builder = builder.header(key, v.clone());
                }
            }
            builder.body(axum::body::Body::from(bytes.to_vec())).ok()
        }
        Err(e) => Some(
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("engine unreachable at {url}: {e:#}"),
            )
                .into_response(),
        ),
    }
}
pub fn default_trials() -> u32 {
    9
}

/// Auto-extend cap. Must be >= `default_trials()` (9): the queue computes
/// `max_total = max_trials.max(total_trials)`, so a cap below the initial
/// batch makes `--extend-if-unstable` a silent no-op.
pub fn default_max_trials() -> u32 {
    9
}

pub fn default_max_turns() -> u32 {
    100
}

/// Screening width for the stop-on-pass capability protocol: the initial
/// batch runs at most this many trials, stopping at the first 100% pass.
/// Shared by the engine and foreground paths so both runners speak one
/// protocol.
pub fn screen_initial_trials(req_trials: u32) -> u32 {
    req_trials.clamp(1, crate::config::SCREEN_CAP_TRIALS)
}

/// Wave-level auto-extend trial budget, read from the environment per
/// decision (not cached: operators raise it mid-wave without a restart).
/// 0/absent (default) means manual-only — auto-extend never spends
/// unapproved trials.
pub fn auto_extend_envelope() -> u64 {
    std::env::var(crate::config::AUTO_EXTEND_BUDGET_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Narrow auto-extend trigger (pure): 0 passing with every completed trial
/// a harness fault is corrupted evidence, not a model verdict. Anything
/// else — mixed signals, model failures, empty cells — stays manual.
/// `planned <= cap` keeps it once-per-cell: after a fire, completed trials
/// exceed the cap and it never refires (a human-owned extend also blocks
/// refire, which is correct — a human took over).
pub fn should_auto_extend(
    passing: usize,
    infra: usize,
    completed: usize,
    planned: u32,
    cap: u32,
) -> bool {
    passing == 0
        && completed > 0
        && completed == planned as usize
        && planned <= cap
        && infra == completed
}

/// Re-exported from config.rs (single parse implementation shared with
/// the scoring merge); kept here so existing queue/main callers move
/// untouched.
pub use crate::config::trial_no_of;

/// Scored model verdicts: trials that carry a real measurement. Anything
/// else (agent_failed, infra_error, agent_killed, unknown) is a non-result
/// by portal doctrine and is never absorbed as evidence.
pub fn is_scored_status(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("completed") | Some("failed_tests") | Some("disqualified_cheat")
    )
}

/// Next trial number for an extension batch: max existing + 1, so numbering
/// never collides with absorbed orphans (len+1 collides once gaps exist).
/// Falls back to len+1 when no id parses (legacy safety, never worse).
pub fn next_trial_idx(subruns: &[BenchmarkRunResult]) -> u32 {
    let max_n = subruns.iter().filter_map(|s| trial_no_of(&s.id)).max();
    match max_n {
        Some(n) => n + 1,
        None if subruns.is_empty() => 1,
        None => subruns.len() as u32 + 1,
    }
}

/// Load stranded scored trials for a parent in one call: trials-table
/// read (skipped entirely when the store is unavailable) + selection.
/// `attempted` follows select_orphan_trials (None pre-batch, Some post).
pub fn load_orphans(
    parent_id: &str,
    model: &str,
    task: &str,
    known_ids: &std::collections::HashSet<String>,
    attempted: Option<std::ops::RangeInclusive<u32>>,
) -> OrphanReport {
    let empty = OrphanReport {
        absorb: Vec::new(),
        skipped_unscored: 0,
        skipped_attempted: 0,
        skipped_mismatch: 0,
    };
    let Some(store) = LeaderboardManager::open_store_if_exists() else {
        return empty;
    };
    select_orphans_from_store(&store, parent_id, model, task, known_ids, attempted)
}

/// Store-backed half of [`load_orphans`], split out so tests can exercise
/// the read + select path against an explicit temp DB (no process-global
/// SDB_BENCH_DB override racing other tests).
fn select_orphans_from_store(
    store: &Store,
    parent_id: &str,
    model: &str,
    task: &str,
    known_ids: &std::collections::HashSet<String>,
    attempted: Option<std::ops::RangeInclusive<u32>>,
) -> OrphanReport {
    select_orphan_trials(
        store.load_trials_for_parent(parent_id),
        model,
        task,
        known_ids,
        attempted,
    )
}

/// Orphan absorb report: scored stored trials missing from the merged set.
pub struct OrphanReport {
    pub absorb: Vec<BenchmarkRunResult>,
    pub skipped_unscored: usize,
    pub skipped_attempted: usize,
    pub skipped_mismatch: usize,
}

/// Select stranded scored trials for absorption. `stored` comes from the
/// trials table (authoritative per-trial log, independent of merging);
/// `known_ids` are trial ids already in the parent merge. A stored trial
/// is absorbed only when: scored status, same model+task (no cross-talk),
/// id unknown to the merge, and trial number NOT attempted by this batch
/// (fresh evidence wins number conflicts; the skip is logged, never
/// silent). Unparseable ids absorb on id-uniqueness alone.
/// `attempted = None` pre-batch (nothing reserved yet — every unknown
/// orphan surfaces); `Some(range)` post-batch as a race belt.
pub fn select_orphan_trials(
    stored: Vec<BenchmarkRunResult>,
    model: &str,
    task: &str,
    known_ids: &std::collections::HashSet<String>,
    attempted: Option<std::ops::RangeInclusive<u32>>,
) -> OrphanReport {
    let mut rep = OrphanReport {
        absorb: Vec::new(),
        skipped_unscored: 0,
        skipped_attempted: 0,
        skipped_mismatch: 0,
    };
    for t in stored {
        if !is_scored_status(t.status.as_deref()) {
            rep.skipped_unscored += 1;
            continue;
        }
        if known_ids.contains(&t.id) {
            continue;
        }
        if t.model != model || t.task != task {
            rep.skipped_mismatch += 1;
            continue;
        }
        match trial_no_of(&t.id) {
            Some(n) if attempted.as_ref().is_some_and(|r| r.contains(&n)) => {
                rep.skipped_attempted += 1;
            }
            _ => rep.absorb.push(t),
        }
    }
    // Deterministic order: absorb by trial number, unparseable ids last.
    rep.absorb
        .sort_by_key(|t| trial_no_of(&t.id).unwrap_or(u32::MAX));
    rep
}

/// Reserve `want` trials from the envelope (CAS loop: concurrent workers
/// can't overshoot). Grants partial (min(want, left)); None when spent.
/// Reserved upfront with no refunds — an attempted trial is spent whether
/// or not it produced evidence.
pub fn reserve_auto_extend(spent: &AtomicU64, budget: u64, want: u32) -> Option<u32> {
    use std::sync::atomic::Ordering;
    let mut cur = spent.load(Ordering::SeqCst);
    loop {
        let left = budget.saturating_sub(cur);
        if left == 0 {
            return None;
        }
        let grant = (want as u64).min(left) as u32;
        match spent.compare_exchange(cur, cur + grant as u64, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Some(grant),
            Err(actual) => cur = actual,
        }
    }
}

pub fn default_budget() -> f64 {
    1.00
}

/// Backstop only: cost (budget_usd) is the real bound, so trials get
/// room for 429 backoffs and slow reasoning. 0 disables the timeout.
/// NOTE: keep in sync with CLI defaults (config.rs) and the GUI input.
pub fn default_timeout_min() -> Option<u64> {
    Some(120)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueStatus {
    Pending,
    Running,
    Done,
    Cancelled,
}
/// What a queue entry carries. Runs own their full request; extends
/// reference a stored parent and resolve trial numbering at dispatch
/// (fresh subrun count), so chained extends never collide on trial dirs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueueKind {
    Run(RunRequest),
    Extend(ExtendRequest),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendRequest {
    pub parent_run_id: String,
    pub trials: u32,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub timeout_min: Option<u64>,
    #[serde(default)]
    pub eval_only: Option<bool>,
    /// API key from the triggering extend request; falls back to
    /// `OPENROUTER_API_KEY` env at dispatch. Queued extends are
    /// deserialized with `#[serde(default)]` so pre-key payloads keep working.
    #[serde(default)]
    pub api_key: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedRun {
    pub id: String,
    pub enqueued_at: String,
    pub status: QueueStatus,
    pub kind: QueueKind,
    /// Last state note: refusal/finish reason, always set on terminal
    /// transitions and on parked refusals. Surfaced in the UI queue rows
    /// and the integrity report — a Done with no evidence and no reason
    /// is the exact silence this field exists to prevent.
    /// Memory-only (the DB mirror stores status timestamps, not the doc).
    #[serde(default)]
    pub note: Option<String>,
    /// Kick-time refusal count. Refusals park as Pending with backoff
    /// instead of Done; past MAX_REFUSALS the cell goes Done as
    /// `refused-final` (loud, still a model non-result).
    #[serde(default)]
    pub refusals: u32,
    /// Earliest retry time (RFC3339) after a parked refusal. Kick and pump
    /// skip items still inside their backoff window.
    #[serde(default)]
    pub not_before: Option<String>,
}

/// Refusals before a cell is terminally refused instead of re-parked.
pub const MAX_REFUSALS: u32 = 3;

/// True when a Pending item may be dispatched now (backoff elapsed).
pub fn dispatch_ready(item: &QueuedRun, now: &str) -> bool {
    match item.not_before.as_deref() {
        Some(nb) => now >= nb,
        None => true,
    }
}
impl QueuedRun {
    /// Back-compat accessor: pre-extend queue payloads deserialized with a
    /// bare `request` field are not expected (queue is in-memory only), but
    /// tests and the GUI only need the run request when present.
    pub fn run_request(&self) -> Option<&RunRequest> {
        match &self.kind {
            QueueKind::Run(r) => Some(r),
            QueueKind::Extend(_) => None,
        }
    }

    /// Copy for serving over HTTP with the caller's API key removed.
    ///
    /// The in-memory entry keeps its key for dispatch; the listing must not
    /// hand it back. `GET /api/run/queue` is unauthenticated like every other
    /// route here, so serializing `QueuedRun` directly returned any key a
    /// caller had POSTed, in plaintext, to whoever asked.
    pub fn redacted(&self) -> QueuedRun {
        let mut copy = self.clone();
        match &mut copy.kind {
            QueueKind::Run(r) => r.api_key = None,
            QueueKind::Extend(e) => e.api_key = None,
        }
        copy
    }
}
/// Last-resort in-memory Store when even the temp fallback cannot open
/// (e.g. read-only filesystem): creates a temp-dir DB that is never read
/// back. Queue dispatch never blocks on persistence.
pub fn tempfile_mem_store() -> Store {
    let fallback = std::env::temp_dir().join(format!(
        "sdb-queue-mem-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    Store::open(&fallback).expect("temp queue store must open")
}
/// Current time as RFC3339; used for queue dispatch/completion stamps.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}
/// Best-effort mirror of an in-memory queue transition into bench.db.
/// Memory stays dispatch truth; DB failures are logged, never fatal.
pub fn db_mirror(state: &AppState, what: &str, f: impl FnOnce(&Store)) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&state.store))) {
        Ok(()) => {}
        Err(_) => eprintln!("[QUEUE-DB] {what}: store panicked (memory still authoritative)"),
    }
}
/// Mirror helper that logs DB-side errors without touching dispatch.
pub fn db_try(what: &str, r: anyhow::Result<()>) {
    if let Err(e) = r {
        eprintln!("[QUEUE-DB] {what}: {e:#}");
    }
}
/// Mark a queue item finished in both memory and the DB mirror.
/// Broadcast a status/console line AND mirror it into the in-memory console
/// buffer so late SSE joiners (replay) and /api/console readers see the same
/// lifecycle tail (DONE/STOP/STATUS/[EXTEND]/[MULTI-TRIAL]) that live
/// subscribers got over the channel.
pub fn send_status(
    tx: &broadcast::Sender<String>,
    log_buf: &Arc<RwLock<Vec<String>>>,
    line: String,
) {
    push_console_line(log_buf, line.clone());
    let _ = tx.send(line);
}
pub fn mark_queue_done(state: &AppState, queue_id: &str, reason: Option<&str>) {
    {
        let mut q = state.queue.lock().unwrap();
        if let Some(item) = q.iter_mut().find(|i| i.id == queue_id) {
            item.status = QueueStatus::Done;
            if let Some(r) = reason {
                item.note = Some(r.to_string());
            }
        }
    }
    let now = now_rfc3339();
    let qid = queue_id.to_string();
    db_mirror(state, "mark done", move |store| {
        db_try(
            &format!("mark {qid} Done"),
            store.update_queue_status(&qid, "Done", None, Some(&now)),
        );
    });
}
/// Park a refused cell back to Pending with reason + backoff instead of
/// Done. Returns true when the cell went terminal (`refused-final`, past
/// the cap). Every park logs loudly: refusals must be visible, never
/// silent, and a parked cell keeps its queue position (no reorder).
pub fn park_refused(state: &AppState, queue_id: &str, reason: &str) -> bool {
    let now = now_rfc3339();
    let (final_, note, wait_s) = {
        let mut q = state.queue.lock().unwrap();
        match q.iter_mut().find(|i| i.id == queue_id) {
            None => return true,
            Some(item) => {
                item.refusals += 1;
                if item.refusals > MAX_REFUSALS {
                    item.status = QueueStatus::Done;
                    let note = format!("refused-final ({} refusals): {}", item.refusals, reason);
                    item.note = Some(note.clone());
                    item.not_before = None;
                    (true, note, 0)
                } else {
                    item.status = QueueStatus::Pending;
                    let note = format!(
                        "refused (attempt {}/{}): {}",
                        item.refusals, MAX_REFUSALS, reason
                    );
                    item.note = Some(note.clone());
                    (false, note, 60 * item.refusals as u64)
                }
            }
        }
    };
    if final_ {
        let qid = queue_id.to_string();
        db_mirror(state, "mark refused-final", move |store| {
            db_try(
                &format!("mark {qid} Done"),
                store.update_queue_status(&qid, "Done", None, Some(&now)),
            );
        });
        send_status(
            &state.log_sender,
            &state.log_buffer,
            format!("[QUEUE-REFUSED] {queue_id}: {note}; no more retries."),
        );
        return true;
    }
    // Backoff: not_before = now + 60s*refusals. RFC3339 string compare
    // orders chronologically within one format, same trick as the publish
    // freeze gate (portal.rs).
    let not_before = chrono::Utc::now() + chrono::Duration::seconds(wait_s as i64);
    {
        let mut q = state.queue.lock().unwrap();
        if let Some(item) = q.iter_mut().find(|i| i.id == queue_id) {
            item.not_before = Some(not_before.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
        }
    }
    send_status(
        &state.log_sender,
        &state.log_buffer,
        format!("[QUEUE-REFUSED] {queue_id}: {note}; retrying in {wait_s}s."),
    );
    pump_queue(state);
    false
}
/// Replay durable queue rows into memory at startup. Running rows reset to
/// Pending (the previous process died mid-dispatch); finished history is
/// not replayed. Returns the replayed items in enqueue order.
pub fn replay_queue(store: &Store) -> Vec<QueuedRun> {
    let mut items = store.load_queue(false);
    for item in items.iter_mut() {
        if item.status == QueueStatus::Running {
            item.status = QueueStatus::Pending;
        }
    }
    items
}
/// Queue truth sweep: a `Running` item with no live worker slot, no
/// worker_runs entry and no worker handle is a ghost — its body died without
/// finishing (restart residue replay misses, or a mid-process abort that
/// skipped the normal exit). Ghosts stay `Running` forever: slots leak,
/// `is_running` sticks true, and the next wave waits on a dead cell (live
/// q118: Running, workers {}, current None — engine idle, queue wedged).
/// Queue items and worker slots are 1:1 (pump/kick claim exactly one slot
/// per Running item), so an empty engine cannot own a Running item.
/// Returns the number of ghosts reaped. Never touches live cells.
pub fn reconcile_queue(state: &AppState) -> usize {
    let engine_busy = !state.worker_runs.read().unwrap().is_empty()
        || !state.worker_handles.lock().unwrap().is_empty()
        || !state.running_slots.lock().unwrap().is_empty();
    if engine_busy {
        return 0;
    }
    let ghosts: Vec<String> = state
        .queue
        .lock()
        .unwrap()
        .iter()
        .filter(|i| i.status == QueueStatus::Running)
        .map(|i| i.id.clone())
        .collect();
    if ghosts.is_empty() {
        return 0;
    }
    // Memory first: flip every ghost while holding the queue lock once.
    state
        .queue
        .lock()
        .unwrap()
        .iter_mut()
        .filter(|i| ghosts.contains(&i.id))
        .for_each(|i| {
            i.status = QueueStatus::Cancelled;
            i.note = Some("ghost: Running with no live worker; reaped".to_string());
        });
    // DB mirror + console outside the lock: rusqlite is synchronous, and a
    // slow/blocked DB must never stall a status read (live: every API hung
    // with empty bodies while two trial bodies held the queue path busy).
    // Memory is dispatch truth; the DB is a best-effort mirror.
    let now = now_rfc3339();
    for id in &ghosts {
        let qid = id.clone();
        let now_c = now.clone();
        db_mirror(state, "reap ghost", move |store| {
            db_try(
                &format!("reap ghost {qid}"),
                store.update_queue_status(&qid, "Cancelled", None, Some(&now_c)),
            );
        });
        send_status(
            &state.log_sender,
            &state.log_buffer,
            format!("[QUEUE] reaped ghost {id}: Running with no live worker; marked Cancelled."),
        );
    }
    ghosts.len()
}
#[cfg(test)]
pub fn test_queue_item(id: &str) -> QueuedRun {
    QueuedRun {
        id: id.to_string(),
        enqueued_at: "2026-09-25T00:00:00Z".to_string(),
        status: QueueStatus::Pending,
        note: None,
        refusals: 0,
        not_before: None,
        kind: QueueKind::Run(RunRequest {
            model: "test-model".to_string(),
            task: "wal".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 5,
            eval_only: true,
            effort: Some("low".to_string()),
            harness: None,
            timeout_min: Some(1),
            trials: 1,
            extend_if_unstable: false,
            max_trials: 9,
            save_results: None,
            review_model: None,
            stop_on_pass: false,
            protocol: None,
            feedback_rounds: None,
        }),
    }
}
/// Next queue sequence start so new ids never collide with finished history.
/// Scans ALL queue rows (including Done/Cancelled), so a restart on a clean
/// queue never reissues `q1` over prior runs. Parses the numeric suffix of
/// `q<N>` ids; empty history starts at 0.
pub fn queue_seq_start(store: &Store) -> u64 {
    store
        .load_queue(true)
        .iter()
        .filter_map(|i| i.id.strip_prefix('q'))
        .filter_map(|n| n.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    pub model: String,
    pub task: String,
    pub api_key: Option<String>,
    #[serde(default = "default_budget")]
    pub budget_usd: f64,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    pub eval_only: bool,
    #[serde(default)]
    pub effort: Option<String>,
    /// Agent harness ("omp" | "opencode"); None/absent = omp (back-compat).
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default = "default_timeout_min")]
    pub timeout_min: Option<u64>,
    #[serde(default = "default_trials")]
    pub trials: u32,
    #[serde(default)]
    pub extend_if_unstable: bool,
    #[serde(default = "default_max_trials")]
    pub max_trials: u32,
    #[serde(default)]
    pub save_results: Option<bool>,
    #[serde(default)]
    pub review_model: Option<String>,
    #[serde(default)]
    pub stop_on_pass: bool,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub feedback_rounds: Option<u32>,
}
/// Forward a mutation to the engine in GUI-proxy mode.
/// Returns Some(response) when proxying applied; None in single-process mode.
pub async fn proxy_mutation(
    state: &AppState,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Option<Response> {
    if state.engine_url.is_empty() {
        return None;
    }
    let url = format!("{}{}", state.engine_url.trim_end_matches('/'), path);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default();
    let mut rb = match method {
        "DELETE" => client.delete(&url),
        _ => client.post(&url),
    };
    // Server-to-server trust: browser sessions live in the GUI process
    // memory only, so a forwarded cookie would never validate on the
    // engine. The proxy authenticates with the shared SDB_UI_TOKEN both
    // processes read from the same .env; the engine's bearer gate accepts
    // it. Token-unset setups pass the engine gate anyway (CSRF-only).
    if let Ok(tok) = std::env::var("SDB_UI_TOKEN") {
        let tok = tok.trim().to_string();
        if !tok.is_empty() {
            rb = rb.header("Authorization", format!("Bearer {tok}"));
        }
    }
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    match rb.send().await {
        Ok(resp) => {
            let status = resp.status();
            let bytes = resp.bytes().await.unwrap_or_default();
            Some((status, bytes.to_vec()).into_response())
        }
        Err(e) => Some(
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("engine unreachable at {url}: {e:#}"),
            )
                .into_response(),
        ),
    }
}
/// Max lines retained in the in-memory live console. Bounds per-fetch
/// payloads, SSE replay on connect, and steady-state memory while a run
/// streams. Full history stays in runs/<id>/console.log on disk.
pub const MAX_CONSOLE_LINES: usize = 3000;
/// Per-worker console cap: same bound as the shared buffer, each.
pub const MAX_WORKER_CONSOLE_LINES: usize = 3000;
/// Mirror one line into a worker's private buffer (bounded). Backs the
/// per-worker console tabs; the shared buffer stays the multiplexed all-view.
pub fn push_worker_line(
    bufs: &Arc<RwLock<std::collections::HashMap<u8, Vec<String>>>>,
    worker: u8,
    line: String,
) {
    let mut m = bufs.write().unwrap();
    let v = m.entry(worker).or_default();
    v.push(line);
    let over = v.len().saturating_sub(MAX_WORKER_CONSOLE_LINES);
    if over > 0 {
        v.drain(..over);
    }
}
pub fn push_console_line(log_buf: &Arc<RwLock<Vec<String>>>, line: String) {
    let mut buf = log_buf.write().unwrap();
    buf.push(line);
    let overflow = buf.len().saturating_sub(MAX_CONSOLE_LINES);
    if overflow > 0 {
        buf.drain(..overflow);
    }
}
pub struct WebLogger {
    pub(crate) tx: broadcast::Sender<String>,
    pub(crate) log_buf: Arc<RwLock<Vec<String>>>,
    pub(crate) current_run: Arc<RwLock<Option<ActiveRunInfo>>>,
    pub(crate) trial_idx: u32,
    pub(crate) total_trials: u32,
    pub(crate) worker: u8,
    pub(crate) worker_bufs: Arc<RwLock<std::collections::HashMap<u8, Vec<String>>>>,
    pub(crate) worker_runs: Arc<RwLock<std::collections::HashMap<u8, ActiveRunInfo>>>,
}
impl PipelineLogger for WebLogger {
    fn log(&self, msg: &str) {
        let ts = Utc::now().format("%H:%M:%S").to_string();
        for line in msg.lines() {
            let clean = line.trim_end();
            let mut line_to_log = clean.to_string();

            if clean.starts_with("[LIVE] Turn ") && self.total_trials > 1 {
                line_to_log = format!(
                    "[LIVE] Subrun {}/{} | {}",
                    self.trial_idx,
                    self.total_trials,
                    &clean[7..]
                );
            } else if clean.starts_with("[TELEMETRY] ") && !clean.contains("subrun=") {
                line_to_log = format!(
                    "{} subrun={} total_subruns={}",
                    clean, self.trial_idx, self.total_trials
                );
            }

            let formatted = format!("[{}] {}", ts, line_to_log);
            push_console_line(&self.log_buf, formatted.clone());
            push_worker_line(&self.worker_bufs, self.worker, formatted.clone());
            let _ = self.tx.send(formatted);
        }

        if msg.contains("[TELEMETRY]") {
            if let Some(telemetry_part) = msg.split("[TELEMETRY]").nth(1) {
                let mut turns = None;
                let mut max_turns = None;
                let mut spent = None;
                let mut tokens = None;
                let mut subrun = None;
                let mut total_subruns = None;

                for token in telemetry_part.split_whitespace() {
                    if let Some((k, v)) = token.split_once("=") {
                        match k {
                            "turn" => turns = v.parse::<u32>().ok(),
                            "max_turns" => max_turns = v.parse::<u32>().ok(),
                            "spent" => spent = v.parse::<f64>().ok(),
                            "tokens" => tokens = v.parse::<u64>().ok(),
                            "subrun" => subrun = v.parse::<u32>().ok(),
                            "total_subruns" => total_subruns = v.parse::<u32>().ok(),
                            _ => {}
                        }
                    }
                }

                // Shared mirror (telemetry bar) AND the per-worker card
                // source: cards froze at trial-start values because only the
                // mirror was ever updated.
                if let Ok(mut run_lock) = self.current_run.write() {
                    if let Some(ref mut info) = *run_lock {
                        if let Some(t) = turns {
                            info.turns = t;
                        }
                        if let Some(mt) = max_turns {
                            info.max_turns = mt;
                        }
                        if let Some(s) = spent {
                            info.cost_usd = s;
                        }
                        if let Some(tok) = tokens {
                            info.tokens = tok;
                        }
                        info.subrun = subrun.or(Some(self.trial_idx));
                        info.total_subruns = total_subruns.or(Some(self.total_trials));
                    }
                }
                if let Ok(mut runs) = self.worker_runs.write() {
                    if let Some(ref mut info) = runs.get_mut(&self.worker) {
                        if let Some(t) = turns {
                            info.turns = t;
                        }
                        if let Some(mt) = max_turns {
                            info.max_turns = mt;
                        }
                        if let Some(s) = spent {
                            info.cost_usd = s;
                        }
                        if let Some(tok) = tokens {
                            info.tokens = tok;
                        }
                        info.subrun = subrun.or(Some(self.trial_idx));
                        info.total_subruns = total_subruns.or(Some(self.total_trials));
                    }
                }
            }
        }
    }
}
pub struct ExtBatchSpec {
    pub(crate) model: String,
    pub(crate) task: TaskType,
    pub(crate) effort: String,
    pub(crate) harness: String,
    pub(crate) max_turns: u32,
    pub(crate) budget_usd: f64,
    pub(crate) timeout_min: u64,
    pub(crate) api_key: Option<String>,
    pub(crate) eval_only: bool,
    pub(crate) save_results: bool,
    pub(crate) review_model: Option<String>,
    pub(crate) parent_run_id: String,
    pub(crate) start_idx: u32,
    pub(crate) count: u32,
    pub(crate) display_total: u32,
    pub(crate) feedback_rounds: u32,
}
pub struct ExtBatchOut {
    pub(crate) results: Vec<BenchmarkRunResult>,
    pub(crate) passing: usize,
    /// Trials that errored (non-cancel, non-billing): attempted but
    /// unscored. Lets callers report "N errored" honestly instead of
    /// blanket "all failed".
    pub(crate) errors: u32,
    pub(crate) drift: Option<bool>,
    /// Provider billing trip during extensions: callers pause the queue.
    pub(crate) billing_tripped: bool,
}
/// Run one extension batch (fresh batch reference, suffix-numbered trial dirs).
/// Shared by auto-extend and the extend endpoint; never touches the runner slot.
#[allow(clippy::too_many_arguments)]
pub async fn run_extension_batch(
    tx: &broadcast::Sender<String>,
    log_buf: &Arc<RwLock<Vec<String>>>,
    current_run: &Arc<RwLock<Option<ActiveRunInfo>>>,
    cancel: &Arc<AtomicBool>,
    _batch_log: &(dyn Fn(&str) + Sync),
    spec: ExtBatchSpec,
    worker: u8,
    worker_bufs: &Arc<RwLock<std::collections::HashMap<u8, Vec<String>>>>,
    worker_runs: &Arc<RwLock<std::collections::HashMap<u8, ActiveRunInfo>>>,
) -> ExtBatchOut {
    let mut results = Vec::new();
    let mut passing = 0usize;
    let mut errors = 0u32;
    let mut billing_tripped = false;
    for i in 0..spec.count {
        if cancel.load(Ordering::SeqCst) {
            send_status(
                tx,
                log_buf,
                "[STOP] Extension cancelled by user.".to_string(),
            );
            break;
        }
        let trial_idx = spec.start_idx + i;
        let run_id = format!("{}_trial{}", spec.parent_run_id, trial_idx);
        *current_run.write().unwrap() = Some(ActiveRunInfo {
            run_id: run_id.clone(),
            model: spec.model.clone(),
            task: spec.task.to_string(),
            effort: spec.effort.clone(),
            started_at: Utc::now().to_rfc3339(),
            turns: 0,
            max_turns: spec.max_turns,
            cost_usd: 0.0,
            tokens: 0,
            duration_secs: None,
            subrun: Some(trial_idx),
            total_subruns: Some(spec.display_total),
            budget_usd: spec.budget_usd,
            timeout_min: spec.timeout_min,
            trials: spec.display_total,
            eval_only: spec.eval_only,
            extend_if_unstable: false,
            max_trials: spec.display_total,
            review_model: spec.review_model.clone(),
        });
        let logger = Arc::new(WebLogger {
            tx: tx.clone(),
            log_buf: log_buf.clone(),
            current_run: current_run.clone(),
            trial_idx,
            total_trials: spec.display_total,
            worker,
            worker_bufs: worker_bufs.clone(),
            worker_runs: worker_runs.clone(),
        });
        logger.log(&format!(
            "[SUBRUN] ━━━ Starting Extension Subrun {} (Model: {}, Task: {}) ━━━",
            trial_idx, spec.model, spec.task
        ));
        let pipe_config = BenchmarkConfig {
            model: spec.model.clone(),
            task: spec.task,
            effort: spec.effort.clone(),
            harness: crate::config::Harness::parse(&spec.harness),
            max_turns: spec.max_turns,
            budget_usd: spec.budget_usd,
            timeout_min: spec.timeout_min,
            api_key: spec.api_key.clone(),
            workdir: workdir_for_run(&run_id),
            eval_only: spec.eval_only,
            run_id: Some(run_id.clone()),
            save_results: spec.save_results,
            review_model: spec.review_model.clone(),
            worker,
            feedback_rounds: spec.feedback_rounds,
            campaign_stage: None,
            baseline_rps: None,
        };
        match BenchmarkPipeline::execute(pipe_config, logger.clone(), Some(Arc::clone(cancel)))
            .await
        {
            Ok(output) => {
                if output.benchmark_result.pass_rate == 100.0 {
                    passing += 1;
                }
                results.push(output.benchmark_result);
            }
            Err(e) => {
                if cancel.load(Ordering::SeqCst) {
                    logger.log("[PIPELINE] Run cancelled by user.");
                    break;
                }
                let msg = format!("{e:#}");
                if msg.to_lowercase().contains("billing:")
                    || crate::sandbox::omp::classify_billing_error(&msg).is_some()
                {
                    logger.log(&format!(
                        "[BILLING] Extension subrun {} hit provider billing error: {}; trial discarded.",
                        trial_idx, msg
                    ));
                    billing_tripped = true;
                    break;
                }
                logger.log(&format!(
                    "[SUBRUN ERROR] Extension subrun {} failed: {}",
                    trial_idx, e
                ));
                errors += 1;
                break;
            }
        }
    }
    ExtBatchOut {
        results,
        passing,
        errors,
        drift: None,
        billing_tripped,
    }
}
/// Pop the next pending queue item and spawn it. Returns true when an item
/// was spawned. Callers must own the runner slot (chain holder or kick CAS
/// winner). Extends re-validate at dispatch: a parent stabilized by an
/// earlier item in the same chain is marked Done with a log line, never run.
/// Claim a free worker slot (0..worker_count). None when all busy.
pub fn claim_slot(state: &AppState) -> Option<u8> {
    let max = worker_count() as u8;
    let mut slots = state.running_slots.lock().unwrap();
    for w in 0..max {
        if !slots.contains(&w) {
            slots.insert(w);
            state.is_running.store(true, Ordering::SeqCst);
            // Fresh worker = fresh console view (stale lines from the
            // previous occupant would read as the new run's output).
            state.worker_buffers.write().unwrap().remove(&w);
            return Some(w);
        }
    }
    None
}
/// Release a worker slot; clears legacy is_running when all idle.
pub fn release_slot(state: &AppState, worker: u8) {
    let empty = {
        let mut slots = state.running_slots.lock().unwrap();
        slots.remove(&worker);
        slots.is_empty()
    };
    state.worker_runs.write().unwrap().remove(&worker);
    state.worker_handles.lock().unwrap().remove(&worker);
    if empty {
        state.is_running.store(false, Ordering::SeqCst);
    }
}
pub fn pump_queue(state: &AppState) -> bool {
    if state.queue_paused.load(Ordering::SeqCst) {
        return false;
    }
    // Work-stealing: claim a free slot first; no slot = all workers busy.
    let worker = match claim_slot(state) {
        Some(w) => w,
        None => return false,
    };
    let next = {
        let mut q = state.queue.lock().unwrap();
        let now_sel = now_rfc3339();
        // A campaign owns the candidate ports: hold SQL jobs while its
        // lock is live (round-7 finding 1).
        let campaign_live = crate::campaign::live_lock_holder().is_some();
        let running_free = state
            .worker_runs
            .read()
            .unwrap()
            .values()
            .filter(|w| w.model.ends_with(":free"))
            .count();
        q.iter_mut()
            .find(|i| {
                if i.status != QueueStatus::Pending || !dispatch_ready(i, &now_sel) {
                    return false;
                }
                if let QueueKind::Run(ref r) = i.kind {
                    if r.task == "sql" && campaign_live {
                        return false;
                    }
                    if r.model.ends_with(":free") && running_free >= 2 {
                        return false;
                    }
                }
                true
            })
            .map(|item| {
                item.status = QueueStatus::Running;
                (item.kind.clone(), item.id.clone())
            })
    };
    if let Some((_, ref id)) = next {
        let now = now_rfc3339();
        let id = id.clone();
        db_mirror(state, "mark running", move |store| {
            db_try(
                &format!("mark {id} Running"),
                store.update_queue_status(&id, "Running", Some(&now), None),
            );
        });
    }
    match next {
        Some((QueueKind::Run(req), id)) => {
            send_status(
                &state.log_sender,
                &state.log_buffer,
                format!("[QUEUE] Starting queued run {id} on worker {worker}..."),
            );
            spawn_run_body(state, req, Some(id), worker);
            true
        }
        Some((QueueKind::Extend(ext), id)) => {
            spawn_extend_body(state, ext, id, worker);
            true
        }
        None => {
            release_slot(state, worker);
            false
        }
    }
}
/// Spawn an extend with fresh dispatch-time validation. The runner slot is
/// already held by the caller; this fn never releases it — the spawned task
/// hands off via pump_queue or stores false when idle, like run bodies.
pub fn spawn_extend_body(state: &AppState, ext: ExtendRequest, queue_id: String, worker: u8) {
    let state_clone = state.clone();
    let handle = tokio::spawn(execute_extend_body(state_clone, ext, queue_id, worker));
    state.worker_handles.lock().unwrap().insert(worker, handle);
}
/// Queued-extend worker: re-read the parent under the held slot, skip when
/// it no longer needs trials, else run the extension batch and re-merge.
/// Trial numbering derives from the fresh subrun count, so chained extends
/// on one parent never collide on trial dirs.
pub async fn execute_extend_body(
    state: AppState,
    ext: ExtendRequest,
    queue_id: String,
    worker: u8,
) {
    let tx = state.log_sender.clone();
    let log_buf = state.log_buffer.clone();
    let finish_item = |state: &AppState, note: &str| {
        mark_queue_done(state, &queue_id, Some(note));
        send_status(&tx, &log_buf, note.to_string());
    };
    let parent_path = crate::config::get_repo_root()
        .join("results")
        .join(format!("{}.json", ext.parent_run_id));
    let data: BenchmarkRunResult = match std::fs::read_to_string(&parent_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
    {
        Some(d) => d,
        None => {
            finish_item(
                &state,
                &format!(
                    "[EXTEND-QUEUE] {queue_id}: parent '{}' gone; skipping.",
                    ext.parent_run_id
                ),
            );
            release_slot(&state, worker);
            if !pump_queue(&state) {
                send_status(&tx, &log_buf, "[DONE]".to_string());
            }
            return;
        }
    };
    if data.subruns.len() < 2 {
        finish_item(
            &state,
            &format!(
                "[EXTEND-QUEUE] {queue_id}: '{}' is not multi-trial; skipping.",
                ext.parent_run_id
            ),
        );
        // Same exit discipline as parent-gone above: release_slot frees the
        // worker and clears is_running only when the last slot frees. The
        // old task_handle/is_running writes leaked the slot AND cleared the
        // global flag under live siblings (legacy single-runner residue).
        release_slot(&state, worker);
        if !pump_queue(&state) {
            send_status(&tx, &log_buf, "[DONE]".to_string());
        }
        return;
    }
    let (stab, reason) = assess_stability(&data.subruns);
    if stab == Stability::Stable {
        finish_item(
            &state,
            &format!(
                "[EXTEND-QUEUE] {queue_id}: '{}' already stable ({reason}); skipping.",
                ext.parent_run_id
            ),
        );
        release_slot(&state, worker);
        if !pump_queue(&state) {
            send_status(&tx, &log_buf, "[DONE]".to_string());
        }
        return;
    }
    let task_type = match TaskType::parse(&data.task) {
        Ok(t) => t,
        Err(e) => {
            // Covers unknown tasks (incl. the removed `dns`), so a grid file
            // still listing one cannot spend money on it through the engine.
            finish_item(
                &state,
                &format!("[EXTEND-QUEUE] {queue_id}: {e}; skipping."),
            );
            release_slot(&state, worker);
            if !pump_queue(&state) {
                send_status(&tx, &log_buf, "[DONE]".to_string());
            }
            return;
        }
    };
    let count = ext.trials.max(1);
    // Pre-absorb BEFORE numbering: stranded scored trials rejoin the base
    // first, so max-N+1 numbering skips their numbers (no workdir reuse)
    // and the merge below sees every trial whether or not this batch ran.
    let mut base_subruns = data.subruns.clone();
    {
        let known: std::collections::HashSet<String> =
            base_subruns.iter().map(|s| s.id.clone()).collect();
        let pre = load_orphans(&ext.parent_run_id, &data.model, &data.task, &known, None);
        if !pre.absorb.is_empty() {
            send_status(
                &tx,
                &log_buf,
                format!(
                    "[EXTEND-QUEUE] {queue_id}: pre-absorbed {} orphan scored trial(s) into the base (skipped: {} unscored, {} mismatch).",
                    pre.absorb.len(),
                    pre.skipped_unscored,
                    pre.skipped_mismatch
                ),
            );
            base_subruns.extend(pre.absorb);
            base_subruns.sort_by_key(|r| trial_no_of(&r.id).unwrap_or(u32::MAX));
        }
    }
    // max-N+1, not len+1: numbering gaps make len+1 collide with an
    // existing trial dir (workdir reuse = data loss + confusion). Falls
    // back to len+1 only when no id parses.
    let start_idx = next_trial_idx(&base_subruns);
    let total = data.subruns.len() as u32 + count;
    let spec = ExtBatchSpec {
        model: data.model.clone(),
        task: task_type,
        effort: data.effort.clone().unwrap_or_else(|| "high".to_string()),
        harness: data.harness.clone().unwrap_or_else(|| "omp".to_string()),
        max_turns: ext.max_turns.unwrap_or(100),
        budget_usd: ext.budget_usd.unwrap_or(1.0),
        timeout_min: ext.timeout_min.unwrap_or(120),
        api_key: ext.api_key.clone(),
        eval_only: ext.eval_only.unwrap_or(false),
        save_results: true,
        review_model: None,
        parent_run_id: ext.parent_run_id.clone(),
        start_idx,
        count,
        display_total: total,
        feedback_rounds: data.feedback_rounds.unwrap_or(0),
    };
    state.cancel_requested.store(false, Ordering::SeqCst);
    send_status(
        &tx,
        &log_buf,
        format!(
            "[EXTEND-QUEUE] {queue_id}: {reason}; running {count} more trial(s) for {} (trials {start_idx}-{total})...",
            ext.parent_run_id
        ),
    );
    let out = run_extension_batch(
        &tx,
        &state.log_buffer,
        &state.current_run,
        &state.cancel_requested,
        &|m: &str| {
            send_status(&tx, &log_buf, m.to_string());
        },
        spec,
        worker,
        &state.worker_buffers,
        &state.worker_runs,
    )
    .await;
    if out.billing_tripped {
        pause_queue(&state, "provider billing error in extension batch");
    }
    // Post-batch absorb (race belt): the pre-absorb above runs before the
    // batch, but trials from a concurrent chain could land mid-batch. Same
    // helper; the batch's own attempted numbers stay excluded.
    let attempted = start_idx..=(start_idx.saturating_add(count).saturating_sub(1));
    let known: std::collections::HashSet<String> = base_subruns
        .iter()
        .map(|s| s.id.clone())
        .chain(out.results.iter().map(|r| r.id.clone()))
        .collect();
    let orphans = load_orphans(
        &ext.parent_run_id,
        &data.model,
        &data.task,
        &known,
        Some(attempted),
    );
    if !orphans.absorb.is_empty() {
        send_status(
            &tx,
            &log_buf,
            format!(
                "[EXTEND-QUEUE] {queue_id}: absorbed {} orphan scored trial(s) missing from the merge (skipped: {} unscored, {} batch-attempted, {} mismatch).",
                orphans.absorb.len(),
                orphans.skipped_unscored,
                orphans.skipped_attempted,
                orphans.skipped_mismatch
            ),
        );
    }
    let mut combined = base_subruns.clone();
    combined.extend(out.results);
    combined.extend(orphans.absorb);
    // Deterministic trial order after absorb (orphans may fill gaps).
    combined.sort_by_key(|r| trial_no_of(&r.id).unwrap_or(u32::MAX));
    if combined.len() > data.subruns.len() {
        let passing = combined.iter().filter(|r| r.pass_rate >= 100.0).count();
        // Quarantine is sticky: the parent's stored drift OR the extension
        // drift. Passing only out.drift let a clean extension erase a drifted
        // parent (score 5.625 on a known-bad denominator, reproduced).
        let drift = data.ref_drift.or(out.drift).or(Some(false));
        let merged = LeaderboardManager::consolidate_multi_trial_run(
            &ext.parent_run_id,
            &combined,
            true,
            drift,
            data.protocol.clone(),
        );
        let parked: Vec<BenchmarkRunResult> = merged.map(|m| m.subruns).unwrap_or_default();
        let (stab2, reason2) = assess_stability(&parked);
        let summary = format!(
            "[EXTEND-QUEUE] {queue_id}: {}: {} trials, {passing} passing, Pass@1 {:.1}% — {stab2:?} ({reason2})",
            ext.parent_run_id,
            combined.len(),
            compute_pass_at_k(combined.len(), passing, 1) * 100.0
        );
        send_status(&tx, &log_buf, summary.clone());
        mark_queue_done(&state, &queue_id, Some(&summary));
    } else {
        // Honest empty-branch: name the actual shape instead of blanket
        // "all failed". Zero scored trials with zero errors and no billing
        // or cancel means nothing was attempted (e.g. pdf batch-ref fatal);
        // errors mean errored trials; billing/cancel speak for themselves.
        let cancelled = state.cancel_requested.load(Ordering::SeqCst);
        let note = if out.billing_tripped {
            format!("[EXTEND-QUEUE] {queue_id}: batch billing-paused; no scored trials, parent untouched.")
        } else if cancelled {
            format!(
                "[EXTEND-QUEUE] {queue_id}: batch cancelled; no scored trials, parent untouched."
            )
        } else if out.errors > 0 {
            format!("[EXTEND-QUEUE] {queue_id}: batch produced no scored trials ({} errored); parent untouched.", out.errors)
        } else {
            format!("[EXTEND-QUEUE] {queue_id}: batch produced no scored trials and no errors (setup refused before any trial); parent untouched.")
        };
        send_status(&tx, &log_buf, note.clone());
        mark_queue_done(&state, &queue_id, Some(&note));
    }
    release_slot(&state, worker);
    if !pump_queue(&state) {
        send_status(&tx, &log_buf, "[DONE]".to_string());
    }
}
pub fn spawn_run_body(state: &AppState, req: RunRequest, queue_id: Option<String>, worker: u8) {
    // Every engine run funnels through here, so this is the one place that
    // refuses an unknown task: `RunRequest.task` is a String resolved by
    // separate match blocks further down (each defaulting an unknown task to
    // Redis). This is the path a wave actually takes, and the one that spends.
    match TaskType::parse(&req.task) {
        Ok(_) => {}
        Err(e) => {
            // Must unwind exactly like every other early return here:
            // log, finish the queue item, release the slot, pump. Skipping
            // release_slot leaks the worker slot - with a 4-slot grid, four
            // refused cells wedge the wave, and is_running stays true because
            // only release_slot clears it when the last slot frees.
            let msg = format!("[QUEUE] refusing run: {e}");
            eprintln!("{msg}");
            push_console_line(&state.log_buffer, msg.clone());
            if let Some(id) = &queue_id {
                park_refused(state, id, &format!("unknown task ({e})"));
            }
            release_slot(state, worker);
            pump_queue(state);
            // Deliberately NOT touching queue_paused: a concurrent 402 sets it
            // as a breaker, and clearing it here would silently unpause a queue
            // that ran out of credit.
            return;
        }
    }
    // A previous `stop` must never poison the next run: the cancel flag is
    // per-run, cleared at dispatch. (Stale global flag once ate 15 queued
    // cells with 0-trial Dones.)
    state.cancel_requested.store(false, Ordering::SeqCst);
    let state_clone = state.clone();
    let handle = tokio::spawn(execute_run_body(state_clone, req, queue_id, worker));
    state.worker_handles.lock().unwrap().insert(worker, handle);
}
pub async fn execute_run_body(
    state: AppState,
    req: RunRequest,
    queue_id: Option<String>,
    worker: u8,
) {
    let tx = state.log_sender.clone();
    let run_log_buf = state.log_buffer.clone();
    let stop_on_pass = req.stop_on_pass || req.protocol.as_deref() == Some("sequential_cap");
    let mut total_trials = if stop_on_pass {
        screen_initial_trials(req.trials)
    } else {
        req.trials.max(1)
    };
    let mut passing_trials: usize = 0;
    // Trials degraded by harness faults (infra_error), counted apart: they
    // are corrupted evidence, the narrow-auto trigger's denominator.
    let mut infra_trials: usize = 0;
    // Drift contributed by a narrow-auto batch (declared here so the
    // post-loop merge sees it alongside the initial bracket's).
    let mut auto_drift = false;
    let mut subrun_results = Vec::new();
    // Millis + worker suffix: two workers dispatching in the same second
    // must not share one parent id — same-second collisions merged two
    // cells' trials into one runs/ dir (15-16 dirs on 20261001_151246).
    let run_timestamp = Utc::now().format("%Y%m%d_%H%M%S%.3f").to_string();
    let parent_run_id = format!(
        "{}_{}_{}_w{}",
        req.task,
        req.model.replace(['/', ':'], "_"),
        run_timestamp,
        worker
    );
    // Per-run cancel flag (cleanup discipline: removed at every exit below
    // AND by stop-by-id, so the map never grows across waves).
    let run_cancel = Arc::new(AtomicBool::new(false));
    state
        .cancel_flags
        .lock()
        .unwrap()
        .insert(parent_run_id.clone(), run_cancel.clone());

    let log_buf_batch = state.log_buffer.clone();
    let batch_log = |m: &str| {
        send_status(&tx, &log_buf_batch, m.to_string());
    };
    // Batch reference: one shared denominator for all trials.
    // Fallback is live per-trial measurement inside the pipeline.

    let mut any_drift = false;

    let mut trial_idx = 1;
    while trial_idx <= total_trials {
        if run_cancel.load(Ordering::SeqCst) || state.cancel_requested.load(Ordering::SeqCst) {
            send_status(
                &tx,
                &run_log_buf,
                "[STOP] Run cancelled by user before next trial.".to_string(),
            );
            break;
        }

        let started_at = Utc::now().to_rfc3339();
        let run_id = if total_trials > 1 || stop_on_pass {
            format!("{}_trial{}", parent_run_id, trial_idx)
        } else {
            parent_run_id.clone()
        };

        let effort_setting = req.effort.clone().unwrap_or_else(|| "high".to_string());

        let live_info = ActiveRunInfo {
            run_id: run_id.clone(),
            model: req.model.clone(),
            task: req.task.clone(),
            effort: effort_setting.clone(),
            started_at,
            turns: 0,
            max_turns: req.max_turns,
            cost_usd: 0.0,
            tokens: 0,
            duration_secs: None,
            subrun: Some(trial_idx),
            total_subruns: Some(total_trials),
            budget_usd: req.budget_usd,
            timeout_min: req.timeout_min.unwrap_or(120),
            trials: total_trials,
            eval_only: req.eval_only,
            extend_if_unstable: req.extend_if_unstable,
            max_trials: req.max_trials,
            review_model: req.review_model.clone(),
        };
        state
            .worker_runs
            .write()
            .unwrap()
            .insert(worker, live_info.clone());
        // Legacy mirror (lowest worker wins in get_status; single worker = exact).
        *state.current_run.write().unwrap() = Some(live_info);
        // Fresh shared view only when this is the sole live worker:
        // parallel workers used to wipe each other's trial-1 history.
        if trial_idx == 1 && state.worker_runs.read().unwrap().len() <= 1 {
            state.log_buffer.write().unwrap().clear();
        }

        let logger = Arc::new(WebLogger {
            tx: tx.clone(),
            log_buf: state.log_buffer.clone(),
            current_run: state.current_run.clone(),
            trial_idx,
            total_trials,
            worker,
            worker_bufs: state.worker_buffers.clone(),
            worker_runs: state.worker_runs.clone(),
        });

        if total_trials > 1 {
            logger.log(&format!(
                "[SUBRUN] ━━━ Starting Subrun {}/{} (Model: {}, Task: {}) ━━━",
                trial_idx, total_trials, req.model, req.task
            ));
        } else {
            logger.log(&format!(
                "[SUBRUN] ━━━ Starting Benchmark Run (Model: {}, Task: {}) ━━━",
                req.model, req.task
            ));
        }
        logger.log(&format!(
            "[LIVE] Subrun {}/{} | Turn 0/{} | Spent: $0.0000 | Tokens: 0",
            trial_idx, total_trials, req.max_turns
        ));
        logger.log(&format!(
            "[TELEMETRY] turn=0 max_turns={} spent=0.000000 tokens=0 subrun={} total_subruns={}",
            req.max_turns, trial_idx, total_trials
        ));

        let task_type = TaskType::Sql;
        let work_path = workdir_for_run(&run_id);

        let pipe_config = BenchmarkConfig {
            model: req.model.clone(),
            task: task_type,
            effort: effort_setting.clone(),
            harness: crate::config::Harness::parse(req.harness.as_deref().unwrap_or("omp")),
            max_turns: req.max_turns,
            budget_usd: req.budget_usd,
            timeout_min: req.timeout_min.unwrap_or(120),
            api_key: req.api_key.clone(),
            workdir: work_path,
            eval_only: req.eval_only,
            run_id: Some(run_id.clone()),
            save_results: req.save_results.unwrap_or(true),
            review_model: req.review_model.clone(),
            worker,
            feedback_rounds: req.feedback_rounds.unwrap_or(0),
            campaign_stage: None,
            baseline_rps: None,
        };

        let pipe_res = BenchmarkPipeline::execute(
            pipe_config,
            logger.clone(),
            Some(state.cancel_requested.clone()),
        )
        .await;

        match pipe_res {
            Ok(output) => {
                if output.infra_error {
                    // Harness fault, not a model verdict: counted apart for
                    // the narrow-auto trigger, still persisted in results.
                    infra_trials += 1;
                }
                if output.benchmark_result.pass_rate == 100.0 {
                    passing_trials += 1;
                    if total_trials > 1 {
                        logger.log(&format!(
                            "[SUBRUN] ━━━ Subrun {}/{} PASSED (100.0%) ━━━",
                            trial_idx, total_trials
                        ));
                    } else {
                        logger.log("[PIPELINE SUCCESS] Benchmark run PASSED (100.0%)");
                    }
                } else if total_trials > 1 || stop_on_pass {
                    logger.log(&format!(
                        "[SUBRUN] ━━━ Subrun {}/{} COMPLETED (Pass Rate: {:.1}%) ━━━",
                        trial_idx, total_trials, output.benchmark_result.pass_rate
                    ));
                } else {
                    logger.log(&format!(
                        "[PIPELINE RESULT] Benchmark run completed with pass rate: {:.1}%",
                        output.benchmark_result.pass_rate
                    ));
                }
                let passed_full = output.benchmark_result.pass_rate >= 100.0;
                subrun_results.push(output.benchmark_result);

                // Stop-on-pass: first 100% stage pass solves the cell and terminates trials
                if stop_on_pass && passed_full {
                    logger.log(&format!(
                        "[CAPABILITY] Solved at trial {} (100.0% stages passed)! Stopping cell (stop-on-pass protocol).",
                        trial_idx
                    ));
                    break;
                }
            }
            Err(e) => {
                if state.cancel_requested.load(Ordering::SeqCst) {
                    logger.log("[PIPELINE] Run cancelled by user.");
                    break;
                }
                // Billing tripwire: provider 402/401/429 (or OmpBillingError)
                // pauses the wave. Trial NOT scored, NOT persisted.
                let msg = format!("{e:#}");
                let billing = msg.to_lowercase().contains("billing:")
                    || crate::sandbox::omp::classify_billing_error(&msg).is_some();
                if billing {
                    logger.log(&format!(
                        "[BILLING] Subrun {}/{} hit provider billing error: {}; pausing queue, trial discarded.",
                        trial_idx, total_trials, msg
                    ));
                    pause_queue(
                        &state,
                        &format!("provider billing error in trial {trial_idx} ({msg})"),
                    );
                    break;
                }
                logger.log(&format!(
                    "[SUBRUN ERROR] Subrun {}/{} failed: {}",
                    trial_idx, total_trials, e
                ));
                break;
            }
        }

        // Narrow auto-extend: 0 passing with every completed trial a
        // harness fault is corrupted evidence, not a model verdict —
        // re-measure within the pre-approved envelope, once per cell.
        // Mixed signals, model failures, and cancels stay manual. Billing
        // needs no guard here: its arm always breaks the loop, so reaching
        // this point proves no billing fired. The reserve is upfront (no
        // refunds); the audit trail is the [AUTO-EXTEND] console lines.
        // Every sub-path breaks: the batch IS the remainder (falling
        // through would re-run duplicate trial numbers).
        if stop_on_pass
            && trial_idx == total_trials
            && !state.cancel_requested.load(Ordering::SeqCst)
        {
            // Pre-absorb: stranded scored trials rejoin before any trigger
            // math. A passing orphan already answers the cell (solve-break,
            // no spend); failing orphans join the denominator honestly.
            {
                let known: std::collections::HashSet<String> =
                    subrun_results.iter().map(|s| s.id.clone()).collect();
                let pre = load_orphans(&parent_run_id, &req.model, &req.task, &known, None);
                if !pre.absorb.is_empty() {
                    let pre_passing = pre.absorb.iter().filter(|r| r.pass_rate >= 100.0).count();
                    let n_pre = pre.absorb.len() as u32;
                    subrun_results.extend(pre.absorb);
                    passing_trials += pre_passing;
                    total_trials += n_pre;
                    if pre_passing > 0 {
                        send_status(
                            &tx,
                            &run_log_buf,
                            format!(
                                "[AUTO-EXTEND] cell already solved by {} stranded trial(s); no fire.",
                                n_pre
                            ),
                        );
                        break;
                    }
                    send_status(
                        &tx,
                        &run_log_buf,
                        format!(
                            "[AUTO-EXTEND] pre-absorbed {} failing orphan(s); re-evaluating trigger.",
                            n_pre
                        ),
                    );
                }
            }
            if !should_auto_extend(
                passing_trials,
                infra_trials,
                subrun_results.len(),
                total_trials,
                crate::config::SCREEN_CAP_TRIALS,
            ) {
                // Not all-harness-fault (a manual verdict or an orphan broke
                // the shape): stay manual. total_trials may have grown via
                // pre-absorb; the loop exits naturally below.
                break;
            }
            {
                let budget = auto_extend_envelope();
                match reserve_auto_extend(
                    &state.auto_extend_spent,
                    budget,
                    crate::config::SCREEN_CAP_TRIALS,
                ) {
                    Some(granted) => {
                        let batch_start = next_trial_idx(&subrun_results);
                        let spent = state.auto_extend_spent.load(Ordering::SeqCst);
                        send_status(
                        &tx,
                        &run_log_buf,
                        format!(
                            "[AUTO-EXTEND] 0/{} passing, all {} harness-fault; firing +{} trial(s) (envelope {}/{}).",
                            total_trials, infra_trials, granted, spent, budget
                        ),
                    );
                        let out = run_extension_batch(
                            &tx,
                            &state.log_buffer,
                            &state.current_run,
                            &state.cancel_requested,
                            &batch_log,
                            ExtBatchSpec {
                                model: req.model.clone(),
                                task: task_type,
                                effort: effort_setting.clone(),
                                harness: req.harness.clone().unwrap_or_else(|| "omp".to_string()),
                                max_turns: req.max_turns,
                                budget_usd: req.budget_usd,
                                timeout_min: req.timeout_min.unwrap_or(120),
                                api_key: req.api_key.clone(),
                                eval_only: req.eval_only,
                                save_results: req.save_results.unwrap_or(true),
                                review_model: req.review_model.clone(),
                                parent_run_id: parent_run_id.clone(),
                                start_idx: batch_start,
                                count: granted,
                                display_total: total_trials + granted,
                                feedback_rounds: req.feedback_rounds.unwrap_or(0),
                            },
                            worker,
                            &state.worker_buffers,
                            &state.worker_runs,
                        )
                        .await;
                        if out.billing_tripped {
                            send_status(
                            &tx,
                            &run_log_buf,
                            "[BILLING] auto-extend hit provider billing error; pausing queue, trials discarded.".to_string(),
                        );
                            pause_queue(&state, "provider billing error in auto-extend batch");
                            break;
                        } else {
                            // Post-batch absorb (race belt): a concurrent chain
                            // on this parent could land trials mid-batch. Fresh
                            // evidence keeps its numbers; the belt only takes
                            // the rest.
                            let attempted = batch_start
                                ..=(batch_start.saturating_add(granted).saturating_sub(1));
                            let known: std::collections::HashSet<String> = subrun_results
                                .iter()
                                .map(|s| s.id.clone())
                                .chain(out.results.iter().map(|r| r.id.clone()))
                                .collect();
                            let post = load_orphans(
                                &parent_run_id,
                                &req.model,
                                &req.task,
                                &known,
                                Some(attempted),
                            );
                            let n_new = (out.results.len() + post.absorb.len()) as u32;
                            let mut post_skipped = String::new();
                            if post.skipped_attempted > 0 {
                                post_skipped = format!(
                                    " ({} skipped: already attempted this batch)",
                                    post.skipped_attempted
                                );
                            }
                            if n_new == 0 {
                                // Honest empty-branch: the batch ran but scored
                                // nothing. Name the shape: billing and cancel
                                // speak for themselves; errors are counted;
                                // zero-of-everything means setup refused.
                                let cancelled = state.cancel_requested.load(Ordering::SeqCst);
                                if cancelled {
                                    send_status(
                                    &tx,
                                    &run_log_buf,
                                    "[AUTO-EXTEND] batch cancelled; no scored trials, parent untouched."
                                        .to_string(),
                                );
                                } else if out.errors > 0 {
                                    send_status(
                                    &tx,
                                    &run_log_buf,
                                    format!(
                                        "[AUTO-EXTEND] batch produced no scored trials ({} errored); parent untouched.",
                                        out.errors
                                    ),
                                );
                                } else {
                                    send_status(
                                    &tx,
                                    &run_log_buf,
                                    "[AUTO-EXTEND] batch produced no scored trials and no errors (setup refused before any trial); parent untouched."
                                        .to_string(),
                                );
                                }
                                break;
                            }
                            total_trials += n_new;
                            passing_trials += out.passing
                                + post.absorb.iter().filter(|r| r.pass_rate >= 100.0).count();
                            subrun_results.extend(out.results);
                            subrun_results.extend(post.absorb);
                            subrun_results.sort_by_key(|r| trial_no_of(&r.id).unwrap_or(u32::MAX));
                            auto_drift = auto_drift || out.drift.unwrap_or(false);
                            send_status(
                                &tx,
                                &run_log_buf,
                                format!(
                                    "[AUTO-EXTEND] absorbed {} trial(s){}; cell now {}/{}.",
                                    n_new, post_skipped, passing_trials, total_trials
                                ),
                            );
                            break;
                        }
                    }
                    None => {
                        send_status(
                        &tx,
                        &run_log_buf,
                        format!(
                            "[AUTO-EXTEND] 0/{} passing, all harness-fault, but envelope exhausted/disabled ({}=0 or spent) — manual extend or accept.",
                            total_trials,
                            crate::config::AUTO_EXTEND_BUDGET_ENV
                        ),
                    );
                    }
                }
            }
        }

        trial_idx += 1;
    }

    // Any untrusted denominator quarantines the parent: initial + extension
    // batches mint separate BatchRefs, so OR-accumulate drift across all of
    // them. (Gate: initial bracket drifted but extensions clean — the merge
    // kept only the last drift and scored off a known-bad denominator.)
    any_drift = any_drift || auto_drift;

    let mut merged_opt = if (total_trials > 1 || stop_on_pass || subrun_results.len() > 1)
        && !subrun_results.is_empty()
    {
        // Protocol is set BEFORE the merge/save so the JSON export and the
        // DB row agree (the old post-save upsert wrote only the DB).
        let protocol = if stop_on_pass {
            Some("sequential_cap".to_string())
        } else {
            None
        };
        LeaderboardManager::consolidate_multi_trial_run(
            &parent_run_id,
            &subrun_results,
            req.save_results.unwrap_or(true),
            Some(any_drift),
            protocol,
        )
    } else {
        None
    };

    // Dead-cell early stop: 0-1 passing at 3 trials never recovered in
    // wave-v2 (proxy/muse-spark 0/3 -> 2/9). Save the extend spend by
    // skipping the extend loop entirely.
    let dead_cell = total_trials <= 3
        && subrun_results
            .iter()
            .filter(|r| r.pass_rate >= 100.0)
            .count()
            <= 1;
    if dead_cell && !stop_on_pass {
        send_status(
            &tx,
            &run_log_buf,
            format!(
                "[EXTEND] dead cell ({}/{} passing at {}); stopping, no extend.",
                subrun_results
                    .iter()
                    .filter(|r| r.pass_rate >= 100.0)
                    .count(),
                subrun_results.len(),
                total_trials
            ),
        );
    }
    if req.extend_if_unstable && total_trials >= 2 && !dead_cell && !stop_on_pass {
        let max_total = req.max_trials.max(total_trials);
        let effort_setting = req.effort.clone().unwrap_or_else(|| "high".to_string());
        let task_type_ext = TaskType::Sql;
        while total_trials < max_total {
            let merged = match merged_opt {
                Some(m) => m,
                None => break,
            };
            let (stab, reason) = assess_stability(&merged.subruns);
            if stab == Stability::Stable {
                send_status(
                    &tx,
                    &run_log_buf,
                    format!("[EXTEND] stable: {}; done.", reason),
                );
                break;
            }
            let add = (max_total - total_trials).min(3);
            send_status(
                &tx,
                &run_log_buf,
                format!(
                    "[EXTEND] unstable ({}); running {} more trial(s)...",
                    reason, add
                ),
            );
            let out = run_extension_batch(
                &tx,
                &state.log_buffer,
                &state.current_run,
                &state.cancel_requested,
                &batch_log,
                ExtBatchSpec {
                    model: req.model.clone(),
                    task: task_type_ext,
                    effort: effort_setting.clone(),
                    harness: req.harness.clone().unwrap_or_else(|| "omp".to_string()),
                    max_turns: req.max_turns,
                    budget_usd: req.budget_usd,
                    timeout_min: req.timeout_min.unwrap_or(120),
                    api_key: req.api_key.clone(),
                    eval_only: req.eval_only,
                    save_results: req.save_results.unwrap_or(true),
                    review_model: req.review_model.clone(),
                    parent_run_id: parent_run_id.clone(),
                    start_idx: total_trials + 1,
                    count: add,
                    display_total: total_trials + add,
                    feedback_rounds: req.feedback_rounds.unwrap_or(0),
                },
                worker,
                &state.worker_buffers,
                &state.worker_runs,
            )
            .await;
            if out.billing_tripped {
                send_status(
                    &tx,
                    &run_log_buf,
                    "[BILLING] extension hit provider billing error; pausing queue, trials discarded.".to_string(),
                );
                pause_queue(&state, "provider billing error in extension batch");
                break;
            }
            if out.results.is_empty() {
                send_status(
                    &tx,
                    &run_log_buf,
                    "[EXTEND] all extension trials failed; keeping current merge.".to_string(),
                );
                break;
            }
            total_trials += out.results.len() as u32;
            passing_trials += out.passing;
            subrun_results.extend(out.results);
            any_drift = any_drift || out.drift.unwrap_or(false);
            merged_opt = LeaderboardManager::consolidate_multi_trial_run(
                &parent_run_id,
                &subrun_results,
                req.save_results.unwrap_or(true),
                Some(any_drift),
                // Auto-extend only runs inside stop-on-pass cells.
                Some("sequential_cap".to_string()),
            );
        }
    }

    if subrun_results.is_empty() {
        send_status(
            &tx,
            &run_log_buf,
            "[STATUS] No trials completed (cancelled or all errored); nothing persisted."
                .to_string(),
        );
    } else if stop_on_pass {
        let solved = passing_trials > 0;
        send_status(
            &tx,
            &run_log_buf,
            format!(
                "[CAPABILITY] Protocol: sequential stop-on-pass | Solved: {} (attempt {}/{})",
                if solved { "YES" } else { "NO" },
                subrun_results.len(),
                total_trials
            ),
        );
    } else if total_trials > 1 {
        let pass_at_1 = compute_pass_at_k(total_trials as usize, passing_trials, 1) * 100.0;
        let pass_at_k =
            compute_pass_at_k(total_trials as usize, passing_trials, total_trials as usize) * 100.0;
        send_status(
            &tx,
            &run_log_buf,
            format!(
                "[MULTI-TRIAL] Trials: {}, Passing: {}, Pass@1: {:.1}%, Pass@{}: {:.1}%",
                total_trials, passing_trials, pass_at_1, total_trials, pass_at_k
            ),
        );
    }

    if passing_trials == total_trials as usize && total_trials > 0 {
        send_status(
            &tx,
            &run_log_buf,
            "[STATUS] All trials passed (100% pass rate).".to_string(),
        );
    } else if passing_trials > 0 {
        send_status(
            &tx,
            &run_log_buf,
            format!(
                "[STATUS] Partial pass: {}/{} trials passed.",
                passing_trials, total_trials
            ),
        );
    } else {
        send_status(
            &tx,
            &run_log_buf,
            "[STATUS] Benchmark completed with 0 passing trials (0.0% pass rate).".to_string(),
        );
    }

    if let Some(info) = state.worker_runs.read().unwrap().get(&worker).cloned() {
        *state.last_run.write().unwrap() = Some(info);
    }
    if let Ok(mut cur_lock) = state.current_run.write() {
        *cur_lock = None;
    }
    state.cancel_flags.lock().unwrap().remove(&parent_run_id);
    release_slot(&state, worker);
    if let Some(qid) = queue_id {
        // Evidence gate: a Done with zero trials is loud, never silent.
        let reason = if subrun_results.is_empty() {
            "DONE WITH ZERO TRIALS — no evidence produced".to_string()
        } else {
            format!("{passing_trials}/{total_trials} trials passing")
        };
        if subrun_results.is_empty() {
            send_status(
                &tx,
                &run_log_buf,
                format!("[QUEUE-EVIDENCE] {qid}: {reason}."),
            );
        }
        mark_queue_done(&state, &qid, Some(&reason));
    }
    // Worker-slot handoff: freed slot + pump lets any free worker take next.
    if pump_queue(&state) {
        return;
    }
    send_status(&tx, &run_log_buf, "[DONE]".to_string());
}
/// Billing circuit breaker: pause the queue (Pending rows kept, never deleted).
pub fn pause_queue(state: &AppState, reason: &str) {
    state.queue_paused.store(true, Ordering::SeqCst);
    send_status(
        &state.log_sender,
        &state.log_buffer,
        format!("[BREAKER] Queue paused: {reason}. Top up balance to resume."),
    );
}
/// Explicit operator resume after a billing pause.
pub fn resume_queue(state: &AppState) {
    state.queue_paused.store(false, Ordering::SeqCst);
    send_status(
        &state.log_sender,
        &state.log_buffer,
        "[BREAKER] Queue resumed by operator.".to_string(),
    );
}
/// Try to start the head of the queue. Returns true when a run was spawned
/// or the item is already picked up. Bounded retries cover the
/// pop-None/store-false handoff gap at the end of a run body.
pub async fn kick_runner(state: &AppState, want_id: &str) -> bool {
    if state.queue_paused.load(Ordering::SeqCst) {
        return false;
    }
    for _ in 0..50 {
        // Claim a free slot first (None = all workers busy).
        let worker = match claim_slot(state) {
            Some(w) => w,
            None => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                continue;
            }
        };
        let head = {
            let mut q = state.queue.lock().unwrap();
            match q.iter().find(|i| i.id == want_id).map(|i| i.status) {
                None => {
                    release_slot(state, worker);
                    return true;
                }
                Some(s) if s != QueueStatus::Pending => {
                    release_slot(state, worker);
                    return true;
                }
                _ => {}
            }
            let now_sel = now_rfc3339();
            let next = q
                .iter_mut()
                .find(|i| i.status == QueueStatus::Pending && dispatch_ready(i, &now_sel))
                .map(|item| {
                    item.status = QueueStatus::Running;
                    (item.kind.clone(), item.id.clone())
                });
            if next.is_none() {
                release_slot(state, worker);
                return false;
            }
            next
        };
        match head {
            Some((kind, id)) => {
                let now = now_rfc3339();
                db_try(
                    &format!("mark {id} Running"),
                    state
                        .store
                        .update_queue_status(&id, "Running", Some(&now), None),
                );
                match kind {
                    QueueKind::Run(req) => spawn_run_body(state, req, Some(id), worker),
                    QueueKind::Extend(ext) => spawn_extend_body(state, ext, id, worker),
                }
                return true;
            }
            None => {
                release_slot(state, worker);
                return false;
            }
        }
    }
    false
}
/// Push a queue item, returning its id. Callers own kick/start decisions.
pub fn push_queue_item(state: &AppState, kind: QueueKind) -> String {
    let id = format!("q{}", state.queue_seq.fetch_add(1, Ordering::SeqCst) + 1);
    let evicted: Vec<String> = {
        let mut q = state.queue.lock().unwrap();
        q.push_back(QueuedRun {
            id: id.clone(),
            enqueued_at: Utc::now().to_rfc3339(),
            status: QueueStatus::Pending,
            note: None,
            refusals: 0,
            not_before: None,
            kind,
        });
        // Bound history: drop oldest finished entries past 100.
        let mut dropped = Vec::new();
        while q.len() > 100 {
            if let Some(pos) = q
                .iter()
                .position(|i| i.status == QueueStatus::Done || i.status == QueueStatus::Cancelled)
            {
                dropped.push(q.remove(pos).map(|item| item.id).unwrap_or_default());
            } else {
                break;
            }
        }
        dropped
    };
    // Durability mirror: memory stays source of truth; DB failures only log.
    if let Some(item) = state
        .queue
        .lock()
        .unwrap()
        .iter()
        .find(|i| i.id == id)
        .cloned()
    {
        let sid = id.clone();
        db_mirror(state, "enqueue", move |store| {
            db_try(&format!("enqueue {sid}"), store.enqueue_run(&item));
        });
    }
    if !evicted.is_empty() {
        db_mirror(state, "evict history", move |store| {
            for eid in &evicted {
                db_try(&format!("evict {eid}"), store.remove_queued(eid));
            }
        });
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn test_screen_initial_trials_caps_at_four() {
        // Flat cap 4 (was 3+escalate-5): min() binds large requests,
        // max(1) floors degenerate ones.
        assert_eq!(screen_initial_trials(9), 4);
        assert_eq!(screen_initial_trials(4), 4);
        assert_eq!(screen_initial_trials(2), 2);
        assert_eq!(screen_initial_trials(1), 1);
        assert_eq!(screen_initial_trials(0), 1);
    }

    #[test]
    fn test_should_auto_extend_only_all_harness_fault() {
        // The trigger case: 0 passing, every completed trial infra.
        assert!(should_auto_extend(0, 4, 4, 4, 4));
        // Mixed signals stay manual — one model failure vetoes.
        assert!(!should_auto_extend(0, 3, 4, 4, 4));
        // Any pass stays manual (solved is solved).
        assert!(!should_auto_extend(1, 3, 4, 4, 4));
        // Empty cell: nothing to re-measure.
        assert!(!should_auto_extend(0, 0, 0, 4, 4));
        // Early loop exit (fewer completed than planned): manual review,
        // the shortfall itself needs explaining.
        assert!(!should_auto_extend(0, 2, 2, 4, 4));
        // Past-cap cell (already extended once, human or auto): no refire.
        assert!(!should_auto_extend(0, 8, 8, 8, 4));
        // All-infra but over cap for the same reason.
        assert!(!should_auto_extend(0, 5, 5, 5, 4));
    }

    #[test]
    fn test_reserve_auto_extend_cas_and_partial() {
        // Disabled envelope: always None, never touches the counter.
        let spent = AtomicU64::new(0);
        assert_eq!(reserve_auto_extend(&spent, 0, 4), None);
        assert_eq!(spent.load(Ordering::SeqCst), 0);
        // Full grant within budget.
        assert_eq!(reserve_auto_extend(&spent, 10, 4), Some(4));
        assert_eq!(spent.load(Ordering::SeqCst), 4);
        // Partial grant at the edge (6 left of 10, want 4 → tested below
        // after consuming to 8).
        assert_eq!(reserve_auto_extend(&spent, 10, 4), Some(4));
        assert_eq!(spent.load(Ordering::SeqCst), 8);
        // Partial: 2 left, want 4 → 2.
        assert_eq!(reserve_auto_extend(&spent, 10, 4), Some(2));
        assert_eq!(spent.load(Ordering::SeqCst), 10);
        // Exhausted: None.
        assert_eq!(reserve_auto_extend(&spent, 10, 4), None);
        assert_eq!(spent.load(Ordering::SeqCst), 10);
    }

    fn trow(id: &str, status: &str, pass: f64) -> BenchmarkRunResult {
        trow_mt(id, status, pass, "m", "redis")
    }

    #[allow(clippy::too_many_arguments)]
    fn trow_mt(id: &str, status: &str, pass: f64, model: &str, task: &str) -> BenchmarkRunResult {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "model": model,
            "task": task,
            "language": "Go",
            "status": status,
            "pass_rate": pass,
            "passed_stages": if pass >= 100.0 { 6 } else { 0 },
            "total_stages": 6,
            "prompt_tokens": 0,
            "cached_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
            "total_cost_usd": 0.01,
            "savings_percent": 0.0,
            "efficiency_score": 0.0,
            "timestamp": "2026-10-12T00:00:00Z"
        }))
        .expect("test row")
    }

    #[test]
    fn test_trial_no_of_parses_only_numeric_suffix() {
        assert_eq!(trial_no_of("pdf_m_20261010_x_trial10"), Some(10));
        assert_eq!(trial_no_of("pdf_m_20261010_x_trial1"), Some(1));
        // Parent ids, empty suffixes, non-numeric suffixes: None.
        assert_eq!(trial_no_of("pdf_m_20261010_x"), None);
        assert_eq!(trial_no_of("pdf_m_20261010_x_trial"), None);
        assert_eq!(trial_no_of("pdf_m_20261010_x_trialX"), None);
        assert_eq!(trial_no_of("pdf_m_20261010_x_trial1x"), None);
        // A model literally named trial-9b must not parse (rsplit + digits).
        assert_eq!(trial_no_of("redis_openrouter_x_trial-9b_20261010_w0"), None);
        assert_eq!(trial_no_of(""), None);
    }

    #[test]
    fn test_is_scored_status_only_ranks_or_excludes() {
        // completed / failed_tests rank; disqualified_cheat is excluded by
        // merge rules but still evidence (listed, auditable).
        for s in ["completed", "failed_tests", "disqualified_cheat"] {
            assert!(is_scored_status(Some(s)), "{s}");
        }
        // Non-results by portal doctrine are never absorbed as evidence.
        for s in [
            Some("agent_failed"),
            Some("infra_error"),
            Some("agent_killed"),
            Some("something_new"),
            None,
        ] {
            assert!(!is_scored_status(s), "{s:?}");
        }
    }

    #[test]
    fn test_next_trial_idx_skips_gaps() {
        // Dense: len+1 either way.
        let subs: Vec<BenchmarkRunResult> = (1..=9)
            .map(|i| trow(&format!("p_trial{i}"), "completed", 100.0))
            .collect();
        assert_eq!(next_trial_idx(&subs), 10);
        // Gap: max-N+1, not len+1 (len+1 would reuse trial10's dir).
        let mut gapped = subs[..9].to_vec();
        gapped.push(trow("p_trial11", "completed", 100.0));
        assert_eq!(gapped.len(), 10);
        assert_eq!(next_trial_idx(&gapped), 12);
        // Empty and unparseable: safe fallbacks.
        assert_eq!(next_trial_idx(&[]), 1);
        assert_eq!(
            next_trial_idx(&[trow("parent-no-suffix", "completed", 100.0)]),
            2
        );
    }

    #[test]
    fn test_select_orphan_trials_filters_and_sorts() {
        let stored = vec![
            trow("p_trial10", "completed", 100.0),
            trow("p_trial11", "failed_tests", 0.0),
            trow("p_trial12", "disqualified_cheat", 0.0),
            trow("p_trial13", "agent_failed", 0.0),
            trow("p_trial14", "completed", 100.0),
            // Same trial number, different cell: cross-talk guard.
            trow_mt("q_trial10", "completed", 100.0, "other-model", "redis"),
        ];
        let mut known = std::collections::HashSet::new();
        known.insert("p_trial11".to_string());
        // No reservations: unknown scored same-cell trials absorb (sorted),
        // unscored/mismatch/known skip with counts.
        let rep = select_orphan_trials(stored.clone(), "m", "redis", &known, None);
        let ids: Vec<&str> = rep.absorb.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["p_trial10", "p_trial12", "p_trial14"]);
        assert_eq!(rep.skipped_unscored, 1);
        assert_eq!(rep.skipped_mismatch, 1);
        assert_eq!(rep.skipped_attempted, 0);
        // Fresh evidence wins number conflicts: batch attempting 10..11
        // excludes trial10 (skip logged, never silent).
        let rep2 = select_orphan_trials(stored, "m", "redis", &known, Some(10..=11));
        let ids2: Vec<&str> = rep2.absorb.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids2, vec!["p_trial12", "p_trial14"]);
        assert_eq!(rep2.skipped_attempted, 1);
    }

    #[test]
    fn test_auto_extend_envelope_env_parsing() {
        // Single test owns the process-global env (parallel tests share
        // env; cf. TEST_UI_TOKEN discipline in server.rs).
        let prev = std::env::var(crate::config::AUTO_EXTEND_BUDGET_ENV).ok();
        std::env::remove_var(crate::config::AUTO_EXTEND_BUDGET_ENV);
        assert_eq!(auto_extend_envelope(), 0);
        std::env::set_var(crate::config::AUTO_EXTEND_BUDGET_ENV, "12");
        assert_eq!(auto_extend_envelope(), 12);
        std::env::set_var(crate::config::AUTO_EXTEND_BUDGET_ENV, "nope");
        assert_eq!(auto_extend_envelope(), 0);
        match prev {
            Some(v) => std::env::set_var(crate::config::AUTO_EXTEND_BUDGET_ENV, v),
            None => std::env::remove_var(crate::config::AUTO_EXTEND_BUDGET_ENV),
        }
    }

    // ---------------------------------------------------------------------
    // Coverage additions: pure planning/pump/proxy/persistence logic.
    //
    // Queues are only driven through idle, paused, saturated, malformed, or
    // engine-refused states. Run bodies dispatched here always carry an
    // unknown task (refused before any pipeline/sandbox work) and extend
    // bodies reference a parent that does not exist. Store-backed checks
    // use explicit temp paths / tempfile stores; the one SDB_BENCH_DB
    // override is mutex-serialized and restored immediately.
    // ---------------------------------------------------------------------

    use crate::web::server::TEST_ENV_LOCK;

    fn test_state() -> AppState {
        let (log_sender, _) = broadcast::channel(100);
        AppState {
            log_sender,
            is_running: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            current_run: Arc::new(RwLock::new(None)),
            last_run: Arc::new(RwLock::new(None)),
            log_buffer: Arc::new(RwLock::new(Vec::new())),
            task_handle: Arc::new(std::sync::Mutex::new(None)),
            queue: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            queue_seq: Arc::new(AtomicU64::new(0)),
            queue_paused: Arc::new(AtomicBool::new(false)),
            auto_extend_spent: Arc::new(AtomicU64::new(0)),
            running_slots: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            worker_runs: Arc::new(RwLock::new(std::collections::HashMap::new())),
            worker_buffers: Arc::new(RwLock::new(std::collections::HashMap::new())),
            worker_handles: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            cancel_flags: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            store: Arc::new(tempfile_mem_store()),
            db_fallback: false,
            engine_url: String::new(),
        }
    }

    fn run_kind(task: &str) -> QueueKind {
        let mut req = test_queue_item("fixture").run_request().unwrap().clone();
        req.task = task.to_string();
        QueueKind::Run(req)
    }

    fn running_item(id: &str) -> QueuedRun {
        let mut item = test_queue_item(id);
        item.status = QueueStatus::Running;
        item
    }

    fn active_info(run_id: &str) -> ActiveRunInfo {
        ActiveRunInfo {
            run_id: run_id.to_string(),
            model: "m".to_string(),
            task: "redis".to_string(),
            effort: "high".to_string(),
            started_at: "2026-01-02T00:00:00Z".to_string(),
            turns: 0,
            max_turns: 100,
            cost_usd: 0.0,
            tokens: 0,
            duration_secs: None,
            subrun: None,
            total_subruns: None,
            budget_usd: 0.1,
            timeout_min: 1,
            trials: 1,
            eval_only: false,
            extend_if_unstable: false,
            max_trials: 9,
            review_model: None,
        }
    }

    #[test]
    fn test_default_knobs_match_cli_documented_values() {
        assert_eq!(default_trials(), 9);
        assert_eq!(default_max_trials(), 9);
        assert!(default_max_trials() >= default_trials());
        assert_eq!(default_max_turns(), 100);
        assert_eq!(default_budget(), 1.00);
        assert_eq!(default_timeout_min(), Some(120));
        // RunRequest serde defaults ride on the same fns.
        let req: RunRequest = serde_json::from_value(
            serde_json::json!({"model": "m", "task": "redis", "eval_only": true}),
        )
        .unwrap();
        assert_eq!(req.max_turns, 100);
        assert_eq!(req.timeout_min, Some(120));
        assert_eq!(req.budget_usd, 1.00);
        assert_eq!(req.trials, 9);
        assert_eq!(req.max_trials, 9);
        assert!(req.harness.is_none());
    }

    #[test]
    fn test_queued_run_accessors_for_extend_kind() {
        let ext = QueuedRun {
            id: "e1".to_string(),
            enqueued_at: "2026-01-02T00:00:00Z".to_string(),
            status: QueueStatus::Pending,
            note: None,
            refusals: 0,
            not_before: None,
            kind: QueueKind::Extend(ExtendRequest {
                parent_run_id: "p1".to_string(),
                trials: 2,
                max_turns: None,
                budget_usd: None,
                timeout_min: None,
                eval_only: None,
                api_key: Some("sk-secret".to_string()),
            }),
        };
        assert!(ext.run_request().is_none());
        assert!(!serde_json::to_string(&ext.redacted())
            .unwrap()
            .contains("sk-secret"));
        // A Run-kind entry still exposes its request through the accessor.
        let run = test_queue_item("qr");
        assert!(run.run_request().is_some());
    }

    #[tokio::test]
    async fn test_pump_queue_paused_saturated_empty_and_backoff() {
        // Paused: no claim, item untouched.
        let state = test_state();
        push_queue_item(&state, run_kind("no-such-task"));
        pause_queue(&state, "ut pause");
        assert!(!pump_queue(&state));
        assert!(state.running_slots.lock().unwrap().is_empty());
        assert_eq!(state.queue.lock().unwrap()[0].status, QueueStatus::Pending);

        // Saturated: every slot held.
        resume_queue(&state);
        for w in 0..worker_count() as u8 {
            state.running_slots.lock().unwrap().insert(w);
        }
        assert!(!pump_queue(&state));

        // Empty queue: slot claimed then released, no leak.
        let state = test_state();
        assert!(!pump_queue(&state));
        assert!(state.running_slots.lock().unwrap().is_empty());
        assert!(!state.is_running.load(Ordering::SeqCst));

        // Backoff window: parked item is skipped until not_before elapses.
        let state = test_state();
        push_queue_item(&state, run_kind("no-such-task"));
        {
            let mut q = state.queue.lock().unwrap();
            q[0].not_before = Some("2999-01-01T00:00:00Z".to_string());
        }
        assert!(!pump_queue(&state));
        assert!(state.running_slots.lock().unwrap().is_empty());
        assert_eq!(state.queue.lock().unwrap()[0].status, QueueStatus::Pending);

        // Past backoff: dispatch proceeds; the unknown task is refused
        // synchronously (no pipeline/sandbox work), parked with a note.
        {
            let mut q = state.queue.lock().unwrap();
            q[0].not_before = Some("2000-01-01T00:00:00Z".to_string());
        }
        assert!(pump_queue(&state));
        let q = state.queue.lock().unwrap();
        assert_eq!(q[0].status, QueueStatus::Pending);
        assert_eq!(q[0].refusals, 1);
        assert!(q[0].note.as_deref().unwrap_or("").contains("unknown task"));
        assert!(state.running_slots.lock().unwrap().is_empty());
        assert!(!state.is_running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_pump_queue_dispatches_extend_and_skips_missing_parent() {
        let state = test_state();
        let ext = ExtendRequest {
            parent_run_id: "sdb_ut_missing_parent".to_string(),
            trials: 3,
            max_turns: None,
            budget_usd: None,
            timeout_min: None,
            eval_only: None,
            api_key: None,
        };
        let qid = push_queue_item(&state, QueueKind::Extend(ext));
        assert!(pump_queue(&state));
        // The extend body runs async; wait for its terminal transition.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let done = {
                let q = state.queue.lock().unwrap();
                q.iter()
                    .find(|i| i.id == qid)
                    .map(|i| i.status == QueueStatus::Done)
                    .unwrap_or(false)
            };
            if done || std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let q = state.queue.lock().unwrap();
        let item = q.iter().find(|i| i.id == qid).unwrap();
        assert_eq!(item.status, QueueStatus::Done);
        assert!(
            item.note
                .as_deref()
                .unwrap_or("")
                .contains("gone; skipping"),
            "missing parent must be named: {:?}",
            item.note
        );
        assert!(state.running_slots.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_kick_runner_idle_branches_and_unknown_task_dispatch() {
        // Paused kick is a no-op.
        let state = test_state();
        let qid = push_queue_item(&state, run_kind("redis"));
        pause_queue(&state, "ut");
        assert!(!kick_runner(&state, &qid).await);
        assert!(state.running_slots.lock().unwrap().is_empty());
        resume_queue(&state);

        // Unknown id: claimed slot is released; treated as already handled.
        assert!(kick_runner(&state, "q_no_such").await);
        assert!(state.running_slots.lock().unwrap().is_empty());

        // Non-pending id: same release-and-return.
        {
            let mut q = state.queue.lock().unwrap();
            q.iter_mut().find(|i| i.id == qid).unwrap().status = QueueStatus::Done;
        }
        assert!(kick_runner(&state, &qid).await);
        assert!(state.running_slots.lock().unwrap().is_empty());

        // Pending but inside backoff: no dispatch, slot released.
        let state = test_state();
        {
            let mut item = test_queue_item("q_wait");
            item.not_before = Some("2999-01-01T00:00:00Z".to_string());
            state.queue.lock().unwrap().push_back(item);
        }
        assert!(!kick_runner(&state, "q_wait").await);
        assert!(state.running_slots.lock().unwrap().is_empty());

        // Ready + unknown task: dispatched, refused, parked Pending.
        let state = test_state();
        let qid = push_queue_item(&state, run_kind("no-such-task"));
        assert!(kick_runner(&state, &qid).await);
        let q = state.queue.lock().unwrap();
        assert_eq!(q[0].refusals, 1);
        assert_eq!(q[0].status, QueueStatus::Pending);
        assert!(state.running_slots.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_reconcile_queue_skips_when_handle_or_worker_live() {
        // A live worker handle (even without slots) marks the engine busy.
        let state = test_state();
        state
            .queue
            .lock()
            .unwrap()
            .push_back(running_item("q_handle"));
        let handle = tokio::spawn(async { tokio::time::sleep(Duration::from_millis(30)).await });
        state.worker_handles.lock().unwrap().insert(0, handle);
        assert_eq!(reconcile_queue(&state), 0);
        assert_eq!(state.queue.lock().unwrap()[0].status, QueueStatus::Running);

        // A live worker_runs entry marks it busy too.
        let state = test_state();
        state
            .queue
            .lock()
            .unwrap()
            .push_back(running_item("q_worker"));
        state
            .worker_runs
            .write()
            .unwrap()
            .insert(2, active_info("redis_m_20260102_w2"));
        assert_eq!(reconcile_queue(&state), 0);
        assert_eq!(state.queue.lock().unwrap()[0].status, QueueStatus::Running);
    }

    #[test]
    fn test_park_refused_unknown_id_is_terminal_noop() {
        let state = test_state();
        assert!(park_refused(&state, "q_absent", "nobody home"));
        assert!(state.queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_pump_queue_throttles_free_model_over_two_live() {
        let state = test_state();
        // Two live free-model runs are already in flight.
        for w in 0..2u8 {
            let mut info = active_info(&format!("free_w{w}"));
            info.model = "vendor/model:free".to_string();
            state.worker_runs.write().unwrap().insert(w, info);
        }
        let mut item = test_queue_item("q_free");
        match &mut item.kind {
            QueueKind::Run(r) => r.model = "vendor/model:free".to_string(),
            QueueKind::Extend(_) => panic!("fixture is a Run"),
        }
        state.queue.lock().unwrap().push_back(item);
        // No slots are actually held, so pump claims one, skips the
        // throttled free cell (2 already live), and releases cleanly.
        assert!(!pump_queue(&state));
        assert!(state.running_slots.lock().unwrap().is_empty());
        assert_eq!(state.queue.lock().unwrap()[0].status, QueueStatus::Pending);
    }

    #[tokio::test]
    async fn test_execute_run_body_cancel_before_trial_is_loud_and_clean() {
        // Cancelled before trial 1: the body must exit through the normal
        // unwind (release slot, clear cancel flag, mark the queue row with
        // a zero-evidence reason) without ever reaching the pipeline.
        let state = test_state();
        let worker = claim_slot(&state).expect("free worker");
        let req = test_queue_item("q_cancel").run_request().unwrap().clone();
        let qid = push_queue_item(&state, QueueKind::Run(req.clone()));
        state.cancel_requested.store(true, Ordering::SeqCst);
        execute_run_body(state.clone(), req, Some(qid.clone()), worker).await;

        {
            let q = state.queue.lock().unwrap();
            let item = q.iter().find(|i| i.id == qid).unwrap();
            assert_eq!(item.status, QueueStatus::Done);
            assert!(
                item.note.as_deref().unwrap_or("").contains("ZERO TRIALS"),
                "zero-evidence Done must be loud: {:?}",
                item.note
            );
        }
        assert!(state.running_slots.lock().unwrap().is_empty());
        assert!(!state.is_running.load(Ordering::SeqCst));
        assert!(
            state.cancel_flags.lock().unwrap().is_empty(),
            "per-run cancel flags must not leak across cells"
        );
        let log = state.log_buffer.read().unwrap();
        assert!(log
            .iter()
            .any(|l| l.contains("[STOP] Run cancelled by user before next trial.")));
        assert!(log
            .iter()
            .any(|l| l.contains("[STATUS] No trials completed")));
        assert!(log.iter().any(|l| l.contains("[QUEUE-EVIDENCE]")));
    }

    #[tokio::test]
    async fn test_mark_queue_done_unknown_id_is_noop() {
        let state = test_state();
        mark_queue_done(&state, "q_absent", Some("nothing to mark"));
        assert!(state.queue.lock().unwrap().is_empty());
    }

    #[test]
    fn test_push_queue_item_evicts_finished_history_past_cap() {
        let state = test_state();
        let mut ids = Vec::new();
        for _ in 0..101 {
            ids.push(push_queue_item(&state, run_kind("redis")));
        }
        assert_eq!(state.queue.lock().unwrap().len(), 101);
        mark_queue_done(&state, &ids[0], Some("done"));
        mark_queue_done(&state, &ids[1], Some("done"));
        // Two more pushes overflow the 100-entry cap; both finished rows
        // are evicted before any pending row is touched.
        push_queue_item(&state, run_kind("redis"));
        push_queue_item(&state, run_kind("redis"));
        let q = state.queue.lock().unwrap();
        assert_eq!(q.len(), 101);
        assert!(
            q.iter().all(|i| i.status == QueueStatus::Pending),
            "no finished rows may survive the history cap"
        );
        assert!(!q.iter().any(|i| i.id == ids[0] || i.id == ids[1]));
    }

    #[test]
    fn test_pause_resume_log_and_flag_lifecycle() {
        let state = test_state();
        pause_queue(&state, "insufficient credit");
        assert!(state.queue_paused.load(Ordering::SeqCst));
        assert!(state
            .log_buffer
            .read()
            .unwrap()
            .iter()
            .any(|l| l.contains("Queue paused: insufficient credit")));
        resume_queue(&state);
        assert!(!state.queue_paused.load(Ordering::SeqCst));
        assert!(state
            .log_buffer
            .read()
            .unwrap()
            .iter()
            .any(|l| l.contains("Queue resumed by operator")));
    }

    #[test]
    fn test_db_mirror_swallows_panics_and_db_try_is_non_fatal() {
        let state = test_state();
        // A panicking mirror closure must never take the queue path down.
        db_mirror(&state, "ut panic", |_| panic!("mirror boom"));
        db_try("ut err", Err(anyhow::anyhow!("db unavailable")));
        db_try("ut ok", Ok(()));
        // State remains fully usable after the swallowed panic.
        let id = push_queue_item(&state, run_kind("redis"));
        assert!(id.starts_with('q'));
        assert_eq!(state.queue.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_queue_store_roundtrip_replay_and_seq() {
        let db = std::env::temp_dir().join(format!(
            "sdb-ut-queue-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let store = Store::open(&db).expect("temp queue db");
        assert!(replay_queue(&store).is_empty());
        assert_eq!(queue_seq_start(&store), 0);

        let mut q1 = test_queue_item("q1");
        q1.status = QueueStatus::Pending;
        store.enqueue_run(&q1).unwrap();
        // A finished row contributes to the sequence high-water mark but is
        // not replayed.
        let mut q7 = test_queue_item("q7");
        q7.status = QueueStatus::Done;
        store.enqueue_run(&q7).unwrap();
        // Running rows reset to Pending on replay (previous process died).
        store
            .update_queue_status("q1", "Running", Some("2026-01-02T00:00:00Z"), None)
            .unwrap();
        let replayed = replay_queue(&store);
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].id, "q1");
        assert_eq!(replayed[0].status, QueueStatus::Pending);
        assert_eq!(queue_seq_start(&store), 7);
        // Non-numeric ids are ignored by the sequence scan.
        let mut qx = test_queue_item("qx");
        qx.status = QueueStatus::Pending;
        store.enqueue_run(&qx).unwrap();
        assert_eq!(queue_seq_start(&store), 7);

        store.enqueue_run(&test_queue_item("q8")).unwrap();
        assert_eq!(store.load_queue(false).len(), 3); // q1, qx, q8
        assert_eq!(store.clear_finished_queue().unwrap(), 1); // q7
        store.remove_queued("q8").unwrap();
        let left: Vec<String> = store.load_queue(true).into_iter().map(|i| i.id).collect();
        assert_eq!(left, vec!["q1".to_string(), "qx".to_string()]);
        drop(store);
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(db.with_extension("db-wal"));
        let _ = std::fs::remove_file(db.with_extension("db-shm"));
    }

    #[test]
    fn test_load_orphans_reads_store_trials() {
        let dir = std::env::temp_dir().join(format!(
            "sdb-ut-orphans-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("bench.db");
        let store = Store::open(&db).unwrap();
        let mut parent = trow("ut_parent", "completed", 50.0);
        parent.subruns = vec![
            trow("ut_parent_trial1", "completed", 100.0),
            trow("ut_parent_trial2", "infra_error", 0.0),
            trow("ut_parent_trial3", "failed_tests", 0.0),
        ];
        store.upsert_parent(&parent).unwrap();

        let known = std::collections::HashSet::new();
        let rep = select_orphans_from_store(&store, "ut_parent", "m", "redis", &known, None);
        assert_eq!(
            rep.absorb.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["ut_parent_trial1", "ut_parent_trial3"]
        );
        assert_eq!(rep.skipped_unscored, 1);
        assert_eq!(rep.skipped_mismatch, 0);
        assert_eq!(rep.skipped_attempted, 0);
        // Attempted range excludes the fresh batch's own numbers.
        let attempted = 1..=1;
        let rep2 =
            select_orphans_from_store(&store, "ut_parent", "m", "redis", &known, Some(attempted));
        assert_eq!(
            rep2.absorb
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["ut_parent_trial3"]
        );
        assert_eq!(rep2.skipped_attempted, 1);
        // A parent with no stored trials yields an all-zero report.
        let rep3 = select_orphans_from_store(&store, "ut_absent", "m", "redis", &known, None);
        assert!(rep3.absorb.is_empty());
        assert_eq!(rep3.skipped_unscored, 0);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_web_logger_rewrites_live_telemetry_and_updates_cards() {
        let state = test_state();
        let info = active_info("redis_m_20260102_w3");
        *state.current_run.write().unwrap() = Some(info.clone());
        state.worker_runs.write().unwrap().insert(3, info);
        let logger = WebLogger {
            tx: state.log_sender.clone(),
            log_buf: state.log_buffer.clone(),
            current_run: state.current_run.clone(),
            trial_idx: 2,
            total_trials: 4,
            worker: 3,
            worker_bufs: state.worker_buffers.clone(),
            worker_runs: state.worker_runs.clone(),
        };
        // Multi-trial LIVE lines are rewritten per-subrun; multi-line
        // messages are split into one console line each.
        logger.log("[LIVE] Turn 5/100 | Spent: $0.10\nsecond line");
        // Telemetry without explicit subrun= gets the current counters.
        logger.log("[TELEMETRY] turn=7 max_turns=50 spent=0.25 tokens=123");
        // Telemetry with subrun=/total_subruns= keeps them, but still
        // updates the card counters.
        logger
            .log("[TELEMETRY] turn=8 max_turns=60 spent=0.30 tokens=200 subrun=3 total_subruns=9");

        let buf = state.log_buffer.read().unwrap();
        assert!(buf.iter().any(|l| l.contains("Subrun 2/4 |")));
        assert!(buf.iter().any(|l| l.contains("second line")));
        assert!(buf.iter().any(|l| l.contains("subrun=2 total_subruns=4")));
        assert!(buf.iter().any(|l| l.contains("subrun=3 total_subruns=9")));
        drop(buf);
        let runs = state.worker_runs.read().unwrap();
        let card = runs.get(&3).unwrap();
        assert_eq!(card.turns, 8);
        assert_eq!(card.max_turns, 60);
        assert!((card.cost_usd - 0.30).abs() < 1e-9);
        assert_eq!(card.tokens, 200);
        assert_eq!(card.subrun, Some(3));
        assert_eq!(card.total_subruns, Some(9));
        let shared = state.current_run.read().unwrap().clone().unwrap();
        assert_eq!(shared.turns, 8);
        assert_eq!(shared.tokens, 200);
        // Worker-private buffer mirrors the shared one.
        assert_eq!(
            state
                .worker_buffers
                .read()
                .unwrap()
                .get(&3)
                .map(|v| v.len()),
            Some(4)
        );
    }

    #[tokio::test]
    async fn test_run_extension_batch_cancel_and_empty_counts() {
        let state = test_state();
        let (tx, _rx) = broadcast::channel(16);
        let cancel = Arc::new(AtomicBool::new(true));
        let spec = ExtBatchSpec {
            model: "m".to_string(),
            task: TaskType::Sql,
            effort: "low".to_string(),
            harness: "omp".to_string(),
            max_turns: 1,
            budget_usd: 0.1,
            timeout_min: 1,
            api_key: None,
            eval_only: true,
            save_results: false,
            review_model: None,
            parent_run_id: "sdb_ut_cancel_parent".to_string(),
            start_idx: 2,
            count: 1,
            display_total: 2,
            feedback_rounds: 0,
        };
        let noop = |_m: &str| {};
        let out = run_extension_batch(
            &tx,
            &state.log_buffer,
            &state.current_run,
            &cancel,
            &noop,
            spec,
            0,
            &state.worker_buffers,
            &state.worker_runs,
        )
        .await;
        assert!(out.results.is_empty());
        assert_eq!(out.passing, 0);
        assert_eq!(out.errors, 0);
        assert!(!out.billing_tripped);
        assert!(out.drift.is_none());
        assert!(state
            .log_buffer
            .read()
            .unwrap()
            .iter()
            .any(|l| l.contains("[STOP] Extension cancelled by user.")));

        // count == 0: no loop body, clean empty result (setup-refusal shape).
        let spec0 = ExtBatchSpec {
            model: "m".to_string(),
            task: TaskType::Sql,
            effort: "low".to_string(),
            harness: "omp".to_string(),
            max_turns: 1,
            budget_usd: 0.1,
            timeout_min: 1,
            api_key: None,
            eval_only: true,
            save_results: false,
            review_model: None,
            parent_run_id: "sdb_ut_zero_parent".to_string(),
            start_idx: 5,
            count: 0,
            display_total: 5,
            feedback_rounds: 0,
        };
        let out0 = run_extension_batch(
            &tx,
            &state.log_buffer,
            &state.current_run,
            &cancel,
            &noop,
            spec0,
            0,
            &state.worker_buffers,
            &state.worker_runs,
        )
        .await;
        assert!(out0.results.is_empty());
        assert!(!out0.billing_tripped);
    }

    /// Mock engine capturing method/token/body; ephemeral port only.
    async fn serve_capture_engine(seen: Arc<std::sync::Mutex<Vec<String>>>) -> String {
        async fn capture(
            axum::extract::State(seen): axum::extract::State<Arc<std::sync::Mutex<Vec<String>>>>,
            req: axum::extract::Request,
        ) -> String {
            let method = req.method().to_string();
            let auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                .await
                .unwrap_or_default();
            seen.lock().unwrap().push(format!(
                "{method}|{auth}|{}",
                String::from_utf8_lossy(&body)
            ));
            "captured".to_string()
        }
        let app = axum::Router::new()
            .route("/cap", axum::routing::any(capture))
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn test_proxy_read_status_header_and_query_passthrough() {
        async fn teapot(
            axum::extract::Query(q): axum::extract::Query<
                std::collections::HashMap<String, String>,
            >,
        ) -> axum::response::Response {
            (
                axum::http::StatusCode::IM_A_TEAPOT,
                [
                    ("x-console-total", "42"),
                    ("content-type", "text/plain; charset=utf-8"),
                ],
                format!("q={}", q.get("q").cloned().unwrap_or_default()),
            )
                .into_response()
        }
        let app = axum::Router::new().route("/teapot", axum::routing::get(teapot));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut state = test_state();
        state.engine_url = format!("http://127.0.0.1:{port}");

        let resp = proxy_read(&state, "/teapot", "?q=hi").await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::IM_A_TEAPOT);
        assert_eq!(
            resp.headers()
                .get("x-console-total")
                .unwrap()
                .to_str()
                .unwrap(),
            "42"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "q=hi");

        // Empty engine_url = single-process mode, no proxying.
        let plain = test_state();
        assert!(proxy_read(&plain, "/teapot", "").await.is_none());
        assert!(proxy_mutation(&plain, "POST", "/cap", None).await.is_none());
    }

    // Env serialization deliberately spans awaits: the vars must stay
    // pinned for the requests under test. No handler takes TEST_ENV_LOCK,
    // so the guard cannot deadlock the runtime.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_proxy_mutation_forwards_method_token_and_body() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SDB_UI_TOKEN", "sdb-test-ui-token");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = serve_capture_engine(seen.clone()).await;
        let mut state = test_state();
        state.engine_url = engine;

        let del = proxy_mutation(&state, "DELETE", "/cap", None)
            .await
            .unwrap();
        assert_eq!(del.status(), 200);
        let post = proxy_mutation(&state, "POST", "/cap", Some(serde_json::json!({"a": 1})))
            .await
            .unwrap();
        assert_eq!(post.status(), 200);
        let captured = seen.lock().unwrap().clone();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0], "DELETE|Bearer sdb-test-ui-token|");
        assert_eq!(captured[1], "POST|Bearer sdb-test-ui-token|{\"a\":1}");
    }
}
