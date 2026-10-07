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
use std::sync::atomic::{AtomicBool, Ordering};
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

pub fn default_budget() -> f64 {
    1.00
}

pub fn default_timeout_min() -> Option<u64> {
    Some(30)
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
}
pub struct ExtBatchOut {
    pub(crate) results: Vec<BenchmarkRunResult>,
    pub(crate) passing: usize,
    pub(crate) drift: Option<bool>,
    /// Provider billing trip during extensions: callers pause the queue.
    pub(crate) billing_tripped: bool,
}
/// Run one extension batch (fresh batch reference, suffix-numbered trial dirs).
/// Shared by auto-extend and the extend endpoint; never touches the runner slot.
pub async fn run_extension_batch(
    tx: &broadcast::Sender<String>,
    log_buf: &Arc<RwLock<Vec<String>>>,
    current_run: &Arc<RwLock<Option<ActiveRunInfo>>>,
    cancel: &Arc<AtomicBool>,
    batch_log: &(dyn Fn(&str) + Sync),
    spec: ExtBatchSpec,
    worker: u8,
) -> ExtBatchOut {
    let ext_batch_ref = if spec.count > 1 {
        // Per-bracket id: the extension's pre must pair with ITS post, not
        // the initial bracket's pre (regime-change the bracket exists to
        // catch). `_ext<N>` brackets vs `_trial<N>` trial dirs below:
        // distinct suffix spaces for brackets vs trials.
        let bracket_id = format!("{}_ext{}", spec.parent_run_id, spec.start_idx);
        match crate::bench::batch_ref::BatchRef::start_on_worker(
            &spec.task,
            &spec.parent_run_id,
            batch_log,
            worker,
            Some(&bracket_id),
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                // Pdf has no live-ref fallback: FATAL (loud) instead of
                // warn-and-None (which would silently return pdf to unscored).
                // Other tasks keep the live-ref fallback.
                if spec.task == TaskType::Pdf {
                    let msg = format!("[BATCH-REF] FATAL pdf batch ref: {}; failing batch.", e);
                    send_status(tx, log_buf, msg.clone());
                    batch_log(&msg);
                    return ExtBatchOut {
                        results: vec![],
                        passing: 0,
                        drift: None,
                        billing_tripped: false,
                    };
                }
                send_status(
                    tx,
                    log_buf,
                    format!("[BATCH-REF] setup failed ({}); trials use live refs.", e),
                );
                None
            }
        }
    } else {
        None
    };
    let mut results = Vec::new();
    let mut passing = 0usize;
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
            batch_reference: ext_batch_ref.as_ref().map(|b| b.reference()),
            worker,
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
                break;
            }
        }
    }
    let drift = match ext_batch_ref {
        Some(b) => Some(b.finish(batch_log).await.drift),
        None => None,
    };
    ExtBatchOut {
        results,
        passing,
        drift,
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
        q.iter_mut()
            .find(|i| i.status == QueueStatus::Pending && dispatch_ready(i, &now_sel))
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
    let start_idx = data.subruns.len() as u32 + 1;
    let total = data.subruns.len() as u32 + count;
    let spec = ExtBatchSpec {
        model: data.model.clone(),
        task: task_type,
        effort: data.effort.clone().unwrap_or_else(|| "high".to_string()),
        harness: data.harness.clone().unwrap_or_else(|| "omp".to_string()),
        max_turns: ext.max_turns.unwrap_or(100),
        budget_usd: ext.budget_usd.unwrap_or(1.0),
        timeout_min: ext.timeout_min.unwrap_or(30),
        api_key: ext.api_key.clone(),
        eval_only: ext.eval_only.unwrap_or(false),
        save_results: true,
        review_model: None,
        parent_run_id: ext.parent_run_id.clone(),
        start_idx,
        count,
        display_total: total,
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
    )
    .await;
    if out.billing_tripped {
        pause_queue(&state, "provider billing error in extension batch");
    }
    if !out.results.is_empty() {
        let mut combined = data.subruns.clone();
        combined.extend(out.results);
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
        );
        let parked: Vec<BenchmarkRunResult> = merged.map(|m| m.subruns).unwrap_or_default();
        let (stab2, reason2) = assess_stability(&parked);
        send_status(
            &tx,
            &log_buf,
            format!(
                "[EXTEND-QUEUE] {queue_id}: {}: {} trials, {passing} passing, Pass@1 {:.1}% — {stab2:?} ({reason2})",
                ext.parent_run_id,
                combined.len(),
                compute_pass_at_k(combined.len(), passing, 1) * 100.0
            ),
        );
    } else {
        send_status(
            &tx,
            &log_buf,
            format!("[EXTEND-QUEUE] {queue_id}: all extension trials failed; parent untouched."),
        );
    }
    mark_queue_done(
        &state,
        &queue_id,
        Some("extend finished: all extension trials failed, parent untouched"),
    );
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
    let mut total_trials = req.trials.max(1);
    let mut passing_trials = 0;
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
    let batch_ref = if total_trials > 1 {
        let task_type_pre = match req.task.as_str() {
            "redis" => TaskType::Redis,
            "http" => TaskType::Http,
            "pdf" => TaskType::Pdf,
            "wal" => TaskType::Wal,
            "proxy" => TaskType::Proxy,
            "pubsub" => TaskType::Pubsub,
            "binproto" => TaskType::Binproto,
            _ => TaskType::Redis,
        };
        match crate::bench::batch_ref::BatchRef::start_on_worker(
            &task_type_pre,
            &parent_run_id,
            &batch_log,
            worker,
            None,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                // Pdf has no live-ref fallback: FATAL (loud). Other tasks
                // keep warn-and-None with live per-trial refs.
                if task_type_pre == TaskType::Pdf {
                    let msg = format!(
                        "[BATCH-REF] FATAL pdf batch ref: {}; failing cell (no silent unscored fallback).",
                        e
                    );
                    batch_log(&msg);
                    send_status(&tx, &run_log_buf, msg.clone());
                    // Mirror the normal exit (DONE block below): no parent
                    // row is produced, so the drain gate WILL fail on count
                    // until the cell is re-run. That is the loudness: never
                    // "fix" the gate instead of the ref.
                    if let Ok(mut cur_lock) = state.current_run.write() {
                        *cur_lock = None;
                    }
                    state.cancel_flags.lock().unwrap().remove(&parent_run_id);
                    release_slot(&state, worker);
                    if let Some(qid) = queue_id.clone() {
                        park_refused(
                            &state,
                            &qid,
                            &format!("pdf batch reference unavailable ({msg})"),
                        );
                    }
                    pump_queue(&state);
                    return;
                }
                batch_log(&format!(
                    "[BATCH-REF] setup failed ({}); trials use live refs.",
                    e
                ));
                None
            }
        }
    } else {
        None
    };

    for trial_idx in 1..=total_trials {
        if run_cancel.load(Ordering::SeqCst) || state.cancel_requested.load(Ordering::SeqCst) {
            send_status(
                &tx,
                &run_log_buf,
                "[STOP] Run cancelled by user before next trial.".to_string(),
            );
            break;
        }

        let started_at = Utc::now().to_rfc3339();
        let run_id = if total_trials > 1 {
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
            timeout_min: req.timeout_min.unwrap_or(30),
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
        if trial_idx == 1 {
            state.log_buffer.write().unwrap().clear();
        }

        let logger = Arc::new(WebLogger {
            tx: tx.clone(),
            log_buf: state.log_buffer.clone(),
            current_run: state.current_run.clone(),
            trial_idx,
            total_trials,
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

        let task_type = match req.task.as_str() {
            "redis" => TaskType::Redis,
            "http" => TaskType::Http,
            "pdf" => TaskType::Pdf,
            "wal" => TaskType::Wal,
            "proxy" => TaskType::Proxy,
            "pubsub" => TaskType::Pubsub,
            "binproto" => TaskType::Binproto,
            _ => TaskType::Redis,
        };

        let work_path = workdir_for_run(&run_id);

        let pipe_config = BenchmarkConfig {
            model: req.model.clone(),
            task: task_type,
            effort: effort_setting.clone(),
            harness: crate::config::Harness::parse(req.harness.as_deref().unwrap_or("omp")),
            max_turns: req.max_turns,
            budget_usd: req.budget_usd,
            timeout_min: req.timeout_min.unwrap_or(30),
            api_key: req.api_key.clone(),
            workdir: work_path,
            eval_only: req.eval_only,
            run_id: Some(run_id.clone()),
            save_results: req.save_results.unwrap_or(true),
            review_model: req.review_model.clone(),
            batch_reference: batch_ref.as_ref().map(|b| b.reference()),
            worker,
        };

        let pipe_res = BenchmarkPipeline::execute(
            pipe_config,
            logger.clone(),
            Some(state.cancel_requested.clone()),
        )
        .await;

        match pipe_res {
            Ok(output) => {
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
                } else if total_trials > 1 {
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
                subrun_results.push(output.benchmark_result);
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
    }

    // Any untrusted denominator quarantines the parent: initial + extension
    // batches mint separate BatchRefs, so OR-accumulate drift across all of
    // them. (Gate: initial bracket drifted but extensions clean — the merge
    // kept only the last drift and scored off a known-bad denominator.)
    let mut any_drift = match batch_ref {
        Some(b) => b.finish(&batch_log).await.drift,
        None => false,
    };

    let mut merged_opt = if total_trials > 1 && !subrun_results.is_empty() {
        LeaderboardManager::consolidate_multi_trial_run(
            &parent_run_id,
            &subrun_results,
            req.save_results.unwrap_or(true),
            Some(any_drift),
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
    if dead_cell {
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
    if req.extend_if_unstable && total_trials >= 2 && !dead_cell {
        let max_total = req.max_trials.max(total_trials);
        let effort_setting = req.effort.clone().unwrap_or_else(|| "high".to_string());
        let task_type_ext = match req.task.as_str() {
            "redis" => TaskType::Redis,
            "http" => TaskType::Http,
            "pdf" => TaskType::Pdf,
            "wal" => TaskType::Wal,
            "proxy" => TaskType::Proxy,
            "pubsub" => TaskType::Pubsub,
            "binproto" => TaskType::Binproto,
            _ => TaskType::Redis,
        };
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
                    timeout_min: req.timeout_min.unwrap_or(30),
                    api_key: req.api_key.clone(),
                    eval_only: req.eval_only,
                    save_results: req.save_results.unwrap_or(true),
                    review_model: req.review_model.clone(),
                    parent_run_id: parent_run_id.clone(),
                    start_idx: total_trials + 1,
                    count: add,
                    display_total: total_trials + add,
                },
                worker,
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
