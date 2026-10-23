// Every handler here returns `Result<_, Response>`; `Response` is a large
// axum type by design, so `result_large_err` fires on all of them. Boxing
// the error would change every handler signature and add an allocation on
// the error path to save a `Result` niche that axum never uses.
#![allow(clippy::result_large_err)]

use crate::config::TaskType;
use crate::report::{
    assess_stability, is_hidden, is_poisoned, is_quarantined, set_hidden, set_poisoned,
    BenchmarkRunResult, EnvironmentInfo, FileInfo, LeaderboardManager, PortalExporter, RunArchiver,
    RunManifest, Stability, Store,
};
use crate::sandbox::SandboxManager;
use axum::{
    extract::{Path as AxumPath, Query, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

// Re-exports: queue engine + portal helpers moved to sibling modules in the
// 2026-10-05 split. Downstream paths (`crate::web::server::X`) keep working.
pub use super::portal_proxy::{
    portal_content_type, resolve_portal_dir, serve_portal, serve_portal_index,
};
#[cfg(test)]
pub use super::queue::test_queue_item;
pub use super::queue::{
    claim_slot, db_mirror, db_try, dispatch_ready, execute_extend_body, execute_run_body,
    kick_runner, mark_queue_done, now_rfc3339, park_refused, pause_queue, proxy_mutation,
    proxy_read, pump_queue, push_console_line, push_queue_item, push_worker_line, queue_seq_start,
    reconcile_queue, release_slot, replay_queue, resume_queue, run_extension_batch, send_status,
    spawn_extend_body, spawn_run_body, tempfile_mem_store, ExtBatchOut, ExtBatchSpec,
    ExtendRequest, QueueKind, QueueStatus, QueuedRun, RunRequest, WebLogger, MAX_CONSOLE_LINES,
    MAX_REFUSALS,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveRunInfo {
    pub run_id: String,
    pub model: String,
    pub task: String,
    pub effort: String,
    pub started_at: String,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub max_turns: u32,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub duration_secs: Option<u64>,
    #[serde(default)]
    pub subrun: Option<u32>,
    #[serde(default)]
    pub total_subruns: Option<u32>,
    #[serde(default)]
    pub budget_usd: f64,
    #[serde(default)]
    pub timeout_min: u64,
    #[serde(default)]
    pub trials: u32,
    #[serde(default)]
    pub eval_only: bool,
    #[serde(default)]
    pub extend_if_unstable: bool,
    #[serde(default)]
    pub max_trials: u32,
    #[serde(default)]
    pub review_model: Option<String>,
}

/// Worker slots for parallel trial cells. Default 4, override with
/// `SDB_WORKERS` (clamped 1..=8). `1` reproduces legacy single-runner behavior.
pub fn worker_count() -> usize {
    std::env::var("SDB_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(1, 8)
}

/// Per-worker candidate port offset: base + worker*100 keeps every cell's
/// ports disjoint (candidate, batch-ref, upstream pool).
pub fn worker_port(base: u16, worker: u8) -> u16 {
    base.saturating_add(worker as u16 * 100)
}

#[derive(Clone)]
pub struct AppState {
    pub log_sender: broadcast::Sender<String>,
    pub is_running: Arc<AtomicBool>,
    pub cancel_requested: Arc<AtomicBool>,
    pub current_run: Arc<RwLock<Option<ActiveRunInfo>>>,
    pub last_run: Arc<RwLock<Option<ActiveRunInfo>>>,
    pub log_buffer: Arc<RwLock<Vec<String>>>,
    pub task_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Parallel dispatch: busy worker ids. `is_running` mirrors any-busy for back-compat.
    pub running_slots: Arc<std::sync::Mutex<std::collections::HashSet<u8>>>,
    /// Per-worker live run info (worker -> ActiveRunInfo).
    pub worker_runs: Arc<RwLock<std::collections::HashMap<u8, ActiveRunInfo>>>,
    /// Per-worker console buffers (worker -> lines). Backs the per-worker
    /// console tabs; cleared when the worker slot is claimed fresh.
    pub worker_buffers: Arc<RwLock<std::collections::HashMap<u8, Vec<String>>>>,
    /// Per-worker task handles (worker -> JoinHandle).
    pub worker_handles:
        Arc<std::sync::Mutex<std::collections::HashMap<u8, tokio::task::JoinHandle<()>>>>,
    /// Per-run cancel flags (run_id -> flag). Replaces global-flag poisoning.
    pub cancel_flags: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>>,
    pub queue: Arc<std::sync::Mutex<VecDeque<QueuedRun>>>,
    pub queue_seq: Arc<AtomicU64>,
    /// Billing circuit breaker: when true, pump/kick refuse new runs.
    /// Pending rows stay queued, never deleted. Set by the 402/401 tripwire;
    /// cleared by explicit resume.
    pub queue_paused: Arc<AtomicBool>,
    /// Spent narrow-auto-extend trials this process lifetime. Reserved
    /// upfront per fire against the SDB_AUTO_EXTEND_TRIALS envelope so
    /// concurrent workers can't overshoot it.
    pub auto_extend_spent: Arc<AtomicU64>,
    pub store: Arc<Store>,
    /// Engine base URL in GUI-proxy mode (empty = single-process engine).
    pub engine_url: String,

    /// True when `bench.db` failed to open and the server fell back to a
    /// temp-dir DB (queue durability degraded); surfaced via /api/status.
    pub db_fallback: bool,
}

#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    pub is_running: bool,
    pub current_run: Option<ActiveRunInfo>,
    pub last_run: Option<ActiveRunInfo>,
    /// Per-worker live runs (worker id -> run). Empty when idle.
    #[serde(default)]
    pub workers: std::collections::HashMap<u8, ActiveRunInfo>,
    /// True when the server runs on a temp-dir fallback DB instead of
    /// repo-root bench.db (queue history not durable). Defaults false for
    /// older clients that omit the field.
    #[serde(default)]
    pub db_fallback: bool,
    /// Billing circuit breaker: queue refuses new dispatch until resumed.
    #[serde(default)]
    pub queue_paused: bool,
    /// The live campaign-lock holder (None = no campaign running); the
    /// same fact POST /api/campaigns refuses on (round-7 finding 1).
    #[serde(default)]
    pub campaign: Option<serde_json::Value>,
    /// GUI-proxy mode (engine owns dispatch): /api/stream there is a
    /// one-shot snapshot, so the GUI must use the console poll as the live
    /// path and skip EventSource entirely.
    #[serde(default)]
    pub proxy: bool,
    /// R28 F1/F5: the oracle and cap basis next to the rules version.
    #[serde(default)]
    pub sqlite_version: String,
    #[serde(default)]
    pub cap_price_basis: String,
}

pub(crate) async fn get_status(State(state): State<AppState>) -> Response {
    if let Some(proxied) = proxy_read(&state, "/api/status", "").await {
        return proxied;
    }
    // Read-path ghost sweep: a body that died without finishing leaves a
    // Running row with no worker behind; without this the queue wedges
    // (slots leak, is_running sticks, next wave waits on a dead cell).
    // Live cells always hold a slot, so the sweep cannot touch them.
    reconcile_queue(&state);
    // is_running is DERIVED from live worker state, never the sticky legacy
    // flag: the flag survives bodies that die without releasing (ghost q118
    // reported running:true with workers {} and zero slots on an idle
    // engine). Slots are 1:1 with live bodies (claimed at dispatch, released
    // at every exit), so empty slots + empty workers = idle, full stop.
    // The flag stays as a back-compat mirror only (set on claim, cleared on
    // last release) and must never OR idle state back to running.
    let workers = state.worker_runs.read().unwrap().clone();
    let any_slot = !state.running_slots.lock().unwrap().is_empty();
    let any_handle = !state.worker_handles.lock().unwrap().is_empty();
    let is_running = !workers.is_empty() || any_slot || any_handle;
    let current_run = workers
        .iter()
        .min_by_key(|(w, _)| *w)
        .map(|(_, r)| r.clone())
        .or_else(|| state.current_run.read().unwrap().clone());
    let last_run = state.last_run.read().unwrap().clone();
    Json(StatusResponse {
        is_running,
        current_run,
        last_run,
        workers,
        db_fallback: state.db_fallback,
        queue_paused: state.queue_paused.load(Ordering::SeqCst),
        proxy: !state.engine_url.is_empty(),
        campaign: crate::campaign::live_lock_holder()
            .map(|h| serde_json::json!({"pid": h.pid, "campaign_id": h.campaign_id})),
        sqlite_version: crate::report::rules::sqlite_version().to_string(),
        cap_price_basis: crate::report::rules::CAP_PRICE_BASIS.to_string(),
    })
    .into_response()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SubDollarModel {
    pub id: String,
    pub name: String,
    pub prompt_price_per_m: f64,
    pub completion_price_per_m: f64,
    pub context_length: u64,
    pub created: i64,
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub is_reasoning: bool,
    #[serde(default)]
    pub reasoning_mandatory: bool,
    #[serde(default)]
    pub supports_max_tokens: bool,
}

#[derive(Debug, Deserialize)]
struct OpenRouterReasoning {
    #[serde(default)]
    pub mandatory: Option<bool>,
    #[serde(default)]
    pub default_enabled: Option<bool>,
    #[serde(default)]
    pub supported_efforts: Option<Vec<String>>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub supports_max_tokens: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelItem {
    id: String,
    name: String,
    context_length: Option<u64>,
    pricing: Option<OpenRouterPricing>,
    created: Option<i64>,
    #[serde(default)]
    pub supported_parameters: Option<Vec<String>>,
    #[serde(default)]
    pub reasoning: Option<OpenRouterReasoning>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterPricing {
    prompt: Option<String>,
    completion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterResponse {
    data: Vec<OpenRouterModelItem>,
}

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    pub file: String,
}

#[derive(Debug, Deserialize)]
pub struct CompareQuery {
    pub run_a: String,
    pub run_b: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunComparisonResponse {
    pub run_a: RunManifest,
    pub run_b: RunManifest,
    pub metric_diff: MetricComparison,
    pub file_diffs: Vec<FileDiffEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MetricComparison {
    pub pass_rate_delta: f64,
    pub cost_delta: f64,
    #[serde(default)]
    pub effective_cost_delta: Option<f64>,
    pub tokens_delta: i64,
    pub duration_delta: f64,
    pub throughput_delta: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileDiffEntry {
    pub path: String,
    pub status: String,
    pub lines_a: usize,
    pub lines_b: usize,
    pub diff: String,
    pub content_a: Option<String>,
    pub content_b: Option<String>,
}

pub use crate::report::leaderboard::compute_pass_at_k;

pub fn generate_unified_diff(path: &str, text_a: &str, text_b: &str) -> String {
    let lines_a: Vec<&str> = text_a.lines().collect();
    let lines_b: Vec<&str> = text_b.lines().collect();
    if lines_a == lines_b {
        return String::new();
    }
    let mut out = format!("--- a/{}\n+++ b/{}\n", path, path);
    let mut i = 0;
    let mut j = 0;
    while i < lines_a.len() || j < lines_b.len() {
        if i < lines_a.len() && j < lines_b.len() {
            if lines_a[i] == lines_b[j] {
                out.push_str(&format!(" {}\n", lines_a[i]));
                i += 1;
                j += 1;
            } else {
                let mut found_match = false;
                for lookahead in 1..=5 {
                    if j + lookahead < lines_b.len() && lines_a[i] == lines_b[j + lookahead] {
                        for k in 0..lookahead {
                            out.push_str(&format!("+{}\n", lines_b[j + k]));
                        }
                        j += lookahead;
                        found_match = true;
                        break;
                    } else if i + lookahead < lines_a.len() && lines_a[i + lookahead] == lines_b[j]
                    {
                        for k in 0..lookahead {
                            out.push_str(&format!("-{}\n", lines_a[i + k]));
                        }
                        i += lookahead;
                        found_match = true;
                        break;
                    }
                }
                if !found_match {
                    out.push_str(&format!("-{}\n", lines_a[i]));
                    out.push_str(&format!("+{}\n", lines_b[j]));
                    i += 1;
                    j += 1;
                }
            }
        } else if i < lines_a.len() {
            out.push_str(&format!("-{}\n", lines_a[i]));
            i += 1;
        } else {
            out.push_str(&format!("+{}\n", lines_b[j]));
            j += 1;
        }
    }
    out
}

fn validate_task_name(task: &str) -> Result<&str, StatusCode> {
    if task.is_empty()
        || task.contains('/')
        || task.contains('\\')
        || task.contains("..")
        || !task
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(task)
}

fn resolve_task_path(task: &str) -> PathBuf {
    crate::config::get_repo_root()
        .join("tasks")
        .join(task)
        .join("prompt.md")
}

pub(crate) fn resolve_results_dir() -> PathBuf {
    crate::config::get_repo_root().join("results")
}

#[allow(dead_code)]
fn resolve_workspace_dir() -> PathBuf {
    let ws = crate::config::get_repo_root().join("workspace");
    let _ = std::fs::create_dir_all(&ws);
    ws
}

/// Per-trial workdir: `workspace/<run_id>`. Parallel workers must never
/// share one bind mount: pipeline wipes workdir at trial start
/// (remove_dir_all), so a shared dir lets a sibling delete a live mount
/// (ENOENT) and cross-contaminates candidate code across tasks.
pub fn workdir_for_run(run_id: &str) -> PathBuf {
    let ws = crate::config::get_repo_root()
        .join("workspace")
        .join(run_id);
    let _ = std::fs::create_dir_all(&ws);
    ws
}

pub struct UiServer;

/// How many times `UiServer::start` got past a successful bind (round-11
/// O1's test: a failed bind must leave this unchanged — no queue pump, no
/// reconcile, no OMP updater).
pub static UI_STARTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Best-effort lookup of the process holding a TCP listen port, for the
/// "cannot bind" message (round-11 O1). Reads /proc/net/tcp{,6} for the
/// listening socket inode, then scans /proc/*/fd for that inode. Returns
/// None when /proc is unavailable or the holder is not visible.
pub fn port_holder(port: u16) -> Option<u32> {
    let mut inodes: Vec<String> = Vec::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 || cols[3] != "0A" {
                continue; // 0A = LISTEN
            }
            let Some((_, hex_port)) = cols[1].rsplit_once(':') else {
                continue;
            };
            if u16::from_str_radix(hex_port, 16) != Ok(port) {
                continue;
            }
            inodes.push(cols[9].to_string());
        }
    }
    if inodes.is_empty() {
        return None;
    }
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                let target = link.to_string_lossy();
                if let Some(rest) = target.strip_prefix("socket:[") {
                    let ino = rest.trim_end_matches(']');
                    if inodes.iter().any(|i| i == ino) {
                        return Some(pid);
                    }
                }
            }
        }
    }
    None
}

impl UiServer {
    pub fn build_app(state: AppState) -> Router {
        super::routes::build_app(state)
    }

    pub async fn start(host: &str, port: u16, engine_url: &str) -> anyhow::Result<()> {
        // Round-11 O1: bind before every side effect. A second `ui` on a
        // taken port used to run the queue pump, the startup reconcile and
        // the OMP updater every 3 s in a systemd crash loop (~19k times
        // each). A held port now exits here with who holds it.
        let addr = format!("{host}:{port}");
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                let holder = port_holder(port)
                    .map(|p| format!(" (held by pid {p})"))
                    .unwrap_or_default();
                anyhow::bail!("cannot bind {addr}: {e}{holder}");
            }
        };
        UI_STARTS.fetch_add(1, Ordering::SeqCst);
        let repo = crate::config::get_repo_root();
        if repo.exists() {
            let _ = std::env::set_current_dir(&repo);
        }
        let (log_sender, _) = broadcast::channel(4096);
        let db_path = Store::resolve_db_path();
        let (store, db_fallback) = match Store::open(&db_path) {
            Ok(store) => (Arc::new(store), false),
            Err(e) => {
                tracing::warn!(
                    "bench.db open {} failed ({e:#}); falling back to temp-dir DB — queue history NOT durable",
                    db_path.display()
                );
                eprintln!(
                    "[QUEUE-DB] open {} failed ({e:#}); falling back to temp-dir DB",
                    db_path.display()
                );
                let fallback = std::env::temp_dir()
                    .join(format!("sdb-queue-fallback-{}.db", std::process::id()));
                match Store::open(&fallback) {
                    Ok(store) => {
                        tracing::warn!(
                            "using temp fallback DB {}; queue history will not survive restarts",
                            fallback.display()
                        );
                        (Arc::new(store), true)
                    }
                    Err(e2) => {
                        tracing::warn!(
                            "temp fallback DB open failed ({e2:#}); last-resort temp store — queue history NOT durable"
                        );
                        let mem = tempfile_mem_store();
                        (Arc::new(mem), true)
                    }
                }
            }
        };
        // Proxy mode: the engine owns dispatch. Never replay, never reset
        // Running rows, never auto-kick — otherwise this process would
        // double-spawn the engine's active cell.
        let proxy_mode = !engine_url.is_empty();
        let replayed: Vec<QueuedRun> = if proxy_mode {
            println!("[QUEUE-DB] proxy mode: queue owned by engine, no replay");
            Vec::new()
        } else {
            replay_queue(&store)
        };
        let seq_start = queue_seq_start(&store);
        // Reset replayed Running rows to Pending in the mirror too, so a
        // second restart does not see stale Running rows.
        for item in &replayed {
            if item.status == QueueStatus::Pending {
                let _ = store.update_queue_status(&item.id, "Pending", None, None);
            }
        }
        if !replayed.is_empty() {
            println!(
                "[QUEUE-DB] replayed {} pending item(s) from bench.db",
                replayed.len()
            );
        }
        // Boot integrity audit (warn-only; `migrate-check` exits 1 for CI).
        {
            let orphans = store.orphan_trial_count();
            if orphans > 0 {
                tracing::warn!("integrity: {orphans} orphan trial(s); run `migrate-check`");
                eprintln!("[INTEGRITY] {orphans} orphan trial(s); run `migrate-check`");
            }
            let junk = store.junk_parent_count();
            if junk > 0 {
                tracing::warn!("integrity: {junk} junk parent row(s); run `migrate-check`");
                eprintln!("[INTEGRITY] {junk} junk parent row(s); run `migrate-check`");
            }
        }
        let state = AppState {
            log_sender,
            is_running: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            current_run: Arc::new(RwLock::new(None)),
            last_run: Arc::new(RwLock::new(None)),
            log_buffer: Arc::new(RwLock::new(Vec::new())),
            task_handle: Arc::new(std::sync::Mutex::new(None)),
            queue: Arc::new(std::sync::Mutex::new(VecDeque::from(replayed))),
            queue_seq: Arc::new(AtomicU64::new(seq_start)),
            queue_paused: Arc::new(AtomicBool::new(false)),
            auto_extend_spent: Arc::new(AtomicU64::new(0)),
            running_slots: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            worker_runs: Arc::new(RwLock::new(std::collections::HashMap::new())),
            worker_buffers: Arc::new(RwLock::new(std::collections::HashMap::new())),
            worker_handles: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            cancel_flags: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            store,
            db_fallback,
            engine_url: engine_url.to_string(),
        };

        // Replay auto-dispatch (engine only; proxy never kicks).
        // Pending work that survived the restart runs without a manual kick.
        // Fan out to every free slot: a single kick leaves SDB_WORKERS-1
        // slots idle until the first completion (the wave then runs serially
        // for a full trial before parallelizing). pump_queue claims one slot
        // per call and returns false when full or dry, so loop until it stops.
        if !proxy_mode {
            let kick_state = state.clone();
            tokio::spawn(async move { while pump_queue(&kick_state) {} });
        }

        // Periodic reconcile (60s): queue ghosts always; docker sweeps only
        // when the engine is idle (same engine_busy rule as reconcile_queue).
        // A mid-trial candidate legitimately stops for seconds in its kill -9
        // window with its network present; sweeping then rm -f's it and
        // `docker start` fails (same murder class as the batchref kills).
        // Startup sweep stays ungated (engine provably idle at boot).
        {
            let sweep = state.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    // Queue ghosts are memory-only flips: always safe.
                    let ghosts = reconcile_queue(&sweep);
                    let busy = !sweep.worker_runs.read().unwrap().is_empty()
                        || !sweep.worker_handles.lock().unwrap().is_empty()
                        || !sweep.running_slots.lock().unwrap().is_empty();
                    if busy {
                        continue;
                    }
                    let orphans =
                        crate::sandbox::docker::SandboxManager::reap_orphan_containers_except(
                            &live_run_ids(&sweep),
                        );
                    crate::sandbox::docker::SandboxManager::prune_labeled_networks();
                    if ghosts + orphans > 0 {
                        send_status(
                            &sweep.log_sender,
                            &sweep.log_buffer,
                            format!(
                                "[RECONCILE] periodic sweep: {ghosts} ghost(s), {orphans} orphan container(s) reaped"
                            ),
                        );
                    }
                    // Idle engine with ready work must not sit still: backoff
                    // expiry (parked refusals) and replayed pendings have no
                    // other trigger. pump claims one slot per call; the loop
                    // fills every free worker, no-op when dry or paused.
                    if !sweep.queue_paused.load(Ordering::SeqCst) {
                        while pump_queue(&sweep) {}
                    }
                }
            });
        }

        let app = Self::build_app(state);

        /// Run ids that must never be reaped: dispatched worker runs plus queued
        /// parent ids (cancel flags are keyed by parent). Trial, parent, and
        /// `<id>_batchref` labels all stem-match these (see run_matches_live).
        pub(crate) fn live_run_ids(state: &AppState) -> std::collections::HashSet<String> {
            let mut ids = std::collections::HashSet::new();
            for info in state.worker_runs.read().unwrap().values() {
                ids.insert(info.run_id.clone());
            }
            for key in state.cancel_flags.lock().unwrap().keys() {
                ids.insert(key.clone());
            }
            ids
        }

        // Startup reconcile: label-filtered docker sweep (orphan containers
        // + stale labeled nets) BEFORE auto-dispatch replays the queue, so a
        // restarted engine never binds a port held by its own corpse.
        // prune_stale_networks (name-filter, age-gated) stays as belt for
        // pre-label legacy nets; prune_labeled_networks covers labeled ones.
        {
            // Fresh process: nothing can be live here (any containers belong
            // to the dead previous engine), so the empty live set lets the
            // age gate reap ancient running orphans too. Restart-while-old-
            // still-dying is serialized by systemd stop-before-start.
            let stale_containers =
                crate::sandbox::docker::SandboxManager::reap_orphan_containers_except(
                    &std::collections::HashSet::new(),
                );
            if stale_containers > 0 {
                println!("[RECONCILE] reaped {stale_containers} orphan container(s) at startup");
            }
            crate::sandbox::docker::SandboxManager::prune_labeled_networks();
            crate::sandbox::docker::SandboxManager::prune_stale_networks();
        }
        // Background check and auto-update of sandbox OMP on server startup
        tokio::spawn(async {
            if let Err(e) = crate::sandbox::omp_updater::auto_update_if_needed().await {
                eprintln!("[OMP Startup Check Warning] {}", e);
            }
        });

        let addr = format!("{}:{}", host, port);
        println!("🚀 SubDollarBench GUI listening on: http://{}", addr);
        if host == "0.0.0.0" || host == "127.0.0.1" {
            println!("   Local URL:  http://localhost:{}", port);
        }
        if host == "0.0.0.0"
            && std::env::var("SDB_UI_TOKEN")
                .ok()
                .filter(|t| !t.trim().is_empty())
                .is_none()
            && login_creds().is_none()
        {
            eprintln!(
                "[SECURITY] binding 0.0.0.0 with no SDB_UI_TOKEN and no SDB_UI_USER/SDB_UI_PASS: \
                 mutating endpoints are CSRF-only. Set SDB_UI_TOKEN (and SDB_UI_USER/SDB_UI_PASS) in .env (0600)."
            );
        }

        axum::serve(listener, app).await?;

        Ok(())
    }
}

/// Shared leaf JS (single source with the static portal, which inlines
/// the same file at export). Served as its own route so the GUI keeps
/// working when the page is cached; the portal inlines it instead.
pub(crate) async fn serve_shared_js() -> Response {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        include_str!("shared.js"),
    )
        .into_response()
}

static INDEX_HTML: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// GUI shell with shared helpers inlined (offline single-file preserved).
/// Shared fns live only in `shared.js`; `index.html` carries a placeholder.
pub(crate) async fn serve_index() -> Html<String> {
    let page = INDEX_HTML.get_or_init(|| {
        include_str!("index.html")
            .replace(
                "<!--__SHARED_JS__-->",
                &format!("<script>\n{}\n</script>", include_str!("shared.js")),
            )
            .replace(
                "<!--__RULES__-->",
                &format!(
                    "<script>window.__SDB_RULES__ = {}; window.__SDB_SQLITE__ = {}; window.__SDB_CAP_BASIS__ = {};</script>",
                    serde_json::to_string(crate::report::rules::RULES_VERSION)
                        .unwrap_or_default(),
                    serde_json::to_string(crate::report::rules::sqlite_version())
                        .unwrap_or_default(),
                    serde_json::to_string(crate::report::rules::CAP_PRICE_BASIS)
                        .unwrap_or_default()
                ),
            )
    });
    Html(page.clone())
}

pub(crate) async fn get_models(headers: HeaderMap) -> Json<Vec<SubDollarModel>> {
    // API key precedence: Authorization/x-api-key header, else server env.
    // Query-param `?key=` is deliberately unsupported: URLs land in
    // access logs, history, and CDN caches.
    let header_key = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .map(|h| {
            if let Some(tok) = h.strip_prefix("Bearer ") {
                tok
            } else if let Some(tok) = h.strip_prefix("bearer ") {
                tok
            } else {
                h
            }
        })
        .map(|s| s.trim().to_string())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|h| h.to_str().ok())
                .map(|s| s.trim().to_string())
        });

    let api_key = header_key.filter(|k| !k.is_empty()).or_else(|| {
        std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let mut req = client.get("https://openrouter.ai/api/v1/models");
    if let Some(ref key) = api_key {
        if !key.trim().is_empty() {
            req = req.header("Authorization", format!("Bearer {}", key.trim()));
        }
    }

    let mut models = Vec::new();
    if let Ok(resp) = req.send().await {
        if let Ok(data) = resp.json::<OpenRouterResponse>().await {
            models = parse_openrouter_models(data);
        }
    }

    Json(models)
}

/// Pure OpenRouter payload -> under-$1 listing mapping (pricing filter,
/// reasoning metadata, meta/ "max" sanitization, cheapest-first sort).
/// Split from the network call so every rule is testable without an API
/// key or egress.
fn parse_openrouter_models(data: OpenRouterResponse) -> Vec<SubDollarModel> {
    let mut models = Vec::new();
    for item in data.data {
        // Skip Batch API variants: queued, minutes-to-hours latency,
        // unusable for interactive agentic runs.
        if item.id.ends_with(":batch") {
            continue;
        }
        let prompt_p = item
            .pricing
            .as_ref()
            .and_then(|p| p.prompt.as_ref())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            * 1_000_000.0;

        let comp_p = item
            .pricing
            .as_ref()
            .and_then(|p| p.completion.as_ref())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            * 1_000_000.0;

        let supported_params = item.supported_parameters.as_ref();
        let has_reasoning_param = supported_params
            .map(|params| {
                params
                    .iter()
                    .any(|p| p == "reasoning" || p == "reasoning_effort")
            })
            .unwrap_or(false);

        let reasoning_obj = item.reasoning.as_ref();
        let mut supported_efforts = reasoning_obj
            .and_then(|r| r.supported_efforts.clone())
            .unwrap_or_default();

        // Upstream providers (like Meta) reject "max" with 400 invalid_request_error.
        // Sanitize: clamp out "max" if upstream endpoint strictly forbids it
        if item.id.starts_with("meta/") {
            supported_efforts.retain(|e| e != "max");
        }

        let default_effort = reasoning_obj.and_then(|r| r.default_effort.clone());
        let supports_max_tokens = reasoning_obj
            .and_then(|r| r.supports_max_tokens)
            .unwrap_or(false);

        let reasoning_mandatory = reasoning_obj.and_then(|r| r.mandatory).unwrap_or(false);

        let is_reasoning = has_reasoning_param
            || !supported_efforts.is_empty()
            || supports_max_tokens
            || reasoning_mandatory
            || reasoning_obj
                .and_then(|r| r.default_enabled)
                .unwrap_or(false);

        // Free ($0/$0) models are the cheapest possible and must list:
        // distinguish explicitly-free (pricing present, both zero)
        // from unpriced (no pricing at all — still hidden).
        let has_pricing = item
            .pricing
            .as_ref()
            .is_some_and(|p| p.prompt.is_some() || p.completion.is_some());
        if (prompt_p + comp_p) / 2.0 <= 1.0 && ((prompt_p > 0.0 || comp_p > 0.0) || has_pricing) {
            models.push(SubDollarModel {
                id: item.id,
                name: item.name,
                prompt_price_per_m: prompt_p,
                completion_price_per_m: comp_p,
                context_length: item.context_length.unwrap_or(0),
                created: item.created.unwrap_or(0),
                supported_efforts,
                default_effort,
                is_reasoning,
                reasoning_mandatory,
                supports_max_tokens,
            });
        }
    }

    models.sort_by(|a, b| {
        ((a.prompt_price_per_m + a.completion_price_per_m) / 2.0)
            .partial_cmp(&((b.prompt_price_per_m + b.completion_price_per_m) / 2.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    models
}

pub(crate) async fn get_prompt(AxumPath(task): AxumPath<String>) -> Result<String, StatusCode> {
    validate_task_name(&task)?;
    let path = resolve_task_path(&task);
    Ok(fs::read_to_string(path).unwrap_or_else(|_| "# Task prompt not found".to_string()))
}

pub(crate) async fn save_prompt(
    AxumPath(task): AxumPath<String>,
    body: String,
) -> Result<String, StatusCode> {
    validate_task_name(&task)?;
    // Test guard: the lifecycle test POSTs tasks/test_task/prompt.md as its
    // write probe. Unknown tasks must not sprout directories in the repo's
    // tasks/ tree — only known tasks are editable via the prompt editor.
    if crate::config::TaskType::from_name(&task).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let path = resolve_task_path(&task);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(path, body) {
        // Graded input changed: the caller must bump RULES_VERSION so the
        // fingerprint stamp moves and cross-regime ranks stay gated.
        Ok(_) => {
            eprintln!(
                "[PROMPT] tasks/{}/prompt.md overwritten via API — bump RULES_VERSION in src/report/rules.rs",
                task
            );
            Ok(
                "Prompt saved — bump RULES_VERSION in src/report/rules.rs to re-stamp grading"
                    .to_string(),
            )
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Deserialize)]
pub struct LeaderboardQuery {
    pub task: Option<String>,
}

pub(crate) async fn get_leaderboard(
    Query(query): Query<LeaderboardQuery>,
) -> Json<Vec<serde_json::Value>> {
    let r_dir = resolve_results_dir();
    let mut list = LeaderboardManager::load_all(r_dir.to_str().unwrap_or("./results"));
    // Quarantined tasks are archived, not published: the default API
    // omits them; an explicit ?task=<name> still renders those rows.
    let want_quarantined = query
        .task
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty() && !t.eq_ignore_ascii_case("all"));
    if !want_quarantined {
        list.retain(|r| !is_quarantined(&r.task));
    }
    if let Some(task_filter) = query.task {
        let trimmed = task_filter.trim();
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("all") {
            list.retain(|r| r.task.eq_ignore_ascii_case(trimmed));
        }
    }
    let runs_dir = RunArchiver::resolve_runs_dir();
    Json(
        list.into_iter()
            .map(|r| {
                let mut v = serde_json::to_value(&r).unwrap_or(serde_json::Value::Null);
                v["portal_hidden"] = serde_json::Value::Bool(is_hidden(&runs_dir, &r.id));
                v["poisoned"] = serde_json::Value::Bool(is_poisoned(&runs_dir, &r.id));
                v
            })
            .collect(),
    )
}

/// Refresh the PUBLIC portal: models summary only (no per-run pages).
/// Per-run detail lives in the GUI (Runs + Leaderboard tabs), never on disk here.
fn refresh_portal() {
    // Round-12 P1: one resolver for every writer (SDB_PORTAL_DIR-aware).
    let out = crate::report::archive::RunArchiver::resolve_portal_dir();
    // The public board is the sdb-sql campaign only: the pre-sdb-sql tasks
    // are retired (2026-10-17) and their rollups no longer publish.
    let runs_dir = crate::report::archive::RunArchiver::resolve_runs_dir();
    let _ = PortalExporter::export_sql_board(
        runs_dir.to_str().unwrap_or("./runs"),
        out.to_str().unwrap_or("./portal"),
    );
    // The board writes index.html only; drop stale per-run artifacts so
    // the public surface never serves run detail. A silent failure here
    // left a root-owned `runs/` tree serving legacy pages (2026-10-17):
    // log it instead.
    match std::fs::remove_dir_all(out.join("runs")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("[PORTAL] stale portal/runs/ not removed: {e}"),
    }
    let _ = std::fs::remove_file(out.join("results.json"));
}

/// Same-origin guard for mutating endpoints. Browser navigations always
/// attach Origin (or Referer for same-origin form/DELETE submissions);
/// curl/CLI send neither and stay allowed. Allowed when:
/// (1) no Origin/Referer present, (2) Origin/Referer host is loopback, or
/// (3) Origin/Referer host matches the Host (or X-Forwarded-Host) header,
/// i.e. a legitimate same-origin fetch from a LAN IP, Tailnet name, or
/// custom domain. Anything else is rejected as cross-site.
fn origin_host_from_url(url: &str) -> String {
    let authority = url
        .split("://")
        .last()
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .trim();
    host_without_port(authority)
}

/// Host header (or X-Forwarded-Host entry) normalized for comparison:
/// lowercased, port stripped, brackets stripped from IPv6 literals.
fn host_without_port(authority: &str) -> String {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[') {
        // [::1] or [::1]:3000 -> ::1
        if let Some(end) = rest.find(']') {
            return rest[..end].to_ascii_lowercase();
        }
    }
    a.split(':')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase()
}

fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "localhost" || host == "::1"
}

fn csrf_origin_allowed(headers: &HeaderMap) -> bool {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let referer = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let candidate = if !origin.is_empty() { origin } else { referer };
    if candidate.is_empty() {
        return true;
    }
    let origin_host = origin_host_from_url(candidate);
    if origin_host.is_empty() {
        return false;
    }
    if is_loopback_host(&origin_host) {
        return true;
    }
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if host_without_port(host) == origin_host {
            return true;
        }
    }
    // Reverse-proxy deployments (Traefik): Host may be internal while the
    // browser's Origin carries the external name. Trust the forwarded host
    // only for an exact same-origin match, never as a bypass.
    if let Some(xfh) = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
    {
        for entry in xfh.split(',') {
            if !entry.trim().is_empty() && host_without_port(entry) == origin_host {
                return true;
            }
        }
    }
    false
}

fn csrf_reject() -> Response {
    (
        axum::http::StatusCode::FORBIDDEN,
        "Cross-origin request rejected (Origin/Referer check)",
    )
        .into_response()
}

/// Axum layer guarding every mutating endpoint (POST/PUT/PATCH/DELETE).
/// Two gates, in order: (1) bearer token when `SDB_UI_TOKEN` is set —
/// `Authorization: Bearer <token>` must match, no Origin bypass; (2) the
/// CSRF Origin/Referer check below. Reads stay open. When no token is
/// configured the layer is CSRF-only (documented insecure default for
/// local dev); production sets the token in `.env` (0600, like the API key).
fn ui_token() -> Option<String> {
    std::env::var("SDB_UI_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
}

fn token_ok(headers: &HeaderMap, token: &str) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let presented = bearer.strip_prefix("Bearer ").unwrap_or("").trim();
    !presented.is_empty() && subtle_eq(presented, token)
}

/// Constant-time compare so a local timing probe cannot walk the token.
fn subtle_eq(a: &str, b: &str) -> bool {
    // No early length return: fold over the longer side with zero-padding so
    // password/user length is not observable via timing (LAN oracle otherwise).
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let n = ab.len().max(bb.len());
    let mut acc = (ab.len() ^ bb.len()) as u8;
    for i in 0..n {
        let x = *ab.get(i).unwrap_or(&0);
        let y = *bb.get(i).unwrap_or(&0);
        acc |= x ^ y;
    }
    acc == 0
}

/// Password envs for the GUI login. Both must be set; unset = login disabled
/// (bearer/CSRF behavior unchanged). No defaults: no `admin/admin` footgun.
fn login_creds() -> Option<(String, String)> {
    let u = std::env::var("SDB_UI_USER")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let p = std::env::var("SDB_UI_PASS")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    Some((u, p))
}

fn session_set() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

fn session_cookie(headers: &HeaderMap) -> bool {
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Ok(set) = session_set().lock() else {
        return false;
    };
    for part in cookies.split(';') {
        let part = part.trim();
        if let Some(tok) = part.strip_prefix("sdb_session=") {
            if set.contains(tok.trim()) {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub user: String,
    pub pass: String,
}

/// POST /api/login {user, pass}: 204 + HttpOnly session cookie on success,
/// 401 otherwise. Rate-limiting: none (LAN tool); wrong passwords log.
pub(crate) async fn login(
    State(_state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Response {
    let Some((eu, ep)) = login_creds() else {
        return (
            StatusCode::UNAUTHORIZED,
            "login disabled (set SDB_UI_USER + SDB_UI_PASS)",
        )
            .into_response();
    };
    if !subtle_eq(body.user.trim(), eu.trim()) || !subtle_eq(&body.pass, &ep) {
        eprintln!("[LOGIN] failed attempt for user {:?}", body.user);
        return (StatusCode::UNAUTHORIZED, "bad credentials").into_response();
    }
    let tok: String = (0..32)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    if let Ok(mut set) = session_set().lock() {
        set.insert(tok.clone());
    }
    (
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            format!("sdb_session={tok}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"),
        )],
    )
        .into_response()
}

/// POST /api/logout: drop the session cookie server-side + clear client.
pub(crate) async fn logout(State(_state): State<AppState>, headers: HeaderMap) -> Response {
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Ok(mut set) = session_set().lock() {
        for part in cookies.split(';') {
            if let Some(tok) = part.trim().strip_prefix("sdb_session=") {
                set.remove(tok.trim());
            }
        }
    }
    (
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            "sdb_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
    )
        .into_response()
}

/// GET /api/me: 200 {login: true, user} when authed (session or bearer),
/// 401 otherwise. Lets the GUI render login form vs app without a probe POST.
pub(crate) async fn me(headers: HeaderMap) -> Response {
    let authed =
        session_cookie(&headers) || ui_token().map(|t| token_ok(&headers, &t)).unwrap_or(false);
    let login_enabled = login_creds().is_some();
    if !authed {
        return Json(serde_json::json!({"login": false, "login_required": login_enabled}))
            .into_response();
    }
    let user = std::env::var("SDB_UI_USER").unwrap_or_default();
    Json(serde_json::json!({"login": true, "user": user, "login_required": login_enabled}))
        .into_response()
}

pub(crate) async fn csrf_guard(req: Request, next: middleware::Next) -> Response {
    // Auth endpoints bootstrap trust: /api/login cannot present a session or
    // bearer by definition (it mints them), but CSRF origin verification
    // must still protect /api/login and /api/logout from cross-origin forging.
    let path = req.uri().path().to_string();
    let is_auth_bootstrap = path == "/api/login" || path == "/api/logout" || path == "/api/me";
    let mutating = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if mutating {
        if !csrf_origin_allowed(req.headers()) {
            return csrf_reject();
        }
        if !is_auth_bootstrap {
            // Session cookie (login) satisfies the token gate: the cookie is
            // HttpOnly + SameSite=Strict, so unlike a readable token it cannot be
            // exfiltrated by page JS — the server holds the secret, not the browser.
            // Bearer still works (CLI + old flows); login disabled = old behavior.
            let authed = session_cookie(req.headers())
                || match ui_token() {
                    Some(token) => token_ok(req.headers(), &token),
                    None => true,
                };
            if !authed {
                // Login configured but neither session nor bearer: point at login.
                if login_creds().is_some() {
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        "login required (POST /api/login)",
                    )
                        .into_response();
                }
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Missing or invalid UI token (Authorization: Bearer <SDB_UI_TOKEN>)",
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}

pub(crate) fn clean_run_id(run_id: &str) -> Option<String> {
    let name = Path::new(run_id).file_name()?.to_str()?;
    if name.is_empty() || name == "." || name != run_id {
        return None;
    }
    Some(name.to_string())
}

fn manifest_with_portal_flag(m: &RunManifest) -> serde_json::Value {
    let mut v = serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
    v["portal_hidden"] =
        serde_json::Value::Bool(is_hidden(&RunArchiver::resolve_runs_dir(), &m.run_id));
    v["poisoned"] =
        serde_json::Value::Bool(is_poisoned(&RunArchiver::resolve_runs_dir(), &m.run_id));
    v
}

pub(crate) async fn get_run_review(AxumPath(run_id): AxumPath<String>) -> Response {
    let clean = match clean_run_id(&run_id) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "invalid run id").into_response(),
    };
    let path = RunArchiver::resolve_runs_dir()
        .join(clean)
        .join("review.json");
    match std::fs::read(&path) {
        Ok(bytes) => ([("content-type", "application/json")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "review not found").into_response(),
    }
}
pub(crate) async fn delete_run(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<String>,
) -> Response {
    let clean = match clean_run_id(&run_id) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "invalid run id").into_response(),
    };
    let runs_dir = RunArchiver::resolve_runs_dir();
    let results_dir = resolve_results_dir();
    let run_path = runs_dir.join(&clean);
    let res_path = results_dir.join(format!("{}.json", clean));
    if !run_path.join("manifest.json").exists() && !res_path.exists() {
        return (StatusCode::NOT_FOUND, "run not found").into_response();
    }
    if run_path.exists() {
        if let Err(e) = std::fs::remove_dir_all(&run_path) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to remove run directory: {}", e),
            )
                .into_response();
        }
    }
    if res_path.exists() {
        if let Err(e) = std::fs::remove_file(&res_path) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to remove result entry: {}", e),
            )
                .into_response();
        }
    }
    // Best-effort DB cleanup; FS stays authoritative so failures are logged, never fatal.
    if let Err(e) = state.store.delete_parent(&clean) {
        eprintln!("[DELETE-RUN] delete_parent {}: {:#}", clean, e);
    }
    if let Err(e) = state.store.delete_run_manifest(&clean) {
        eprintln!("[DELETE-RUN] delete_run_manifest {}: {:#}", clean, e);
    }
    refresh_portal();
    Json(serde_json::json!({"deleted": clean})).into_response()
}

#[derive(Debug, Deserialize)]
pub struct PortalVisibilityRequest {
    pub hidden: bool,
}

#[derive(Debug, Deserialize)]
pub struct PoisonRequest {
    pub poisoned: bool,
}

pub(crate) async fn set_run_poison(
    AxumPath(run_id): AxumPath<String>,
    Json(payload): Json<PoisonRequest>,
) -> Response {
    let clean = match clean_run_id(&run_id) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "invalid run id").into_response(),
    };
    match set_poisoned(&RunArchiver::resolve_runs_dir(), &clean, payload.poisoned) {
        Ok(poisoned) => {
            refresh_portal();
            Json(serde_json::json!({"run_id": clean, "poisoned": poisoned})).into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, format!("cannot update run: {}", e)).into_response(),
    }
}

pub(crate) async fn set_run_portal_visibility(
    AxumPath(run_id): AxumPath<String>,
    Json(payload): Json<PortalVisibilityRequest>,
) -> Response {
    let clean = match clean_run_id(&run_id) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "invalid run id").into_response(),
    };
    match set_hidden(&RunArchiver::resolve_runs_dir(), &clean, payload.hidden) {
        Ok(hidden) => {
            refresh_portal();
            Json(serde_json::json!({"run_id": clean, "portal_hidden": hidden})).into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, format!("cannot update run: {}", e)).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct CampaignFlagRequest {
    /// `poisoned` for /poison, `hidden` for /portal.
    #[serde(alias = "poisoned", alias = "hidden")]
    pub on: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

async fn set_campaign_flag_response(
    campaign_id: String,
    flag_file: &'static str,
    payload: CampaignFlagRequest,
) -> Response {
    let clean = match clean_run_id(&campaign_id) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "invalid campaign id").into_response(),
    };
    match crate::report::portal::set_campaign_flag(
        &RunArchiver::resolve_runs_dir(),
        &clean,
        flag_file,
        payload.on,
        payload.reason.as_deref(),
    ) {
        Ok(on) => {
            refresh_portal();
            let key = if flag_file == crate::report::portal::POISONED_FILE {
                "poisoned"
            } else {
                "portal_hidden"
            };
            Json(serde_json::json!({"campaign_id": clean, key: on})).into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            format!("cannot update campaign: {e}"),
        )
            .into_response(),
    }
}

/// Round-14 N1: poison a campaign cell (a non-result: excluded from the
/// portal and the console board). Body: `{"poisoned": true, "reason": "…"}`.
pub(crate) async fn set_campaign_poison(
    AxumPath(campaign_id): AxumPath<String>,
    Json(payload): Json<CampaignFlagRequest>,
) -> Response {
    set_campaign_flag_response(campaign_id, crate::report::portal::POISONED_FILE, payload).await
}

/// Round-14 N1: hide a campaign cell from both boards without calling it a
/// non-result. Body: `{"hidden": true, "reason": "…"}`.
pub(crate) async fn set_campaign_portal_visibility(
    AxumPath(campaign_id): AxumPath<String>,
    Json(payload): Json<CampaignFlagRequest>,
) -> Response {
    set_campaign_flag_response(
        campaign_id,
        crate::report::portal::PORTAL_HIDDEN_FILE,
        payload,
    )
    .await
}

pub(crate) async fn rebuild_portal() -> Response {
    // Public portal is models-summary-only (rollup + prune stale per-run pages).
    refresh_portal();
    let idx = crate::config::get_repo_root()
        .join("portal")
        .join("index.html");
    if idx.exists() {
        Json(serde_json::json!({"ok": true, "rollup": true})).into_response()
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "portal export failed: missing index.html",
        )
            .into_response()
    }
}

/// Machine-readable trust summary: store-level orphan/junk counts plus
/// live-queue refusal signals. `ok` is false when anything needs a human.
/// Polled by the GUI header badge; same numbers as `migrate-check`.
#[derive(Serialize)]
pub(crate) struct IntegrityResponse {
    pub ok: bool,
    pub orphans: usize,
    pub junk_parents: usize,
    pub refused_final: usize,
    pub pending_refused: usize,
}

pub(crate) async fn get_integrity(State(state): State<AppState>) -> Json<IntegrityResponse> {
    let orphans = state.store.orphan_trial_count();
    let junk = state.store.junk_parent_count();
    let q = state.queue.lock().unwrap();
    let refused_final = q
        .iter()
        .filter(|i| {
            i.status == QueueStatus::Done
                && i.note
                    .as_deref()
                    .is_some_and(|n| n.starts_with("refused-final"))
        })
        .count();
    let pending_refused = q
        .iter()
        .filter(|i| {
            i.status == QueueStatus::Pending
                && i.note.as_deref().is_some_and(|n| n.starts_with("refused"))
        })
        .count();
    Json(IntegrityResponse {
        ok: orphans == 0 && junk == 0 && refused_final == 0,
        orphans,
        junk_parents: junk,
        refused_final,
        pending_refused,
    })
}

pub(crate) async fn get_runs(State(state): State<AppState>) -> Json<Vec<serde_json::Value>> {
    // DB-first (same pattern as load_all); FS fallback until migration.
    let from_db = state.store.load_run_manifests();
    let list = if from_db.is_empty() {
        RunArchiver::list_runs(&RunArchiver::resolve_runs_dir())
    } else {
        from_db
    };
    Json(list.iter().map(manifest_with_portal_flag).collect())
}

pub(crate) async fn get_run_detail(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, Response> {
    // DB-first with filesystem fallback (same pattern as load_all).
    if let Some(m) = state.store.get_run_manifest(&run_id) {
        return Ok(Json(manifest_with_portal_flag(&m)));
    }
    let r_dir = RunArchiver::resolve_runs_dir();
    match RunArchiver::get_run(&r_dir, &run_id) {
        Some(m) => Ok(Json(manifest_with_portal_flag(&m))),
        None => Err((axum::http::StatusCode::NOT_FOUND, "Run not found").into_response()),
    }
}

pub(crate) async fn get_run_log(AxumPath(run_id): AxumPath<String>) -> Response {
    let r_dir = RunArchiver::resolve_runs_dir();
    match RunArchiver::get_console_log(&r_dir, &run_id) {
        Some(log) => log.into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "Console log not found").into_response(),
    }
}

/// Campaign list for the GUI: every `runs/campaign_*` directory with its
/// live `status.json` (the foreground CLI publishes it at ticket
/// boundaries) and, once finished, the `campaign.json` rollup.
/// Newest-first, capped.
/// The ladder, stop reason and board key for one campaign value, when its
/// spec is loadable. A running cell without a rollup gets a stub, so it
/// renders with its current ticket running and sorts first as its own
/// group (round-8 findings 8 and 9). One spec load per campaign name.
#[allow(clippy::type_complexity)]
/// S3: a live stub's phase and ceiling. A cell started before S3 lost its
/// numeric phase to the activity key; a chained cell's phase is then the
/// phase of its current ticket. The stub reads 0 of the phase's points
/// (0 of the ladder's for a full-ladder cell), not 0 of 0.
fn live_stub_phase(
    rollup: &mut crate::campaign::CampaignRollup,
    spec: &crate::campaign::CampaignSpec,
    v: &serde_json::Value,
) {
    if rollup.phase.is_none() && rollup.chain_from.is_some() {
        rollup.phase = v["current_stage"]
            .as_u64()
            .and_then(|c| crate::campaign::phase_of(spec, c as u32))
            .map(|p| p.id);
    }
    rollup.max_points = rollup
        .phase
        .and_then(|p| spec.phases.iter().find(|ph| ph.id == p))
        .map(|ph| crate::campaign::phase_max_points(ph, &spec.stages))
        .unwrap_or_else(|| crate::campaign::max_points(&spec.stages));
}

/// S3: a cell's own `ladder_state.json` (a running cell's carried state
/// before its rollup holds one).
fn cell_ladder_state(campaign_id: &str) -> Option<crate::campaign::LadderState> {
    let path = RunArchiver::resolve_runs_dir()
        .join(campaign_id)
        .join("ladder_state.json");
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The ladder, stop reason, sort key, rollup to write back and phase table
/// of one Board cell. The rollup is written back when it was parsed (so
/// display-only corrections reach the list) or for a live running cell's
/// stub.
type BoardView = (
    Vec<crate::campaign::LadderEntry>,
    String,
    crate::campaign::BoardSortKey,
    Option<crate::campaign::CampaignRollup>,
    serde_json::Value,
);

fn campaign_board_view(
    v: &serde_json::Value,
    specs: &mut std::collections::HashMap<String, Option<crate::campaign::CampaignSpec>>,
    campaign_id: &str,
) -> Option<BoardView> {
    let parsed: Option<crate::campaign::CampaignRollup> = v
        .get("rollup")
        .filter(|r| !r.is_null())
        .and_then(|r| serde_json::from_value(r.clone()).ok());
    let had_rollup = parsed.is_some();
    let live = !had_rollup;
    let mut rollup = parsed.unwrap_or_else(|| crate::campaign::live_rollup_stub(campaign_id, v));
    // R22-5: the Board list applies the same crash relabel as the detail
    // endpoint, including stages that already have recorded rounds.
    crate::campaign::relabel_graded_crashes(
        &mut rollup,
        &RunArchiver::resolve_runs_dir().join(campaign_id),
    );
    let spec = specs
        .entry(rollup.name.clone())
        .or_insert_with(|| {
            crate::campaign::load_campaign(&rollup.name)
                .ok()
                .map(|lc| lc.spec)
        })
        .clone()?;
    if live {
        live_stub_phase(&mut rollup, &spec, v);
    }
    // Only a *running* cell marks its current ticket running: a stalled
    // cell must not read `running` anywhere.
    let running = v["state"].as_str() == Some("running");
    let current = v["current_stage"]
        .as_u64()
        .filter(|_| running)
        .map(|c| c as u32);
    let carried = cell_ladder_state(campaign_id);
    let ladder = crate::campaign::phase_ladder(&spec, &rollup, current, carried.as_ref());
    let mut key = crate::campaign::BoardSortKey::of(&rollup);
    key.live = live;
    let stop = if live {
        String::new()
    } else if v["state"].as_str() == Some("cancelled") {
        // O2: the runner wrote why (a rollup stop reason does not exist).
        v["stop_reason"]
            .as_str()
            .unwrap_or("on request")
            .to_string()
    } else if !running && v["state"].as_str() != Some("done") {
        String::new()
    } else {
        crate::campaign::stop_reason(&rollup).to_string()
    };
    // A running cell has no rollup until its first ticket is recorded. The
    // board groups and renders cells from their rollup, so it gets the stub
    // the ladder and the key were built from; without it the cell vanished
    // from the Board for the whole first ticket. A stalled or dead cell
    // without a rollup keeps none: the stub's current rules would misfile it.
    let rollup_out = (had_rollup || running).then_some(rollup);
    Some((
        ladder,
        stop,
        key,
        rollup_out,
        crate::campaign::phases_json(&spec),
    ))
}

pub(crate) async fn get_campaigns() -> Json<Vec<serde_json::Value>> {
    let runs_dir = RunArchiver::resolve_runs_dir();
    let mut specs: std::collections::HashMap<String, Option<crate::campaign::CampaignSpec>> =
        std::collections::HashMap::new();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&runs_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let dir = e.path();
            if !name.starts_with("campaign_") || !dir.is_dir() {
                continue;
            }
            // Retired campaigns stay on disk (their runs are history) but
            // never reach the console: only the active ladder is listed.
            if !crate::campaign::ACTIVE_CAMPAIGNS
                .iter()
                .any(|c| name.starts_with(&format!("campaign_{c}_")))
            {
                continue;
            }
            // Round-13: a hidden or poisoned cell never reaches the console
            // either (the same rule the portal exporter applies).
            if crate::report::portal::is_hidden(&runs_dir, &name)
                || crate::report::portal::is_poisoned(&runs_dir, &name)
            {
                continue;
            }
            let mut v = std::fs::read_to_string(dir.join("status.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .unwrap_or_else(
                    || serde_json::json!({"state": "unknown", "rollup": serde_json::Value::Null}),
                );
            // A dead writer must not show "running" forever (round-7
            // finding 6).
            let derived = crate::campaign::effective_state(
                &name,
                v["state"].as_str().unwrap_or("unknown"),
                v["updated_at"].as_str(),
            );
            v["state"] = serde_json::json!(derived);
            if let Ok(t) = std::fs::read_to_string(dir.join("campaign.json")) {
                if let Ok(r) = serde_json::from_str::<serde_json::Value>(&t) {
                    v["state"] = serde_json::json!("done");
                    v["rollup"] = r;
                }
            }
            v["id"] = serde_json::json!(name);
            // O2 (round-11): the last log write is the most recent activity
            // a cancelled cell has; status.json can be 13 minutes older.
            v["log_mtime"] = campaign_log_mtime(&dir)
                .map(|t| serde_json::json!(t))
                .unwrap_or(serde_json::Value::Null);
            // The full ladder (attempted and not) + the stop reason: one
            // derivation for the console and the board (gui-spec.md §5.3).
            if let Some((ladder, stop, key, rollup_out, phases)) =
                campaign_board_view(&v, &mut specs, &name)
            {
                v["ladder"] = serde_json::to_value(ladder).unwrap_or(serde_json::Value::Null);
                v["phases"] = phases;
                v["stop_reason"] = serde_json::json!(stop);
                v["board_key"] = serde_json::to_value(key).unwrap_or_default();
                if let Some(rollup) = rollup_out {
                    v["rollup"] = serde_json::to_value(rollup).unwrap_or(serde_json::Value::Null);
                }
            }
            out.push(v);
        }
    }
    out.sort_by(|a, b| {
        b["id"]
            .as_str()
            .unwrap_or("")
            .cmp(a["id"].as_str().unwrap_or(""))
    });
    // The board order is the Rust comparator (no second ranking in JS).
    // The key was computed with the view above (a stub for live cells):
    // a total order, live cells pinned first (round-8 finding 9).
    let key = |v: &serde_json::Value| -> crate::campaign::BoardSortKey {
        v.get("board_key")
            .and_then(|k| serde_json::from_value(k.clone()).ok())
            .unwrap_or(crate::campaign::BoardSortKey {
                live: false,
                current_rules: false,
                points: 0.0,
                per_dollar: None,
                cost: 0.0,
            })
    };
    out.sort_by(|a, b| crate::campaign::board_order(&key(a), &key(b)));
    out.truncate(50);
    Json(out)
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct CampaignStartRequest {
    pub model: String,
    #[serde(default)]
    pub budget_usd: f64,
    #[serde(default)]
    pub wall_min: u64,
    #[serde(default)]
    pub effort: Option<String>,
    /// Budgets above $5 need an explicit confirmation (round-7 finding 1).
    #[serde(default)]
    pub confirm: bool,
    /// Career phase (P1); phases after 1 require `chain`.
    #[serde(default)]
    pub phase: Option<u32>,
    /// Previous phase cell id to continue from.
    #[serde(default)]
    pub chain: Option<String>,
    /// R1: deliberate fork reason for an already-used chain.
    #[serde(default)]
    pub refork: Option<String>,
}

/// Start a campaign run (gui-spec.md §4.4): what `scripts/sql-cell.sh`
/// runs, spawned from the engine. Round-7 finding 1: the POST is proxied
/// to the engine, it never touches `is_running` (the campaign holds its
/// own lock, which the CLI takes too), the child is reaped with its exit
/// status logged, and its output goes to `runs/campaign-launch-<ts>.log`.
pub(crate) async fn start_campaign(
    State(state): State<AppState>,
    Json(req): Json<CampaignStartRequest>,
) -> Response {
    if let Some(proxied) = proxy_mutation(
        &state,
        "POST",
        "/api/campaigns",
        Some(serde_json::to_value(&req).unwrap_or_default()),
    )
    .await
    {
        let status = proxied.status();
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        return (status, String::from_utf8_lossy(&bytes).to_string()).into_response();
    }
    // One campaign at a time, whichever launcher took the lock.
    if let Some(holder) = crate::campaign::live_lock_holder() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "campaign {} (pid {}) is already running",
                    holder.campaign_id, holder.pid
                )
            })),
        )
            .into_response();
    }
    if state.is_running.load(std::sync::atomic::Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "engine busy: a run is in flight"})),
        )
            .into_response();
    }
    let model = req.model.trim().to_string();
    if model.is_empty() || model.len() > 200 || model.contains([' ', '\n', '\t']) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "a model id is required"})),
        )
            .into_response();
    }
    let effort = req.effort.unwrap_or_else(|| "high".to_string());
    if !["minimal", "low", "medium", "high", "xhigh", "max", "off"].contains(&effort.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("unknown effort {effort:?}")})),
        )
            .into_response();
    }
    let budget = if req.budget_usd > 0.0 {
        req.budget_usd
    } else {
        crate::campaign::CELL_BUDGET_USD
    };
    if budget > 5.0 && !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "a budget above $5 needs confirm: true"
            })),
        )
            .into_response();
    }
    let wall = if req.wall_min > 0 {
        req.wall_min
    } else {
        crate::campaign::CELL_WALL_MIN
    };
    match (req.phase, req.chain.as_deref()) {
        (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "--chain requires --phase"})),
            )
                .into_response()
        }
        (Some(0), _) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "phase must be >= 1"})),
            )
                .into_response()
        }
        (Some(1), Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "phase 1 has no chain"})),
            )
                .into_response()
        }
        (Some(p), None) if p > 1 => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("phase {p} requires a chain cell")})),
            )
                .into_response()
        }
        _ => {}
    }
    if let Some(c) = req.chain.as_deref() {
        if c.contains('/') || c.contains("..") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid chain cell id"})),
            )
                .into_response();
        }
    }
    if req.refork.is_some() && req.chain.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "--refork requires a chain cell"})),
        )
            .into_response();
    }
    let refork = req
        .refork
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    if req.refork.is_some() && refork.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "a refork reason is required"})),
        )
            .into_response();
    }
    if let Some(chain_id) = req.chain.as_deref() {
        let runs_dir = RunArchiver::resolve_runs_dir();
        match crate::campaign::chain_already_used(&runs_dir, chain_id) {
            Some(other) if refork.is_none() => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": format!(
                            "chain cell {chain_id} was already continued by {other}; give a refork reason to fork deliberately"
                        ),
                        "refork_required": true,
                        "chain_used_by": other
                    })),
                )
                    .into_response()
            }
            None if req.refork.is_some() => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "this chain cell is not used by an existing cell; drop the refork reason"
                    })),
                )
                    .into_response()
            }
            _ => {}
        }
    }
    let Some(exe) = crate::campaign::launcher_binary() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "cannot resolve the engine binary"})),
        )
            .into_response();
    };
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let log_path = RunArchiver::resolve_runs_dir().join(format!("campaign-launch-{ts}.log"));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);
    let (out, err) = match log.and_then(|f| f.try_clone().map(|c| (f, c))) {
        Ok((a, b)) => (a, b),
        Err(e) => return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("cannot open {}: {e}", log_path.display())})),
        )
            .into_response(),
    };
    let args = crate::campaign::launcher_args_with_phase(
        &model,
        &effort,
        budget,
        wall,
        req.phase,
        req.chain.as_deref(),
        refork,
    );
    let cmd = format!("subdollarbench {}", args.join(" "));
    let mut command = std::process::Command::new(exe);
    command
        .args(&args)
        .current_dir(crate::config::get_repo_root())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err));
    // Its own process group: restarting the console (or a service manager
    // killing its group) must not take a two-hour campaign down with it
    // (round-8 finding 3).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let spawned = command.spawn();
    match spawned {
        Ok(mut child) => {
            let pid = child.id();
            let log_for_status = log_path.clone();
            // Reap it: a campaign that dies immediately must leave a trace
            // and its exit status in the launch log, never a silent
            // {"started": true} (round-7 finding 1).
            tokio::task::spawn_blocking(move || {
                let status = child.wait();
                let line = match status {
                    Ok(s) => format!("\n[LAUNCHER] campaign exited: {s}\n"),
                    Err(e) => format!("\n[LAUNCHER] campaign wait failed: {e}\n"),
                };
                eprintln!("[CAMPAIGN] {line}");
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&log_for_status)
                {
                    use std::io::Write as _;
                    let _ = f.write_all(line.as_bytes());
                }
            });
            Json(serde_json::json!({
                "started": true,
                "pid": pid,
                "cmd": cmd,
                "log": log_path.display().to_string(),
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("spawn failed: {e}")})),
        )
            .into_response(),
    }
}

/// One campaign in depth (gui-spec.md §5.4): the rollup with rounds and
/// classified failures backfilled for old cells, the full ladder (every
/// ticket, attempted or not) and the caps. This is what the Run view and
/// the board's mini-strip render.
pub(crate) async fn get_campaign_detail(AxumPath(id): AxumPath<String>) -> Response {
    let clean = match clean_run_id(&id) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "invalid campaign id").into_response(),
    };
    let dir = RunArchiver::resolve_runs_dir().join(&clean);
    if !dir.is_dir() {
        return (StatusCode::NOT_FOUND, "no such campaign").into_response();
    }
    let final_rollup = std::fs::read_to_string(dir.join("campaign.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<crate::campaign::CampaignRollup>(&t).ok());
    let status: Option<serde_json::Value> = std::fs::read_to_string(dir.join("status.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let state = crate::campaign::effective_state(
        &clean,
        status
            .as_ref()
            .and_then(|s| s["state"].as_str())
            .unwrap_or(if final_rollup.is_some() {
                "done"
            } else {
                "unknown"
            }),
        status.as_ref().and_then(|s| s["updated_at"].as_str()),
    );
    let current_stage = status.as_ref().and_then(|s| s["current_stage"].as_u64());
    // A running cell has no rollup until its first verdict lands; return a
    // definition-shaped stub instead of 404, so the Run view can show the
    // current ticket (round-8 finding 8).
    let mut live = false;
    let mut rollup = match final_rollup {
        Some(r) => r,
        None => match status
            .as_ref()
            .and_then(|s| s.get("rollup"))
            .filter(|r| !r.is_null())
            .and_then(|r| serde_json::from_value::<crate::campaign::CampaignRollup>(r.clone()).ok())
        {
            Some(r) => r,
            None => {
                live = true;
                let v = status.clone().unwrap_or_else(|| serde_json::json!({}));
                crate::campaign::live_rollup_stub(&clean, &v)
            }
        },
    };
    // Old cells: derive rounds/failures from their manifests (marked
    // derived) so the Run view renders them too.
    crate::campaign::backfill_rounds(&mut rollup, &dir);
    let stop = if live || state != "done" {
        // Only a finished cell has a stop reason; running and stalled ones
        // show their state instead.
        String::new()
    } else {
        crate::campaign::stop_reason(&rollup).to_string()
    };
    let budget = rollup.budget_usd;
    let wall = rollup.wall_min;
    let running = state == "running";
    let current = current_stage.filter(|_| running).map(|c| c as u32);
    let spec = crate::campaign::load_campaign(&rollup.name)
        .ok()
        .map(|lc| lc.spec);
    if live {
        if let (Some(spec), Some(v)) = (spec.as_ref(), status.as_ref()) {
            live_stub_phase(&mut rollup, spec, v);
        }
    }
    let carried = cell_ladder_state(&clean);
    let ladder = spec
        .as_ref()
        .map(|spec| crate::campaign::phase_ladder(spec, &rollup, current, carried.as_ref()));
    let mut body = serde_json::json!({
        "id": clean,
        "state": state,
        "current_stage": current_stage,
        "rollup": rollup,
        "stop_reason": stop,
        "budget_usd": budget,
        "wall_min": wall,
    });
    if let Some(ladder) = ladder {
        body["ladder"] = serde_json::to_value(ladder).unwrap_or(serde_json::Value::Null);
    }
    if let Some(spec) = spec.as_ref() {
        body["phases"] = crate::campaign::phases_json(spec);
    }
    body["live_run"] = serde_json::json!(live);
    body["updated_at"] = status
        .as_ref()
        .map(|s| s["updated_at"].clone())
        .unwrap_or(serde_json::Value::Null);
    body["log_mtime"] = campaign_log_mtime(&dir)
        .map(|t| serde_json::json!(t))
        .unwrap_or(serde_json::Value::Null);
    Json(body).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct CampaignLogQuery {
    /// Max lines to return (newest) when `after` is absent. Defaults to 400.
    tail: Option<usize>,
    /// Byte offset to resume from (incremental polling; gui-spec.md §4.3).
    after: Option<u64>,
    /// Comma-separated kinds to keep: verdict, agent, tools, verifier, setup.
    kinds: Option<String>,
}

/// Strip the campaign log's `[HH:MM:SS] ` timestamp (every real line has
/// one; the tag checks below must see the tag itself — round-8 finding 6).
fn strip_log_ts(line: &str) -> &str {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix('[') {
        if let Some((ts, rest)) = rest.split_once("] ") {
            let b = ts.as_bytes();
            if b.len() == 8 && b[2] == b':' && b[5] == b':' {
                return rest.trim_start();
            }
        }
    }
    t
}

/// One log line's bucket for the Live view's filter chips (gui-spec.md
/// §4.3). Unknown lines fall into `verifier` (pipeline output).
pub(crate) fn log_kind(line: &str) -> &'static str {
    let t = strip_log_ts(line);
    if t.starts_with("[OMP")
        || t.starts_with("[TELEMETRY]")
        || t.starts_with("[TOKENS]")
        || t.starts_with("[LIVE]")
        || t.starts_with("[WAITING]")
    {
        // Per-turn bookkeeping the header card already shows; off by
        // default in the Live chips (round-10 U1).
        "telemetry"
    } else if t.starts_with("[THOUGHT]")
        || t.starts_with("[RESPONSE]")
        || t.starts_with("[LIMIT]")
        || (t.starts_with("[FEEDBACK] Round") && t.contains("agent completed"))
    {
        "agent"
    } else if t.starts_with("[ACTION]")
        || t.starts_with("[RESULT]")
        || t.starts_with("[PROMPT]")
        || t.starts_with("[STDERR]")
    {
        "tools"
    } else if t.starts_with("[SETUP]")
        || t.starts_with("[INFRA]")
        || t.starts_with("[WARN]")
        || t.starts_with("[CAMPAIGN]")
        || t.starts_with("[PORTAL]")
    {
        "setup"
    } else if t.starts_with("[VERIFY]")
        || t.starts_with("[BENCH]")
        || t.starts_with("[DIAGNOSTICS]")
        || t.contains("statements graded")
    {
        "verifier"
    } else if t.starts_with("  >") || t.starts_with("> ") {
        // Tool output folds under its [ACTION] (round-8 finding 6).
        "tools"
    } else if t.starts_with("  |") || t.starts_with("| ") {
        // The re-logged prompt/report blocks are setup prose.
        "setup"
    } else if t.starts_with("[UPGRADE]")
        || t.starts_with("[CRASH]")
        || t.starts_with("[TXN]")
        || t.starts_with("[PERF]")
        || t.starts_with("[FEEDBACK]")
        || t.starts_with("[ROLLBACK]")
        || (t.starts_with("Stage ") && t.contains("->"))
        || t.starts_with("─── ticket")
        || t.starts_with("=== CAMPAIGN")
        || t.starts_with("reached:")
        || t.starts_with("stopped:")
        || t.starts_with("regression-free")
        || t.starts_with("rollup:")
        || (t.starts_with('s')
            && t.as_bytes()
                .get(1)
                .map(|b| b.is_ascii_digit())
                .unwrap_or(false))
    {
        "verdict"
    } else {
        "verifier"
    }
}

/// The campaign's console log: `live.log`, else the newest stage
/// `console.log` (campaigns recorded before live logging existed).
fn campaign_log_path(dir: &Path) -> Option<std::path::PathBuf> {
    let live = dir.join("live.log");
    if live.is_file() {
        return Some(live);
    }
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = n.strip_prefix('s') {
            if let Ok(stage) = rest.parse::<u32>() {
                let p = e.path().join("console.log");
                if p.is_file() && best.as_ref().map(|(b, _)| stage > *b).unwrap_or(true) {
                    best = Some((stage, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// RFC 3339 mtime of that log: the latest activity a cancelled or stalled
/// cell has (round-11 O2; status.json can be minutes behind).
fn campaign_log_mtime(dir: &Path) -> Option<String> {
    let path = campaign_log_path(dir)?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339())
}

/// Live campaign console: `live.log` while/after a run (append-only, written
/// by the campaign CLI), falling back to the newest stage `console.log`
/// (campaigns recorded before live logging existed).
pub(crate) async fn get_campaign_log(
    AxumPath(id): AxumPath<String>,
    Query(q): Query<CampaignLogQuery>,
) -> Response {
    let clean = match clean_run_id(&id) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "invalid campaign id").into_response(),
    };
    let dir = RunArchiver::resolve_runs_dir().join(&clean);
    let path = match campaign_log_path(&dir) {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "no campaign log yet").into_response(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "campaign log unreadable").into_response(),
    };
    let wanted: Option<Vec<&str>> = q.kinds.as_deref().map(|k| {
        k.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    });
    let (start, end) = if let Some(after) = q.after {
        // Incremental: everything after the offset, whole lines only (a
        // half-written line is re-read next poll).
        let start = (after as usize).min(bytes.len());
        let end = bytes[start..]
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| start + i + 1)
            .unwrap_or(start);
        (start, end)
    } else {
        // Backward-compatible tail: the last `tail` whole lines.
        let tail = q.tail.unwrap_or(400).max(1);
        let text = String::from_utf8_lossy(&bytes);
        let mut cut = 0usize;
        let mut seen = 0usize;
        for (i, _) in text.match_indices('\n').rev() {
            seen += 1;
            if seen > tail {
                cut = i + 1;
                break;
            }
        }
        (cut, bytes.len())
    };
    let chunk = String::from_utf8_lossy(&bytes[start..end]);
    let lines: Vec<serde_json::Value> = chunk
        .lines()
        .filter(|l| {
            wanted
                .as_ref()
                .map(|w| w.contains(&log_kind(l)))
                .unwrap_or(true)
        })
        .map(|l| serde_json::json!({"kind": log_kind(l), "text": l}))
        .collect();
    Json(serde_json::json!({"offset": end, "lines": lines})).into_response()
}

pub(crate) async fn get_run_files(
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<Vec<FileInfo>>, Response> {
    // Same guard the sibling run handlers use; this one was joining a raw
    // `:id` straight onto the runs dir, so a percent-encoded `..%2F` walked out.
    let clean = match clean_run_id(&run_id) {
        Some(c) => c,
        None => return Err((StatusCode::BAD_REQUEST, "invalid run id").into_response()),
    };
    let ws_dir = RunArchiver::resolve_runs_dir()
        .join(clean)
        .join("workspace");
    if ws_dir.exists() {
        Ok(Json(RunArchiver::scan_workspace_files(&ws_dir)))
    } else {
        Err((axum::http::StatusCode::NOT_FOUND, "Workspace not found").into_response())
    }
}

pub(crate) async fn get_run_file(
    AxumPath(run_id): AxumPath<String>,
    Query(query): Query<FileQuery>,
) -> Response {
    let r_dir = RunArchiver::resolve_runs_dir();
    match RunArchiver::get_workspace_file(&r_dir, &run_id, &query.file) {
        Some(content) => content.into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "File not found").into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConsoleQuery {
    /// Worker id for a per-worker view. Absent = multiplexed all-workers view.
    worker: Option<u8>,
    /// Max lines to return (newest). Defaults to the buffer cap.
    tail: Option<usize>,
    /// Lines the client already holds; returns only newer lines plus a
    /// `X-Console-Total` header so the client can detect truncation gaps.
    since: Option<usize>,
}

pub(crate) async fn get_active_console(
    State(state): State<AppState>,
    Query(q): Query<ConsoleQuery>,
    request: axum::extract::Request,
) -> Response {
    if !state.engine_url.is_empty() {
        let qs = request
            .uri()
            .query()
            .map(|x| format!("?{x}"))
            .unwrap_or_default();
        if let Some(proxied) = proxy_read(&state, "/api/console", &qs).await {
            return proxied;
        }
    }
    // Per-worker view: one worker's lines only (see worker_buffers).
    // Multiplexed all-view stays the default for back-compat. Unknown
    // workers yield an empty body with total 0 (same as an empty buffer).
    let buf: Vec<String> = if let Some(w) = q.worker {
        state
            .worker_buffers
            .read()
            .unwrap()
            .get(&w)
            .cloned()
            .unwrap_or_default()
    } else {
        state.log_buffer.read().unwrap().clone()
    };
    let total = buf.len();
    // Steady-state deltas (`since` without explicit tail) are capped small:
    // agents emit 100KB+ single lines (tool payloads) and a 3000-line dump
    // every 2s visibly thrashes the terminal. Cursor still jumps to total.
    let default_cap = if q.since.is_some() {
        200
    } else {
        MAX_CONSOLE_LINES
    };
    let cap = q.tail.unwrap_or(default_cap).min(total.max(1));
    let tail_start = total.saturating_sub(cap);
    let start = match q.since {
        // Unknown cursor (client counts rendered lines, server counts pushed
        // lines): resend the capped tail so the client re-renders in sync.
        Some(known) if known >= total || known < tail_start => tail_start,
        Some(known) => known,
        None => tail_start,
    };
    let body = buf[start..].join("\n");
    ([("X-Console-Total", total.to_string())], body).into_response()
}

pub(crate) async fn get_env() -> Json<EnvironmentInfo> {
    Json(EnvironmentInfo::detect())
}

#[derive(Deserialize)]
pub(crate) struct StopQuery {
    #[serde(default)]
    run: String,
}

pub(crate) async fn stop_run(
    State(state): State<AppState>,
    Query(q): Query<StopQuery>,
) -> Response {
    if let Some(proxied) = proxy_mutation(
        &state,
        "POST",
        &format!(
            "/api/run/stop{}",
            if q.run.is_empty() {
                String::new()
            } else {
                format!("?run={}", q.run)
            }
        ),
        None,
    )
    .await
    {
        return proxied;
    }
    eprintln!("[AUDIT] POST /api/run/stop at {}", Utc::now().to_rfc3339());
    if !state.is_running.load(Ordering::SeqCst) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "No benchmark is currently running",
        )
            .into_response();
    }

    // Per-run stop (?run=id-or-prefix): kill only matching workers, release
    // their slots, mark their queue items Cancelled. Other workers continue.
    // Cleanup discipline: cancel_flags entries removed on run completion AND
    // here, so the map never grows across waves.
    if !q.run.trim().is_empty() {
        let needle = q.run.trim().to_string();
        // The worker cards send `run_id.slice(-14)`, so a suffix match has to
        // keep working - but an unbounded substring match means an
        // unauthenticated `POST /api/run/stop?run=e` ends-matches most live
        // runs. Require a substantial fragment, then require the fragment to
        // identify exactly one run: a 14-char suffix cannot collide in practice
        // (ids end in a timestamp), whereas a short one matches many.
        const MIN_STOP_FRAGMENT: usize = 8;
        if needle.len() < MIN_STOP_FRAGMENT {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("run fragment must be at least {MIN_STOP_FRAGMENT} characters"),
            )
                .into_response();
        }
        let matches = |id: &str| id == needle || id.starts_with(&needle) || id.ends_with(&needle);
        let hits: Vec<(u8, String)> = state
            .worker_runs
            .read()
            .unwrap()
            .iter()
            .filter(|(_, info)| matches(&info.run_id))
            .map(|(w, info)| (*w, info.run_id.clone()))
            .collect();
        let qhits: Vec<String> = state
            .queue
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.status == QueueStatus::Running && matches(&i.id))
            .map(|i| i.id.clone())
            .collect();
        if hits.is_empty() && qhits.is_empty() {
            return (
                axum::http::StatusCode::NOT_FOUND,
                format!("no running cell matches '{needle}'"),
            )
                .into_response();
        }
        if hits.len() + qhits.len() > 1 {
            let mut candidates: Vec<&str> = hits
                .iter()
                .map(|(_, id)| id.as_str())
                .chain(qhits.iter().map(|s| s.as_str()))
                .collect();
            candidates.sort_unstable();
            return (
                axum::http::StatusCode::CONFLICT,
                format!(
                    "'{needle}' matches {} running cells; pass a longer fragment: {:?}",
                    candidates.len(),
                    candidates
                ),
            )
                .into_response();
        }
        for (w, run_id) in &hits {
            if let Some(h) = state.worker_handles.lock().unwrap().remove(w) {
                h.abort();
            }
            let sandbox = SandboxManager::with_id(run_id);
            sandbox.cleanup_instance();
            state.worker_runs.write().unwrap().remove(w);
            release_slot(&state, *w);
        }
        state
            .cancel_flags
            .lock()
            .unwrap()
            .retain(|k, _| !hits.iter().any(|(_, r)| r == k));
        {
            let mut ql = state.queue.lock().unwrap();
            for item in ql.iter_mut().filter(|i| i.status == QueueStatus::Running) {
                if item.id == needle
                    || item.id.starts_with(&needle)
                    || item.id.ends_with(&needle)
                    || hits.iter().any(|(_, r)| item.id == *r)
                {
                    item.status = QueueStatus::Cancelled;
                }
            }
        }
        send_status(
            &state.log_sender,
            &state.log_buffer,
            format!(
                "[USER ACTION] Stopped {} cell(s) matching '{needle}'.",
                hits.len().max(qhits.len())
            ),
        );
        return (axum::http::StatusCode::OK, format!("stopped {needle}")).into_response();
    }
    state.cancel_requested.store(true, Ordering::SeqCst);
    let tx = state.log_sender.clone();
    let log_buf = state.log_buffer.clone();
    send_status(
        &tx,
        &log_buf,
        "[USER ACTION] Cancellation requested. Terminating all worker sandboxes...".to_string(),
    );

    // Abort every worker handle + per-run cancel flags + sandboxes.
    for (_w, handle) in state.worker_handles.lock().unwrap().drain() {
        handle.abort();
    }
    {
        let flags = state.cancel_flags.lock().unwrap();
        for flag in flags.values() {
            flag.store(true, Ordering::SeqCst);
        }
    }
    for info in state.worker_runs.read().unwrap().values() {
        let sandbox = SandboxManager::with_id(&info.run_id);
        sandbox.cleanup_instance();
    }
    // Legacy single handle/run (SDB_WORKERS=1 path + in-flight direct tasks).
    if let Some(handle) = state.task_handle.lock().unwrap().take() {
        handle.abort();
    }
    if state.current_run.read().unwrap().is_some() {
        let maybe_run = state.current_run.read().unwrap().clone();
        if let Some(info) = maybe_run {
            let sandbox = SandboxManager::with_id(&info.run_id);
            sandbox.cleanup_instance();
        }
    }
    *state.current_run.write().unwrap() = None;
    state.worker_runs.write().unwrap().clear();
    state.cancel_flags.lock().unwrap().clear();
    {
        let cancelled: Vec<String> = {
            let mut q = state.queue.lock().unwrap();
            q.iter_mut()
                .filter(|i| i.status == QueueStatus::Running)
                .map(|item| {
                    item.status = QueueStatus::Cancelled;
                    item.id.clone()
                })
                .collect()
        };
        if !cancelled.is_empty() {
            let now = now_rfc3339();
            db_mirror(&state, "mark cancelled", move |store| {
                for id in &cancelled {
                    db_try(
                        &format!("mark {id} Cancelled"),
                        store.update_queue_status(id, "Cancelled", None, Some(&now)),
                    );
                }
            });
        }
    }
    {
        let mut slots = state.running_slots.lock().unwrap();
        slots.clear();
        state.is_running.store(false, Ordering::SeqCst);
    }
    state.worker_handles.lock().unwrap().clear();
    send_status(
        &tx,
        &log_buf,
        "[DONE] Benchmark run was stopped by user.".to_string(),
    );

    (axum::http::StatusCode::OK, "Benchmark run stopped").into_response()
}

pub(crate) async fn compare_runs(
    Query(query): Query<CompareQuery>,
) -> Result<Json<RunComparisonResponse>, Response> {
    // Workspace joins below must not walk out of runs/: validate both ids
    // with the same file_name guard the manifest path uses.
    let clean_a = match clean_run_id(&query.run_a) {
        Some(c) => c,
        None => {
            return Err((StatusCode::BAD_REQUEST, "invalid run_a id").into_response());
        }
    };
    let clean_b = match clean_run_id(&query.run_b) {
        Some(c) => c,
        None => {
            return Err((StatusCode::BAD_REQUEST, "invalid run_b id").into_response());
        }
    };
    let runs_dir = RunArchiver::resolve_runs_dir();
    let manifest_a = RunArchiver::get_run(&runs_dir, &query.run_a).ok_or_else(|| {
        (
            axum::http::StatusCode::NOT_FOUND,
            format!("Run A '{}' not found", query.run_a),
        )
            .into_response()
    })?;
    let manifest_b = RunArchiver::get_run(&runs_dir, &query.run_b).ok_or_else(|| {
        (
            axum::http::StatusCode::NOT_FOUND,
            format!("Run B '{}' not found", query.run_b),
        )
            .into_response()
    })?;

    let metric_diff = MetricComparison {
        pass_rate_delta: manifest_b.pass_rate - manifest_a.pass_rate,
        cost_delta: manifest_b.cost_usd - manifest_a.cost_usd,
        effective_cost_delta: Some(manifest_b.effective_cost() - manifest_a.effective_cost()),
        tokens_delta: (manifest_b.tokens.total_tokens as i64)
            - (manifest_a.tokens.total_tokens as i64),
        duration_delta: manifest_b.duration_seconds - manifest_a.duration_seconds,
        throughput_delta: match (manifest_a.throughput_req_sec, manifest_b.throughput_req_sec) {
            (Some(a), Some(b)) => Some(b - a),
            _ => None,
        },
    };

    let ws_a = runs_dir.join(&clean_a).join("workspace");
    let ws_b = runs_dir.join(&clean_b).join("workspace");

    let file_diffs = compare_workspaces(&ws_a, &ws_b);

    Ok(Json(RunComparisonResponse {
        run_a: manifest_a,
        run_b: manifest_b,
        metric_diff,
        file_diffs,
    }))
}

/// Per-file diff assembly for `compare_runs`: one entry per path present in
/// either workspace, with a unified diff for modified/added/removed files.
/// Split from the handler so the whole status matrix is testable against
/// temp dirs.
fn compare_workspaces(ws_a: &Path, ws_b: &Path) -> Vec<FileDiffEntry> {
    let files_a = if ws_a.exists() {
        RunArchiver::scan_workspace_files(ws_a)
    } else {
        Vec::new()
    };
    let files_b = if ws_b.exists() {
        RunArchiver::scan_workspace_files(ws_b)
    } else {
        Vec::new()
    };

    let mut all_paths: BTreeSet<String> = BTreeSet::new();
    for f in &files_a {
        all_paths.insert(f.name.clone());
    }
    for f in &files_b {
        all_paths.insert(f.name.clone());
    }

    let mut file_diffs = Vec::new();
    for rel_path in all_paths {
        let path_a = ws_a.join(&rel_path);
        let path_b = ws_b.join(&rel_path);

        let content_a = if path_a.exists() {
            fs::read_to_string(&path_a).ok()
        } else {
            None
        };
        let content_b = if path_b.exists() {
            fs::read_to_string(&path_b).ok()
        } else {
            None
        };

        let lines_a = content_a.as_ref().map(|s| s.lines().count()).unwrap_or(0);
        let lines_b = content_b.as_ref().map(|s| s.lines().count()).unwrap_or(0);

        let (status, diff) = match (&content_a, &content_b) {
            (Some(a), Some(b)) => {
                if a == b {
                    ("identical".to_string(), String::new())
                } else {
                    let d = generate_unified_diff(&rel_path, a, b);
                    ("modified".to_string(), d)
                }
            }
            (Some(a), None) => {
                let d = generate_unified_diff(&rel_path, a, "");
                ("removed_in_b".to_string(), d)
            }
            (None, Some(b)) => {
                let d = generate_unified_diff(&rel_path, "", b);
                ("added_in_b".to_string(), d)
            }
            (None, None) => ("unknown".to_string(), String::new()),
        };

        file_diffs.push(FileDiffEntry {
            path: rel_path,
            status,
            lines_a,
            lines_b,
            diff,
            content_a,
            content_b,
        });
    }

    file_diffs
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ExtendBody {
    trials: Option<u32>,
    max_turns: Option<u32>,
    budget_usd: Option<f64>,
    timeout_min: Option<u64>,
    eval_only: Option<bool>,
    /// Optional caller key; forwarded into the extend spec so extension
    /// trials bill the same account as the triggering request. Falls back
    /// to `OPENROUTER_API_KEY` env when absent.
    #[serde(default)]
    api_key: Option<String>,
    /// When true and the runner is busy, enqueue behind the running job
    /// instead of failing with 409. Stability re-validates at dispatch:
    /// a parent stabilized meanwhile skips with a log line, never runs.
    #[serde(default)]
    queue: bool,
}

/// Run N more trials for a consolidated parent and re-merge.
/// Refuses stable parents and unknown ids; 409 while the engine is busy
/// unless `queue: true` asks to wait behind the running job.
pub(crate) async fn extend_parent_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ExtendBody>,
) -> Result<Json<serde_json::Value>, Response> {
    if let Some(proxied) = proxy_mutation(
        &state,
        "POST",
        &format!("/api/runs/{id}/extend"),
        Some(serde_json::to_value(&body).unwrap_or_default()),
    )
    .await
    {
        let status = proxied.status();
        if status.is_success() {
            let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
                .await
                .unwrap_or_default();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            return Ok(Json(v));
        }
        return Err(proxied);
    }
    let fail = |code: StatusCode, msg: String| (code, msg).into_response();
    let clean = clean_run_id(&id)
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, format!("invalid run id '{id}'")))?;
    // Cheap checks for the queue path: id shape, parent parses, multi-trial.
    // Stability re-runs at dispatch under the held slot.
    let cheap_check = || -> Result<BenchmarkRunResult, Response> {
        let parent_path = crate::config::get_repo_root()
            .join("results")
            .join(format!("{clean}.json"));
        let data: BenchmarkRunResult = std::fs::read_to_string(&parent_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .ok_or_else(|| {
                fail(
                    StatusCode::NOT_FOUND,
                    format!("no consolidated result for '{clean}'"),
                )
            })?;
        if data.subruns.len() < 2 {
            return Err(fail(
                StatusCode::CONFLICT,
                format!("{clean} is not a multi-trial run"),
            ));
        }
        Ok(data)
    };
    if state
        .is_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        if !body.queue {
            return Err(fail(
                StatusCode::CONFLICT,
                "Benchmark is already running".to_string(),
            ));
        }
        cheap_check()?;
        let ext = ExtendRequest {
            parent_run_id: clean.clone(),
            trials: body.trials.unwrap_or(3).max(1),
            max_turns: body.max_turns,
            budget_usd: body.budget_usd,
            timeout_min: body.timeout_min,
            eval_only: body.eval_only,
            api_key: body.api_key.clone(),
        };
        let qid = push_queue_item(&state, QueueKind::Extend(ext));
        let position = state
            .queue
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.status == QueueStatus::Pending)
            .position(|i| i.id == qid)
            .map(|p| p + 1);
        // NOTE: no kick here — the runner is busy by definition; the
        // current body hands off via pump_queue on completion.
        return Ok(Json(serde_json::json!({
            "queued": qid,
            "parent": clean,
            "position": position,
        })));
    }
    // NOTE: every early return below must release the slot.
    let release = |state: &AppState| state.is_running.store(false, Ordering::SeqCst);
    let parent_path = crate::config::get_repo_root()
        .join("results")
        .join(format!("{clean}.json"));
    let data: BenchmarkRunResult = match std::fs::read_to_string(&parent_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
    {
        Some(d) => d,
        None => {
            release(&state);
            return Err(fail(
                StatusCode::NOT_FOUND,
                format!("no consolidated result for '{clean}'"),
            ));
        }
    };
    if data.subruns.len() < 2 {
        release(&state);
        return Err(fail(
            StatusCode::CONFLICT,
            format!("{clean} is not a multi-trial run"),
        ));
    }
    let (stab, reason) = assess_stability(&data.subruns);
    if stab == Stability::Stable {
        release(&state);
        return Err(fail(
            StatusCode::CONFLICT,
            format!("{clean} already stable"),
        ));
    }
    let task_type = match data.task.as_str() {
        "redis" => TaskType::Redis,
        "http" => TaskType::Http,
        "pdf" => TaskType::Pdf,
        "wal" => TaskType::Wal,
        "proxy" => TaskType::Proxy,
        "pubsub" => TaskType::Pubsub,
        "binproto" => TaskType::Binproto,
        _ => {
            release(&state);
            return Err(fail(
                StatusCode::BAD_REQUEST,
                format!("unknown task '{}'", data.task),
            ));
        }
    };
    let count = body.trials.unwrap_or(3).max(1);
    let total = data.subruns.len() as u32 + count;
    let spec = ExtBatchSpec {
        model: data.model.clone(),
        task: task_type,
        effort: data.effort.clone().unwrap_or_else(|| "high".to_string()),
        harness: data.harness.clone().unwrap_or_else(|| "omp".to_string()),
        max_turns: body.max_turns.unwrap_or(100),
        budget_usd: body.budget_usd.unwrap_or(1.0),
        timeout_min: body.timeout_min.unwrap_or(30),
        api_key: body.api_key.clone(),
        eval_only: body.eval_only.unwrap_or(false),
        save_results: true,
        review_model: None,
        parent_run_id: clean.clone(),
        start_idx: data.subruns.len() as u32 + 1,
        count,
        display_total: total,
        feedback_rounds: data.feedback_rounds.unwrap_or(0),
    };
    let state_clone = state.clone();
    let clean_for_task = clean.clone();
    state.cancel_requested.store(false, Ordering::SeqCst);
    let handle = tokio::spawn(async move {
        state_clone.cancel_requested.store(false, Ordering::SeqCst);
        let ext_worker = match claim_slot(&state_clone) {
            Some(w) => w,
            None => {
                send_status(
                    &state_clone.log_sender,
                    &state_clone.log_buffer,
                    "[EXTEND] all workers busy; skipping direct extension.".to_string(),
                );
                return;
            }
        };
        let tx = state_clone.log_sender.clone();
        let ext_log_buf = state_clone.log_buffer.clone();
        send_status(
            &tx,
            &ext_log_buf,
            format!("[EXTEND] {reason}; running {count} more trial(s) for {clean_for_task}..."),
        );
        let out = run_extension_batch(
            &tx,
            &state_clone.log_buffer,
            &state_clone.current_run,
            &state_clone.cancel_requested,
            &|m: &str| {
                send_status(&tx, &ext_log_buf, m.to_string());
            },
            spec,
            ext_worker,
            &state_clone.worker_buffers,
            &state_clone.worker_runs,
        )
        .await;
        if out.billing_tripped {
            pause_queue(&state_clone, "provider billing error in extension batch");
        }
        if !out.results.is_empty() {
            let mut combined = data.subruns.clone();
            combined.extend(out.results);
            let passing = combined.iter().filter(|r| r.pass_rate >= 100.0).count();
            let drift = data.ref_drift.or(out.drift).or(Some(false));
            let merged = LeaderboardManager::consolidate_multi_trial_run(
                &clean_for_task,
                &combined,
                true,
                drift,
                data.protocol.clone(),
            );
            let parked: Vec<BenchmarkRunResult> = merged.map(|m| m.subruns).unwrap_or_default();
            let (stab2, reason2) = assess_stability(&parked);
            send_status(
                &tx,
                &ext_log_buf,
                format!(
                    "[EXTEND] {clean_for_task}: {} trials, {passing} passing, Pass@1 {:.1}% — {stab2:?} ({reason2})",
                    combined.len(),
                    compute_pass_at_k(combined.len(), passing, 1) * 100.0
                ),
            );
        }
        release_slot(&state_clone, ext_worker);
        if !pump_queue(&state_clone) {
            send_status(&tx, &ext_log_buf, "[DONE]".to_string());
        }
    });
    *state.task_handle.lock().unwrap() = Some(handle);
    Ok(Json(serde_json::json!({
        "started": clean,
    })))
}

pub(crate) async fn pause_queue_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    if let Some(proxied) = proxy_mutation(&state, "POST", "/api/queue/pause", None).await {
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        return Json(serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({"ok": false})));
    }
    pause_queue(&state, "paused by operator");
    Json(serde_json::json!({"ok": true, "paused": true}))
}

pub(crate) async fn resume_queue_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    if let Some(proxied) = proxy_mutation(&state, "POST", "/api/queue/resume", None).await {
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        return Json(serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({"ok": false})));
    }
    resume_queue(&state);
    let first = state
        .queue
        .lock()
        .unwrap()
        .iter()
        .find(|i| i.status == QueueStatus::Pending)
        .map(|i| i.id.clone());
    if let Some(id) = first {
        kick_runner(&state, &id).await;
    }
    Json(serde_json::json!({"ok": true, "paused": false}))
}

/// Pre-flight balance gate: refuse new batches when credit is exhausted.
/// Unknown balance = allow (fail open, tripwire guards).
pub(crate) async fn balance_check_handler(
    State(_state): State<AppState>,
) -> Json<serde_json::Value> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        return Json(serde_json::json!({"ok": false, "reason": "no API key configured"}));
    }
    match crate::cost::ModelPricing::query_key_balance(&key).await {
        Some(b) => {
            let remaining = b.remaining();
            let ok = remaining.map(|r| r >= 1.0).unwrap_or(true);
            Json(
                serde_json::json!({"ok": ok, "usage": b.usage, "limit": b.limit, "remaining": remaining}),
            )
        }
        None => Json(serde_json::json!({"ok": true, "unknown": true})),
    }
}

pub(crate) async fn get_queue(State(state): State<AppState>) -> Response {
    if let Some(proxied) = proxy_read(&state, "/api/queue", "").await {
        return proxied;
    }
    reconcile_queue(&state);
    Json::<Vec<QueuedRun>>(
        state
            .queue
            .lock()
            .unwrap()
            .iter()
            .map(|q| q.redacted())
            .collect(),
    )
    .into_response()
}

pub(crate) async fn enqueue_run(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Result<Json<serde_json::Value>, Response> {
    if let Some(proxied) = proxy_mutation(
        &state,
        "POST",
        "/api/queue",
        Some(serde_json::to_value(&req).unwrap_or_default()),
    )
    .await
    {
        let status = proxied.status();
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            return Ok(Json(v));
        }
        return Err((status, text).into_response());
    }
    if !matches!(
        req.task.as_str(),
        "redis" | "http" | "pdf" | "wal" | "proxy" | "pubsub" | "binproto"
    ) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "Unknown task '{}': expected redis, http, pdf, wal, proxy, pubsub, or binproto",
                req.task
            ),
        )
            .into_response());
    }
    // Dedupe: an identical PENDING cell
    // (model+task+effort+harness+trials+budget) returns the existing id
    // instead of double-spend. Harness is in the key: omp and opencode
    // cells are different attempts, never interchangeable. Running cells
    // are never deduped (different attempt, different spend). `req` IS the
    // RunRequest here; the enum wraps at push time below.
    {
        let rr = &req;
        let dup = state
            .queue
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.status == QueueStatus::Pending)
            .find(|i| match &i.kind {
                QueueKind::Run(o) => {
                    o.model == rr.model
                        && o.task == rr.task
                        && o.effort == rr.effort
                        && o.harness == rr.harness
                        && o.trials == rr.trials
                        && (o.budget_usd - rr.budget_usd).abs() < 1e-9
                }
                _ => false,
            })
            .map(|i| i.id.clone());
        if let Some(id) = dup {
            return Ok(Json(serde_json::json!({
                "id": id,
                "started": false,
                "position": state
                    .queue
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|i| i.status == QueueStatus::Pending)
                    .position(|i| i.id == id)
                    .map(|p| p + 1),
                "deduped": true,
            })));
        }
    }
    let id = push_queue_item(&state, QueueKind::Run(req));
    let started = kick_runner(&state, &id).await;
    let position = state
        .queue
        .lock()
        .unwrap()
        .iter()
        .filter(|i| i.status == QueueStatus::Pending)
        .position(|i| i.id == id)
        .map(|p| p + 1);
    Ok(Json(serde_json::json!({
        "id": id,
        "started": started,
        "position": position,
    })))
}

pub(crate) async fn remove_queued_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<String, Response> {
    if let Some(proxied) = proxy_mutation(&state, "DELETE", &format!("/api/queue/{id}"), None).await
    {
        let status = proxied.status();
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() {
            return Ok(text);
        }
        return Err((status, text).into_response());
    }
    let removed = {
        let mut q = state.queue.lock().unwrap();
        match q.iter().position(|i| i.id == id) {
            Some(pos) if q[pos].status == QueueStatus::Pending => {
                q.remove(pos);
                true
            }
            Some(_) => {
                return Err((
                    axum::http::StatusCode::CONFLICT,
                    format!("{id} is already running or finished"),
                )
                    .into_response())
            }
            None => {
                return Err((
                    axum::http::StatusCode::NOT_FOUND,
                    format!("{id} not in queue"),
                )
                    .into_response())
            }
        }
    };
    if removed {
        let rid = id.clone();
        db_mirror(&state, "remove queued", move |store| {
            db_try(&format!("remove {rid}"), store.remove_queued(&rid));
        });
        Ok(format!("Removed {id} from queue"))
    } else {
        Err((
            axum::http::StatusCode::NOT_FOUND,
            format!("{id} not in queue"),
        )
            .into_response())
    }
}

pub(crate) async fn clear_queue(State(state): State<AppState>) -> String {
    if let Some(proxied) = proxy_mutation(&state, "DELETE", "/api/queue", None).await {
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        return String::from_utf8_lossy(&bytes).to_string();
    }
    // Drops finished history only; pending work is never discarded here.
    let n_before = state.queue.lock().unwrap().len();
    {
        let mut q = state.queue.lock().unwrap();
        q.retain(|i| i.status == QueueStatus::Pending || i.status == QueueStatus::Running);
    }
    db_mirror(&state, "clear finished", |store| {
        db_try("clear finished", store.clear_finished_queue().map(|_| ()));
    });
    let n_after = state.queue.lock().unwrap().len();
    format!("Cleared {} finished run(s)", n_before - n_after)
}

pub(crate) async fn start_run(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Result<String, Response> {
    if let Some(proxied) = proxy_mutation(
        &state,
        "POST",
        "/api/run",
        Some(serde_json::to_value(&req).unwrap_or_default()),
    )
    .await
    {
        let status = proxied.status();
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() {
            return Ok(text);
        }
        return Err((status, text).into_response());
    }
    let start_worker = match claim_slot(&state) {
        Some(w) => w,
        None => {
            return Err((
                axum::http::StatusCode::CONFLICT,
                "Benchmark is already running (all workers busy)",
            )
                .into_response());
        }
    };

    if !matches!(
        req.task.as_str(),
        "redis" | "http" | "pdf" | "wal" | "proxy" | "pubsub" | "binproto"
    ) {
        release_slot(&state, start_worker);
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "Unknown task '{}': expected redis, http, pdf, wal, proxy, pubsub, or binproto",
                req.task
            ),
        )
            .into_response());
    }

    state.cancel_requested.store(false, Ordering::SeqCst);
    spawn_run_body(&state, req, None, start_worker);

    Ok("Benchmark started".to_string())
}

pub(crate) async fn stream_logs(
    State(state): State<AppState>,
) -> Sse<std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>>> {
    if !state.engine_url.is_empty() {
        // Proxy mode: one-shot snapshot of the engine console tail as SSE
        // events (the GUI also polls /api/console every 2s = the live path).
        let url = format!(
            "{}/api/console?tail=200",
            state.engine_url.trim_end_matches('/')
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        let text = match client.get(&url).send().await {
            Ok(resp) => resp
                .text()
                .await
                .unwrap_or_else(|_| "[STREAM NOTICE] engine console unreadable".to_string()),
            Err(_) => "[STREAM NOTICE] engine console down".to_string(),
        };
        let evts: Vec<Result<Event, Infallible>> =
            text.lines().map(|l| Ok(Event::default().data(l))).collect();
        return Sse::new(Box::pin(tokio_stream::iter(evts))
            as std::pin::Pin<
                Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
            >)
        .keep_alive(KeepAlive::default());
    }
    let rx = state.log_sender.subscribe();

    let initial_events: Vec<Event> = {
        let buf = state.log_buffer.read().unwrap();
        let mut evts = Vec::new();
        for item in buf.iter() {
            for line in item.lines() {
                let clean = line.trim_end();
                if !clean.is_empty() {
                    evts.push(Event::default().data(clean));
                }
            }
        }
        evts
    };
    let initial_stream = tokio_stream::iter(initial_events.into_iter().map(Ok));

    let live_stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(line) => {
            let clean = line.replace("\r", "").replace("\n", " ");
            Some(Ok(Event::default().data(clean)))
        }
        Err(_) => Some(Ok(Event::default().data(
            "[STREAM NOTICE] Event burst: log stream caught up (full log preserved in console)",
        ))),
    });

    let combined = initial_stream.chain(live_stream);
    Sse::new(Box::pin(combined)
        as std::pin::Pin<
            Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
        >)
    .keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
pub struct UpdateOmpRequest {
    pub version: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct OpencodeHarnessStatus {
    pub version: String,
    pub image_exists: bool,
}

pub(crate) async fn get_sandbox_opencode_status() -> Json<OpencodeHarnessStatus> {
    Json(OpencodeHarnessStatus {
        version: crate::sandbox::OPENCODE_VERSION.to_string(),
        image_exists: crate::sandbox::omp_updater::check_image_exists(),
    })
}

pub(crate) async fn get_sandbox_omp_status() -> Json<crate::sandbox::OmpVersionStatus> {
    let status = crate::sandbox::omp_updater::check_status(false).await;
    Json(status)
}

pub(crate) async fn update_sandbox_omp(
    payload: Option<Json<UpdateOmpRequest>>,
) -> Result<Json<crate::sandbox::OmpVersionStatus>, (StatusCode, String)> {
    let target = payload.and_then(|p| p.0.version);
    match crate::sandbox::omp_updater::update_sandbox_image(target.as_deref()).await {
        Ok(status) => Ok(Json(status)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    /// Shared probe bearer for UI-gate tests. Parallel tests share process
    /// env, so ALL of them use one value — different per-test tokens race
    /// (one test's set_var overwrites another's mid-flight → 401 flake).
    const TEST_UI_TOKEN: &str = "sdb-test-ui-token";
    use crate::report::RunTokenUsage;
    use tokio::net::TcpListener;

    /// Round-11 O1: a taken port must fail the bind *before* any side
    /// effect — no queue pump, no reconcile, no OMP updater. The systemd
    /// units crash-looped ~19k times doing those before a failing bind.
    #[tokio::test]
    async fn test_ui_start_fails_before_side_effects_on_a_taken_port() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        let before = UI_STARTS.load(Ordering::SeqCst);
        let err = UiServer::start("127.0.0.1", port, "")
            .await
            .expect_err("a taken port must refuse to start");
        let msg = format!("{err:#}");
        assert!(msg.contains("cannot bind"), "{msg}");
        assert!(msg.contains(&port.to_string()), "{msg}");
        assert_eq!(
            UI_STARTS.load(Ordering::SeqCst),
            before,
            "a failed bind must not start background work"
        );
    }

    #[tokio::test]
    async fn test_dispatch_clears_stale_cancel_flag() {
        // Regression: a `stop` during run N left cancel_requested=true and
        // the next queued run aborted all trials before trial 1 (15 cells
        // with 0-trial Dones). Dispatch must clear the flag per run.
        let state = create_test_state();
        state.cancel_requested.store(true, Ordering::SeqCst);
        let req = RunRequest {
            model: "m".to_string(),
            task: "redis".to_string(),
            api_key: None,
            budget_usd: 1.0,
            max_turns: 1,
            eval_only: false,
            effort: Some("high".to_string()),
            harness: None,
            timeout_min: None,
            trials: 1,
            extend_if_unstable: false,
            max_trials: 1,
            save_results: Some(false),
            review_model: None,
            stop_on_pass: false,
            protocol: None,
            feedback_rounds: None,
        };
        spawn_run_body(&state, req, Some("q-test-cancel".to_string()), 0);
        assert!(!state.cancel_requested.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_billing_pause_blocks_dispatch_and_resume_restores() {
        // End-to-end breaker chain (mock, no spend): pause -> pump/kick refuse,
        // resume -> dispatch works. Proves the wiring, not just classification.
        let state = create_test_state();
        // Seed one pending item directly.
        let id = push_queue_item(&state, QueueKind::Run(queue_test_req()));
        pause_queue(&state, "mock 402 in test");
        assert!(state.queue_paused.load(Ordering::SeqCst));
        assert!(!pump_queue(&state));
        assert!(!kick_runner(&state, &id).await);
        // Item untouched.
        let q = test_queue_of(&state).await;
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].status, QueueStatus::Pending);
        resume_queue(&state);
        assert!(!state.queue_paused.load(Ordering::SeqCst));
    }

    #[test]
    fn test_dispatch_ready_respects_backoff() {
        let mut item = test_queue_item("qr");
        assert!(dispatch_ready(&item, "2026-10-07T00:00:00Z"));
        item.not_before = Some("2030-01-01T00:00:00Z".to_string());
        assert!(!dispatch_ready(&item, "2026-10-07T00:00:00Z"));
        assert!(dispatch_ready(&item, "2031-06-01T00:00:00Z"));
    }

    #[tokio::test]
    async fn test_park_refused_parks_then_goes_final() {
        let state = create_test_state();
        let id = push_queue_item(&state, QueueKind::Run(queue_test_req()));
        for n in 1..=MAX_REFUSALS {
            assert!(
                !park_refused(&state, &id, "load gate"),
                "attempt {n} must park"
            );
            let q = state.queue.lock().unwrap();
            let item = q.iter().find(|i| i.id == id).unwrap();
            assert_eq!(item.status, QueueStatus::Pending);
            assert_eq!(item.refusals, n);
            assert!(item
                .note
                .as_deref()
                .unwrap_or("")
                .contains(&format!("attempt {n}/{MAX_REFUSALS}")));
            assert!(item
                .not_before
                .as_deref()
                .is_some_and(|nb| nb > "2026-01-01T00:00:00Z"));
        }
        assert!(
            park_refused(&state, &id, "load gate"),
            "past cap must go final"
        );
        let q = state.queue.lock().unwrap();
        let item = q.iter().find(|i| i.id == id).unwrap();
        assert_eq!(item.status, QueueStatus::Done);
        assert!(item
            .note
            .as_deref()
            .unwrap_or("")
            .starts_with("refused-final"));
    }

    #[tokio::test]
    async fn test_mark_done_records_reason() {
        let state = create_test_state();
        let id = push_queue_item(&state, QueueKind::Run(queue_test_req()));
        mark_queue_done(&state, &id, Some("7/9 trials passing"));
        let q = state.queue.lock().unwrap();
        let item = q.iter().find(|i| i.id == id).unwrap();
        assert_eq!(item.status, QueueStatus::Done);
        assert_eq!(item.note.as_deref(), Some("7/9 trials passing"));
    }

    #[tokio::test]
    async fn test_worker_buffers_separate_lines_per_worker() {
        // Two workers logging concurrently must not interleave or wipe:
        // regression test for the shared-buffer trial-1 clear.
        let state = create_test_state();
        push_worker_line(&state.worker_buffers, 0, "w0-a".to_string());
        push_worker_line(&state.worker_buffers, 1, "w1-a".to_string());
        push_worker_line(&state.worker_buffers, 0, "w0-b".to_string());
        let bufs = state.worker_buffers.read().unwrap();
        assert_eq!(
            bufs.get(&0).unwrap(),
            &vec!["w0-a".to_string(), "w0-b".to_string()]
        );
        assert_eq!(bufs.get(&1).unwrap(), &vec!["w1-a".to_string()]);
    }

    #[tokio::test]
    async fn test_console_worker_param_serves_one_worker() {
        use axum::extract::{Query, State};
        let state = create_test_state();
        push_worker_line(&state.worker_buffers, 0, "w0-line".to_string());
        push_worker_line(&state.worker_buffers, 1, "w1-line".to_string());
        push_console_line(&state.log_buffer, "w0-line".to_string());
        push_console_line(&state.log_buffer, "w1-line".to_string());
        let q = Query(ConsoleQuery {
            worker: Some(1),
            tail: None,
            since: None,
        });
        let req = axum::extract::Request::builder()
            .uri("/api/console")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = get_active_console(State(state.clone()), q, req).await;
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(text.contains("w1-line"), "worker view must have w1 lines");
        assert!(
            !text.contains("w0-line"),
            "worker view must not leak w0 lines"
        );
    }

    #[tokio::test]
    async fn test_claim_slot_clears_worker_console() {
        let state = create_test_state();
        push_worker_line(&state.worker_buffers, 0, "stale".to_string());
        let w = claim_slot(&state).expect("slot");
        assert_eq!(w, 0);
        assert!(state.worker_buffers.read().unwrap().get(&0).is_none());
        release_slot(&state, w);
    }

    async fn test_queue_of(state: &AppState) -> Vec<QueuedRun> {
        let resp = get_queue(State(state.clone())).await;
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Point this test process at an isolated runs dir, once (round-8
    /// finding 1): the suite must never read, write or delete anything in
    /// the real runs/ directory while a cell is live.
    pub(crate) fn isolate_test_runs_dir() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("sdb_test_runs_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("SDB_RUNS_DIR", &dir);
            // Round-12 P1: the portal output is isolated the same way, or a
            // test that runs the pipeline would overwrite the served board.
            let portal =
                std::env::temp_dir().join(format!("sdb_test_portal_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&portal);
            let _ = std::fs::create_dir_all(&portal);
            std::env::set_var("SDB_PORTAL_DIR", &portal);
        });
    }

    fn create_test_state() -> AppState {
        isolate_test_runs_dir();
        let (log_sender, _) = broadcast::channel(100);
        AppState {
            log_sender,
            is_running: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            current_run: Arc::new(RwLock::new(None)),
            last_run: Arc::new(RwLock::new(None)),
            log_buffer: Arc::new(RwLock::new(Vec::new())),
            task_handle: Arc::new(std::sync::Mutex::new(None)),
            queue: Arc::new(std::sync::Mutex::new(VecDeque::new())),
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

    /// R28 F1: the rules/status endpoint and a run's detail carry the
    /// linked SQLite oracle version.
    #[tokio::test]
    async fn r28_f1_detail_names_the_oracle_version() {
        let base = serve_test_app(create_test_state()).await;
        let client = authed_client();
        let status: serde_json::Value = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["sqlite_version"], rusqlite::version());
        let mut recorded = test_manifest("r28_f1_oracle");
        recorded.sqlite_version = Some("3.0.0".to_string());
        recorded.cap_price_basis = Some("recorded cap basis".to_string());
        recorded.rules_version = Some("2026-10-19.40".to_string());
        let detail = manifest_with_portal_flag(&recorded);
        assert_eq!(detail["sqlite_version"], "3.0.0");
        assert_eq!(detail["cap_price_basis"], "recorded cap basis");
        let mut old = test_manifest("r28_f1_old");
        old.rules_version = Some("2026-10-19.38".to_string());
        let old_detail = manifest_with_portal_flag(&old);
        assert!(
            old_detail.get("sqlite_version").is_none(),
            "an old run must not borrow today's oracle"
        );
        assert!(crate::report::rules::recorded_cap_basis("2026-10-19.38", None).is_none());
    }

    /// R28 F5: the cap basis is served and reaches the Board shell.
    #[tokio::test]
    async fn r28_f5_cap_basis_is_rendered() {
        let base = serve_test_app(create_test_state()).await;
        let client = authed_client();
        let status: serde_json::Value = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(status["cap_price_basis"]
            .as_str()
            .unwrap_or_default()
            .contains("list price"));
        let page = client
            .get(format!("{base}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(page.contains("__SDB_CAP_BASIS__"), "Board misses the basis");
        assert!(
            page.contains("list price"),
            "Board misses the list-price text"
        );
    }

    #[tokio::test]
    async fn status_response_reports_db_fallback_flag() {
        // get_status surfaces AppState::db_fallback so the GUI can show a
        // degraded-durability banner; default (false) keeps old shape.
        let mut state = create_test_state();
        assert!(!state.db_fallback);
        let resp = get_status(State(state.clone())).await;
        {
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v.get("db_fallback"), Some(&serde_json::Value::Bool(false)));
        }
        state.db_fallback = true;
        let resp = get_status(State(state)).await;
        {
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v.get("db_fallback"), Some(&serde_json::Value::Bool(true)));
        }
    }

    #[test]
    fn queue_replay_resets_running_and_seq() {
        let store = tempfile_mem_store();
        let mut running = crate::web::server::test_queue_item("q7");
        running.status = QueueStatus::Running;
        store.enqueue_run(&running).expect("enqueue");
        store
            .update_queue_status("q7", "Running", Some("2026-09-25T00:00:01Z"), None)
            .expect("mark running");
        store
            .enqueue_run(&crate::web::server::test_queue_item("q8"))
            .expect("enqueue");
        let replayed = replay_queue(&store);
        assert_eq!(replayed.len(), 2);
        assert!(replayed.iter().all(|i| i.status == QueueStatus::Pending));
        assert_eq!(queue_seq_start(&store), 8);
        // Seq survives a clean restart: finish everything so the replay is
        // empty, but the max still comes from finished history (never q1).
        store
            .update_queue_status("q7", "Done", None, Some("2026-09-25T00:02:00Z"))
            .expect("done q7");
        store
            .update_queue_status("q8", "Done", None, Some("2026-09-25T00:02:00Z"))
            .expect("done q8");
        assert!(replay_queue(&store).is_empty());
        assert_eq!(queue_seq_start(&store), 8);
    }

    fn queue_test_req() -> RunRequest {
        RunRequest {
            model: "test_model".to_string(),
            task: "wal".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 5,
            eval_only: true,
            effort: Some("low".to_string()),
            harness: None,
            timeout_min: Some(1),
            trials: 1,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
            stop_on_pass: false,
            protocol: None,
            feedback_rounds: None,
        }
    }

    fn extend_test_body() -> ExtendBody {
        ExtendBody {
            trials: Some(3),
            max_turns: None,
            budget_usd: None,
            timeout_min: None,
            eval_only: None,
            api_key: None,
            queue: false,
        }
    }

    #[test]
    fn test_extend_request_carries_caller_key() {
        // Queue-path construction (mirrors extend_parent_run): the caller
        // key rides along; absent keys stay None for env fallback.
        let with_key = ExtendRequest {
            parent_run_id: "p".to_string(),
            trials: 3,
            max_turns: None,
            budget_usd: None,
            timeout_min: None,
            eval_only: None,
            api_key: Some("sk-test-key".to_string()),
        };
        assert_eq!(with_key.api_key.as_deref(), Some("sk-test-key"));
        // Pre-key payloads still deserialize (serde default).
        let legacy: ExtendRequest =
            serde_json::from_str(r#"{"parent_run_id":"p","trials":3}"#).unwrap();
        assert_eq!(legacy.api_key, None);
    }

    #[test]
    fn test_reconcile_reaps_ghost_running_with_idle_engine() {
        let state = create_test_state();
        state.queue.lock().unwrap().push_back(QueuedRun {
            id: "q99".to_string(),
            enqueued_at: "2026-10-02T00:00:00Z".to_string(),
            status: QueueStatus::Running,
            note: None,
            refusals: 0,
            not_before: None,
            kind: QueueKind::Run(queue_test_req()),
        });
        assert_eq!(reconcile_queue(&state), 1);
        let q = state.queue.lock().unwrap();
        assert_eq!(
            q.iter().find(|i| i.id == "q99").map(|i| i.status),
            Some(QueueStatus::Cancelled)
        );
    }

    #[test]
    fn test_reconcile_keeps_running_when_worker_live() {
        let state = create_test_state();
        state.running_slots.lock().unwrap().insert(0);
        state.queue.lock().unwrap().push_back(QueuedRun {
            id: "q100".to_string(),
            enqueued_at: "2026-10-02T00:00:00Z".to_string(),
            status: QueueStatus::Running,
            note: None,
            refusals: 0,
            not_before: None,
            kind: QueueKind::Run(queue_test_req()),
        });
        assert_eq!(reconcile_queue(&state), 0);
        let q = state.queue.lock().unwrap();
        assert_eq!(
            q.iter().find(|i| i.id == "q100").map(|i| i.status),
            Some(QueueStatus::Running)
        );
    }

    #[tokio::test]
    async fn status_truth_idle_engine_reports_not_running() {
        // Ghost aftermath: sticky is_running flag true, but no workers,
        // no slots, no handles. Status must derive from live state.
        let state = create_test_state();
        state.is_running.store(true, Ordering::SeqCst);
        let resp = get_status(State(state.clone())).await;
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.get("is_running"), Some(&serde_json::Value::Bool(false)));
    }

    #[tokio::test]
    async fn status_truth_live_slot_reports_running() {
        let state = create_test_state();
        state.running_slots.lock().unwrap().insert(0);
        let resp = get_status(State(state.clone())).await;
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.get("is_running"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn test_reconcile_leaves_pending_and_done_alone() {
        let state = create_test_state();
        for (id, st) in [("q101", QueueStatus::Pending), ("q102", QueueStatus::Done)] {
            state.queue.lock().unwrap().push_back(QueuedRun {
                id: id.to_string(),
                enqueued_at: "2026-10-02T00:00:00Z".to_string(),
                status: st,
                note: None,
                refusals: 0,
                not_before: None,
                kind: QueueKind::Run(queue_test_req()),
            });
        }
        assert_eq!(reconcile_queue(&state), 0);
    }

    /// Skipped extends must free the worker slot (release_slot), not leak
    /// it — and must not clear is_running under a live sibling. Reproduced:
    /// not-multi-trial + stable skips cleared task_handle/is_running(false)
    /// while holding the slot: slots leaked, flag lied under live workers.
    /// Uses real results files (execute_extend_body reads <root>/results).
    async fn extend_skip_state(
        parent_id: &str,
        doc: serde_json::Value,
    ) -> (AppState, u8, ExtendRequest, String) {
        // Write into the real results dir (unique suffix, removed at test
        // end) like test_extend_queue_unstable_parent_pending does. Never
        // SDB_HOME: process-global env races parallel tests sharing one
        // process (observed: unstable_parent_pending failed at :4033 when
        // run in the full suite, green in isolation).
        let real = crate::config::get_repo_root().join("results");
        let _ = std::fs::create_dir_all(&real);
        std::fs::write(
            real.join(format!("{parent_id}.json")),
            serde_json::to_string(&doc).unwrap(),
        )
        .unwrap();
        let state = create_test_state();
        // Claim worker 0 for the extend; hold worker 1 as a live sibling.
        state.running_slots.lock().unwrap().insert(0);
        state.running_slots.lock().unwrap().insert(1);
        state.is_running.store(true, Ordering::SeqCst);
        let ext = ExtendRequest {
            parent_run_id: parent_id.to_string(),
            trials: 3,
            max_turns: None,
            budget_usd: None,
            timeout_min: None,
            eval_only: None,
            api_key: None,
        };
        let qid = "qskip".to_string();
        state.queue.lock().unwrap().push_back(QueuedRun {
            id: qid.clone(),
            enqueued_at: "2026-10-02T00:00:00Z".to_string(),
            status: QueueStatus::Running,
            note: None,
            refusals: 0,
            not_before: None,
            kind: QueueKind::Extend(ext.clone()),
        });
        (state, 0, ext, qid)
    }

    fn two_trial_doc(pass: f64) -> serde_json::Value {
        let sub = |id: &str| {
            serde_json::json!({
                "id": id, "model": "m", "task": "redis", "language": "Go",
                "pass_rate": pass, "passed_stages": 4, "total_stages": 4,
                "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0,
                "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 1.0,
                "timestamp": "2026-09-12T08:00:00Z", "subruns": []
            })
        };
        serde_json::json!({
            "id": "qp", "model": "m", "task": "redis", "language": "Go",
            "pass_rate": pass, "passed_stages": 4, "total_stages": 4,
            "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0,
            "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 1.0,
            "timestamp": "2026-09-12T08:00:00Z",
            "subruns": [sub("qp_trial1"), sub("qp_trial2")]
        })
    }

    #[tokio::test]
    async fn test_extend_skip_not_multitrial_frees_slot_keeps_running() {
        // Single-subrun parent: not multi-trial skip.
        let mut doc = two_trial_doc(100.0);
        doc["subruns"] = serde_json::Value::Array(vec![doc["subruns"][0].clone()]);
        let (state, worker, ext, qid) = extend_skip_state("qp_single", doc).await;
        execute_extend_body(state.clone(), ext, qid.clone(), worker).await;
        // Slot 0 freed, sibling slot 1 still held; is_running stays true.
        assert!(!state.running_slots.lock().unwrap().contains(&0));
        assert!(state.running_slots.lock().unwrap().contains(&1));
        assert!(state.is_running.load(Ordering::SeqCst));
        assert_eq!(
            state
                .queue
                .lock()
                .unwrap()
                .iter()
                .find(|i| i.id == qid)
                .map(|i| i.status),
            Some(QueueStatus::Done)
        );
        let _ = std::fs::remove_file(
            crate::config::get_repo_root()
                .join("results")
                .join("qp_single.json"),
        );
    }

    #[tokio::test]
    async fn test_extend_skip_stable_frees_slot_keeps_running() {
        // Two passing trials: stable skip.
        let (state, worker, ext, qid) = extend_skip_state("qp_stable", two_trial_doc(100.0)).await;
        execute_extend_body(state.clone(), ext, qid.clone(), worker).await;
        assert!(!state.running_slots.lock().unwrap().contains(&0));
        assert!(state.running_slots.lock().unwrap().contains(&1));
        assert!(state.is_running.load(Ordering::SeqCst));
        assert_eq!(
            state
                .queue
                .lock()
                .unwrap()
                .iter()
                .find(|i| i.id == qid)
                .map(|i| i.status),
            Some(QueueStatus::Done)
        );
        let _ = std::fs::remove_file(
            crate::config::get_repo_root()
                .join("results")
                .join("qp_stable.json"),
        );
    }

    #[tokio::test]
    async fn test_extend_busy_conflict() {
        let state = create_test_state();
        state.is_running.store(true, Ordering::SeqCst);
        let res = extend_parent_run(
            State(state.clone()),
            AxumPath("whatever".to_string()),
            Json(extend_test_body()),
        )
        .await;
        assert!(res.is_err());
        assert!(state.is_running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_extend_unknown_parent_404_and_idle() {
        let state = create_test_state();
        let res = extend_parent_run(
            State(state.clone()),
            AxumPath("no-such-run".to_string()),
            Json(extend_test_body()),
        )
        .await;
        assert!(res.is_err());
        assert!(!state.is_running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_queue_enqueue_while_busy_stays_pending() {
        let state = create_test_state();
        // Busy = all worker slots claimed (legacy flag alone no longer blocks).
        for w in 0..worker_count() as u8 {
            state.running_slots.lock().unwrap().insert(w);
        }
        state.is_running.store(true, Ordering::SeqCst);
        let res = enqueue_run(State(state.clone()), Json(queue_test_req())).await;
        assert!(res.is_ok());
        let body = res.unwrap().0;
        assert_eq!(body["started"], false);
        let q = test_queue_of(&state).await;
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].status, QueueStatus::Pending);
        assert!(q[0].id.starts_with('q'));
    }

    fn extend_queue_body() -> ExtendBody {
        ExtendBody {
            trials: Some(3),
            max_turns: None,
            budget_usd: None,
            timeout_min: None,
            eval_only: None,
            api_key: None,
            queue: true,
        }
    }

    #[tokio::test]
    async fn test_extend_queue_missing_parent_404() {
        let state = create_test_state();
        state.is_running.store(true, Ordering::SeqCst);
        let res = extend_parent_run(
            State(state.clone()),
            AxumPath("qtest_parent".to_string()),
            Json(extend_queue_body()),
        )
        .await;
        assert!(res.is_err());
        // Slot untouched: still busy, queue empty.
        assert!(state.is_running.load(Ordering::SeqCst));
        assert!(test_queue_of(&state).await.is_empty());
    }

    #[tokio::test]
    async fn test_extend_queue_unstable_parent_pending() {
        // Unstable = pass spread across trials (100, 0): needs more trials.
        let dir = std::env::temp_dir().join(format!("qtest_res_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let subs = serde_json::json!([
            {"id": "qp_trial1", "model": "m", "task": "redis", "language": "Go",
             "pass_rate": 100.0, "passed_stages": 6, "total_stages": 6,
             "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0,
             "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 1.0,
             "timestamp": "2026-09-12T08:00:00Z"},
            {"id": "qp_trial2", "model": "m", "task": "redis", "language": "Go",
             "pass_rate": 0.0, "passed_stages": 0, "total_stages": 6,
             "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0,
             "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 1.0,
             "timestamp": "2026-09-12T08:00:00Z"}
        ]);
        let v = serde_json::json!({
            "id": "qp", "model": "m", "task": "redis", "language": "Go",
            "pass_rate": 50.0, "passed_stages": 3, "total_stages": 6,
            "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0,
            "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 1.0,
            "timestamp": "2026-09-12T08:00:00Z", "subruns": subs,
        });
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("qp.json"), serde_json::to_string(&v).unwrap()).unwrap();
        // Point SDB_HOME at the temp dir so resolve uses it... instead:
        // cheap_check uses get_repo_root; emulate by writing into the real
        // results dir and cleaning up (suffix is unique to this test).
        let real = crate::config::get_repo_root().join("results");
        let _ = std::fs::create_dir_all(&real);
        std::fs::write(
            real.join("qp_qtest_unstable.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "qp_qtest_unstable", "model": "m", "task": "redis", "language": "Go",
                "pass_rate": 50.0, "passed_stages": 3, "total_stages": 6,
                "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0,
                "total_cost_usd": 0.01, "savings_percent": 0.0, "efficiency_score": 1.0,
                "timestamp": "2026-09-12T08:00:00Z", "subruns": subs,
            }))
            .unwrap(),
        )
        .unwrap();
        let state = create_test_state();
        state.is_running.store(true, Ordering::SeqCst);
        let res = extend_parent_run(
            State(state.clone()),
            AxumPath("qp_qtest_unstable".to_string()),
            Json(extend_queue_body()),
        )
        .await;
        assert!(res.is_ok(), "unstable parent must queue while busy");
        let body = res.unwrap().0;
        assert!(body.get("queued").is_some());
        let q = test_queue_of(&state).await;
        assert_eq!(q.len(), 1);
        assert!(matches!(q[0].kind, QueueKind::Extend(_)));
        // Pending extends are removable like runs.
        assert!(
            remove_queued_run(State(state.clone()), AxumPath(q[0].id.clone()))
                .await
                .is_ok()
        );
        let _ = std::fs::remove_file(real.join("qp_qtest_unstable.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refused run must unwind like every other early return in the engine.
    ///
    /// 514cb0b's predecessor (c7981ad) marked the queue item cancelled and
    /// returned without `release_slot`, so each refusal leaked a worker slot.
    /// On the default 4-slot grid four refused cells wedged the wave -
    /// `pump_queue` can never reclaim a slot nobody released - and
    /// `is_running` stayed true, so `/api/status` reported a phantom run
    /// forever. `test_queue_item` already builds a `task: "wal"` request; the
    /// refusal under test comes from the unknown-task gate.
    #[tokio::test]
    async fn test_refused_run_releases_its_slot() {
        let state = create_test_state();
        // One more item than slots, so the accumulation case is reachable: with
        // the leak, iteration 4 holds the last slot and iteration 5's
        // `claim_slot` returns None. The per-iteration assert catches the leak
        // a whole iteration earlier, with a clearer message than a panic from
        // `expect`.
        for i in 0..(worker_count() + 1) {
            let id = format!("q_unknown_refuse_{i}");
            let item = test_queue_item(&id);
            let mut req = item.run_request().expect("fixture is a Run").clone();
            req.task = "no-such-task".to_string();
            state.queue.lock().unwrap().push_back(item);

            // Dispatch exactly the way the queue does.
            let worker = claim_slot(&state).expect("a slot should be free");
            spawn_run_body(&state, req, Some(id.clone()), worker);

            // Snapshot before asserting. Locking `running_slots` twice in one
            // statement deadlocks the moment the predicate is false - the
            // format arg is only evaluated on failure, so the hang would
            // appear exactly when the bug regresses and the message is wanted.
            let held: Vec<u8> = state
                .running_slots
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect();
            assert!(
                held.is_empty(),
                "refused run {i} leaked worker slot {worker}; still held: {held:?}"
            );
        }
        // Accumulation: every slot released, so the grid is not wedged.
        let left: Vec<u8> = state
            .running_slots
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect();
        assert!(
            left.is_empty(),
            "slots still held after {n} refusals: {left:?} - the grid would wedge",
            n = worker_count() + 1
        );
        assert!(
            !state.is_running.load(Ordering::SeqCst),
            "is_running must clear once the last slot frees"
        );
        let logged = state
            .log_buffer
            .read()
            .unwrap()
            .iter()
            .any(|l| l.contains("refusing run") && l.contains("unknown task"));
        assert!(
            logged,
            "the refusal reason must reach the console, not just stderr"
        );
    }

    #[tokio::test]
    async fn test_queue_remove_and_clear() {
        let state = create_test_state();
        for w in 0..worker_count() as u8 {
            state.running_slots.lock().unwrap().insert(w);
        }
        state.is_running.store(true, Ordering::SeqCst);
        let a = enqueue_run(State(state.clone()), Json(queue_test_req()))
            .await
            .unwrap()
            .0;
        let b = enqueue_run(State(state.clone()), Json(queue_test_req()))
            .await
            .unwrap()
            .0;
        // Dedupe: identical pending cell returns the existing id with
        // deduped=true instead of double-spend.
        assert_eq!(
            b.get("deduped"),
            Some(&serde_json::Value::Bool(true)),
            "second identical enqueue must dedupe"
        );
        assert_eq!(b.get("id"), a.get("id"));
        let ida = a["id"].as_str().unwrap().to_string();
        let _idb = b["id"].as_str().unwrap().to_string();
        assert!(
            remove_queued_run(State(state.clone()), AxumPath(ida.clone()))
                .await
                .is_ok()
        );
        // Removing twice -> not found.
        assert!(
            remove_queued_run(State(state.clone()), AxumPath(ida.clone()))
                .await
                .is_err()
        );
        // A DISTINCT cell (different trials) enqueues separately.
        let mut req2 = queue_test_req();
        req2.trials = 3;
        let c = enqueue_run(State(state.clone()), Json(req2))
            .await
            .unwrap()
            .0;
        let idc = c["id"].as_str().unwrap().to_string();
        assert_ne!(idc, ida, "distinct cell must get its own id");
        // A DISTINCT cell (different budget) enqueues separately too —
        // the dedupe key covers budget, so a raised budget is never
        // silently folded into a cheaper cell.
        let mut req3 = queue_test_req();
        req3.budget_usd = 99.0;
        let d = enqueue_run(State(state.clone()), Json(req3))
            .await
            .unwrap()
            .0;
        assert_eq!(d.get("deduped"), None);
        assert_ne!(
            d["id"].as_str().unwrap(),
            ida,
            "different budget must get its own id"
        );
        // A DISTINCT cell (different harness) enqueues separately too —
        // omp and opencode cells are different attempts, never folded.
        let mut req4 = queue_test_req();
        req4.harness = Some("opencode".to_string());
        let e = enqueue_run(State(state.clone()), Json(req4))
            .await
            .unwrap()
            .0;
        assert_eq!(e.get("deduped"), None);
        assert_ne!(
            e["id"].as_str().unwrap(),
            ida,
            "different harness must get its own id"
        );
        assert!(remove_queued_run(
            State(state.clone()),
            AxumPath(e["id"].as_str().unwrap().to_string())
        )
        .await
        .is_ok());
        assert!(remove_queued_run(
            State(state.clone()),
            AxumPath(d["id"].as_str().unwrap().to_string())
        )
        .await
        .is_ok());
        // Running items cannot be removed.
        {
            let mut q = state.queue.lock().unwrap();
            q.iter_mut().find(|i| i.id == idc).unwrap().status = QueueStatus::Running;
        }
        assert!(remove_queued_run(State(state.clone()), AxumPath(idc))
            .await
            .is_err());
        assert_eq!(test_queue_of(&state).await.len(), 1);
        let msg = clear_queue(State(state.clone())).await;
        assert!(msg.contains("Cleared 0"));
        {
            let mut q = state.queue.lock().unwrap();
            q.iter_mut().for_each(|i| i.status = QueueStatus::Done);
        }
        let msg = clear_queue(State(state.clone())).await;
        assert!(msg.contains("Cleared 1"));
        assert!(test_queue_of(&state).await.is_empty());
    }

    #[test]
    fn test_pass_at_k_computation() {
        assert_eq!(compute_pass_at_k(0, 0, 1), 0.0);
        assert_eq!(compute_pass_at_k(5, 0, 1), 0.0);
        assert_eq!(compute_pass_at_k(5, 5, 1), 1.0);
        assert_eq!(compute_pass_at_k(5, 5, 5), 1.0);
        assert_eq!(compute_pass_at_k(3, 1, 3), 1.0);
        let p1 = compute_pass_at_k(3, 1, 1);
        assert!((p1 - (1.0 / 3.0)).abs() < 1e-4);
    }

    #[test]
    fn test_generate_unified_diff() {
        let diff_empty = generate_unified_diff("test.txt", "abc\n", "abc\n");
        assert!(diff_empty.is_empty());

        let diff_mod = generate_unified_diff("test.txt", "line1\nline2\n", "line1\nline_new\n");
        assert!(diff_mod.contains("--- a/test.txt"));
        assert!(diff_mod.contains("-line2"));
        assert!(diff_mod.contains("+line_new"));

        let diff_add = generate_unified_diff("test.txt", "", "hello\n");
        assert!(diff_add.contains("+hello"));

        let diff_del = generate_unified_diff("test.txt", "world\n", "");
        assert!(diff_del.contains("-world"));
    }

    #[test]
    fn test_portal_content_type_renders_text_inline() {
        assert_eq!(
            portal_content_type(Path::new("server.py"), b"print(1)"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            portal_content_type(Path::new("Dockerfile"), b"FROM golang:1.22"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            portal_content_type(Path::new("run.bin"), &[0, 1, 2, 0]),
            "application/octet-stream"
        );
        assert_eq!(
            portal_content_type(Path::new("index.html"), b"<html>"),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn test_csrf_origin_policy() {
        // curl/CLI (no Origin/Referer) stays allowed; loopback Origins are
        // trusted; cross-site browser Origins are rejected.
        let bare = HeaderMap::new();
        assert!(csrf_origin_allowed(&bare));
        let mut local = HeaderMap::new();
        local.insert(header::ORIGIN, "http://127.0.0.1:3000".parse().unwrap());
        assert!(csrf_origin_allowed(&local));
        let mut host = HeaderMap::new();
        host.insert(header::ORIGIN, "http://localhost:3000".parse().unwrap());
        assert!(csrf_origin_allowed(&host));
        let mut evil = HeaderMap::new();
        evil.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        assert!(!csrf_origin_allowed(&evil));
        // Referer fallback covers same-origin form/DELETE submissions.
        let mut ref_ok = HeaderMap::new();
        ref_ok.insert(
            header::REFERER,
            "http://127.0.0.1:3000/api/queue".parse().unwrap(),
        );
        assert!(csrf_origin_allowed(&ref_ok));
        let mut ref_evil = HeaderMap::new();
        ref_evil.insert(
            header::REFERER,
            "https://evil.example.com/pwn".parse().unwrap(),
        );
        assert!(!csrf_origin_allowed(&ref_evil));
        // Same-origin on a non-loopback host (LAN IP, Tailnet, custom
        // domain): Origin host matches Host header, so the matrix "+"
        // POST /api/queue from http://192.168.x.x:3000 must pass.
        let mut lan = HeaderMap::new();
        lan.insert(header::ORIGIN, "http://192.168.1.10:3000".parse().unwrap());
        lan.insert(header::HOST, "192.168.1.10:3000".parse().unwrap());
        assert!(csrf_origin_allowed(&lan));
        let mut lan_evil = HeaderMap::new();
        lan_evil.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        lan_evil.insert(header::HOST, "192.168.1.10:3000".parse().unwrap());
        assert!(!csrf_origin_allowed(&lan_evil));
        let mut domain = HeaderMap::new();
        domain.insert(header::ORIGIN, "https://bench.example.com".parse().unwrap());
        domain.insert(header::HOST, "bench.example.com".parse().unwrap());
        assert!(csrf_origin_allowed(&domain));
        // Referer variant of the same LAN same-origin case.
        let mut lan_ref = HeaderMap::new();
        lan_ref.insert(
            header::REFERER,
            "http://192.168.1.10:3000/".parse().unwrap(),
        );
        lan_ref.insert(header::HOST, "192.168.1.10:3000".parse().unwrap());
        assert!(csrf_origin_allowed(&lan_ref));
        // IPv6 loopback with brackets was previously misparsed by a naive
        // split(':'); it must stay allowed.
        let mut v6 = HeaderMap::new();
        v6.insert(header::ORIGIN, "http://[::1]:3000".parse().unwrap());
        assert!(csrf_origin_allowed(&v6));
        // Host mismatch without loopback is still cross-site.
        let mut mismatch = HeaderMap::new();
        mismatch.insert(header::ORIGIN, "http://10.0.0.5:3000".parse().unwrap());
        mismatch.insert(header::HOST, "192.168.1.10:3000".parse().unwrap());
        assert!(!csrf_origin_allowed(&mismatch));
    }

    #[test]
    fn test_token_gate_401_shape() {
        // The bearer gate behind csrf_guard's 401: exact token passes,
        // everything else fails closed. (Guard itself needs a live
        // middleware::Next, so these pin the decision fns it calls.)
        let token = "sdb-test-ui-token";
        let bearer = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, v.parse().unwrap());
            h
        };
        assert!(token_ok(&bearer("Bearer sdb-test-ui-token"), token));
        assert!(!token_ok(&bearer("Bearer wrong"), token));
        assert!(!token_ok(&bearer("Bearer "), token));
        assert!(!token_ok(&bearer("Basic c2Ri"), token));
        assert!(!token_ok(&HeaderMap::new(), token));
        // subtle_eq: length leak closed, single-bit flip closed.
        assert!(subtle_eq("abc", "abc"));
        assert!(!subtle_eq("abc", "abd"));
        assert!(!subtle_eq("abc", "abcd"));
        assert!(!subtle_eq("", "x"));
    }

    #[tokio::test]
    async fn test_log_buffer_mirrors_lifecycle_tail() {
        // send_status mirrors every lifecycle line into the console buffer
        // so late SSE joiners and /api/console readers see DONE/STOP tails.
        let (tx, _) = broadcast::channel(16);
        let buf = Arc::new(RwLock::new(Vec::new()));
        send_status(&tx, &buf, "[DONE]".to_string());
        send_status(
            &tx,
            &buf,
            "[STATUS] All trials passed (100% pass rate).".to_string(),
        );
        send_status(&tx, &buf, "[STOP] Run cancelled by user.".to_string());
        {
            let snap = buf.read().unwrap();
            assert_eq!(snap.len(), 3);
            assert!(snap[0].contains("[DONE]"));
            assert!(snap[2].contains("[STOP]"));
        }
        // push_console_line itself caps; mirror a small overflow directly.
        for i in 0..10 {
            push_console_line(&buf, format!("line {i}"));
        }
        assert_eq!(buf.read().unwrap().len(), 3 + 10);
        // Cap behavior itself lives in push_console_line: verified cheaply
        // with a local buffer instead of 3000+ sends.
        let small = Arc::new(RwLock::new(Vec::new()));
        for i in 0..(MAX_CONSOLE_LINES + 10) {
            push_console_line(&small, format!("line {i}"));
        }
        assert_eq!(small.read().unwrap().len(), MAX_CONSOLE_LINES);
    }

    /// `GET /api/run/queue` must not echo a caller's API key back. The route
    /// is unauthenticated, so serializing `QueuedRun` directly handed any
    /// POSTed key to whoever asked.
    #[test]
    fn test_queue_listing_redacts_api_key() {
        let mut q = test_queue_item("q_secret");
        match &mut q.kind {
            QueueKind::Run(r) => r.api_key = Some("sk-or-v1-SUPERSECRET123".to_string()),
            QueueKind::Extend(_) => panic!("fixture is a Run"),
        }
        let json = serde_json::to_string(&q.redacted()).unwrap();
        assert!(
            !json.contains("SUPERSECRET123"),
            "queue listing leaked an API key: {json}"
        );
        // The key must still be on the in-memory entry, or dispatch loses it.
        match &q.kind {
            QueueKind::Run(r) => assert_eq!(r.api_key.as_deref(), Some("sk-or-v1-SUPERSECRET123")),
            QueueKind::Extend(_) => unreachable!(),
        }
    }

    /// Extend entries carry a key too, and go through the same redaction.
    #[test]
    fn test_queue_listing_redacts_extend_api_key() {
        let q = QueuedRun {
            id: "e1".to_string(),
            enqueued_at: "2026-09-25T00:00:00Z".to_string(),
            status: QueueStatus::Pending,
            note: None,
            refusals: 0,
            not_before: None,
            kind: QueueKind::Extend(ExtendRequest {
                parent_run_id: "p1".to_string(),
                trials: 1,
                max_turns: None,
                budget_usd: None,
                timeout_min: None,
                eval_only: None,
                api_key: Some("sk-or-v1-EXTENDSECRET".to_string()),
            }),
        };
        let json = serde_json::to_string(&q.redacted()).unwrap();
        assert!(!json.contains("EXTENDSECRET"), "extend key leaked: {json}");
    }

    /// The stop endpoint is unauthenticated, so a short fragment must not be
    /// able to end-match every live run, and a fragment that names more than
    /// one run must be refused rather than killing all of them.
    #[tokio::test]
    async fn test_stop_rejects_short_and_ambiguous_fragments() {
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        fn active(run_id: &str) -> ActiveRunInfo {
            ActiveRunInfo {
                run_id: run_id.to_string(),
                model: "m".to_string(),
                task: "redis".to_string(),
                effort: "high".to_string(),
                started_at: "2026-09-30T01:01:01Z".to_string(),
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
                max_trials: 7,
                review_model: None,
            }
        }
        let state = create_test_state();
        // Both ids share `_openrouter_` (12 chars) so an ambiguous-but-long
        // fragment exists, and differ in their 15-char timestamp tails.
        for (w, run) in [
            (0u8, "redis_openrouter_aaa_20260930_010101"),
            (1u8, "redis_openrouter_bbb_20260930_020202"),
        ] {
            state.worker_runs.write().unwrap().insert(w, active(run));
        }
        let app = UiServer::build_app(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut _h = reqwest::header::HeaderMap::new();
        _h.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {TEST_UI_TOKEN}")).unwrap(),
        );
        let client = reqwest::Client::builder()
            .default_headers(_h)
            .build()
            .unwrap();
        let stop = |needle: &'static str| {
            let url = format!("http://127.0.0.1:{port}/api/run/stop?run={needle}");
            let client = client.clone();
            async move { client.post(url).send().await.unwrap() }
        };

        // Before anything: two live workers.
        // stop_run refuses everything with 400 when nothing is running, so the
        // fragment guards below are unreachable unless a run is in flight.
        state
            .is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(state.worker_runs.read().unwrap().len(), 2);

        // 1 char: the wildcard-kill primitive. Rejected, nothing touched.
        assert_eq!(
            stop("e").await.status(),
            400,
            "1-char fragment must be rejected"
        );
        assert_eq!(
            state.worker_runs.read().unwrap().len(),
            2,
            "1-char stop killed a run"
        );

        // The predicate is equal / prefix / suffix, not substring, so the
        // ambiguous case must be a shared *prefix* (16 chars) and the unique
        // case a distinct *suffix* (15 chars).
        let res = stop("redis_openrouter_").await;
        assert_eq!(res.status(), 409, "an ambiguous prefix must be refused");
        assert_eq!(
            state.worker_runs.read().unwrap().len(),
            2,
            "an ambiguous fragment must not kill every matching run"
        );

        // 15 chars, unique: accepted, and stops exactly one.
        let res = stop("20260930_020202").await;
        assert_ne!(
            res.status(),
            409,
            "a unique fragment must not be called ambiguous"
        );
        assert_ne!(
            res.status(),
            404,
            "a unique fragment must resolve to its run"
        );
        assert_eq!(
            state.worker_runs.read().unwrap().len(),
            1,
            "exactly one worker should have been stopped"
        );
    }

    #[tokio::test]
    async fn test_run_delete_guards() {
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        let state = create_test_state();
        let app = UiServer::build_app(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut _h = reqwest::header::HeaderMap::new();
        _h.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {TEST_UI_TOKEN}")).unwrap(),
        );
        let client = reqwest::Client::builder()
            .default_headers(_h)
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{}", port);

        // Traversal rejected.
        let res = client
            .delete(format!("{}/api/runs/..%2Ffoo", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // Missing run 404s.
        let res = client
            .delete(format!("{}/api/runs/nonexistent_xyz_123", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_portal_static_routes() {
        let state = create_test_state();
        let app = UiServer::build_app(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{}", port);

        // Bare /portal redirects to /portal/.
        let res = client.get(format!("{}/portal", base)).send().await.unwrap();
        assert_eq!(res.status(), 308);

        // Unknown portal path 404s regardless of export state.
        let res = client
            .get(format!("{}/portal/nonexistent_xyz_123", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // Traversal outside the portal root is rejected.
        let res = client
            .get(format!("{}/portal/../Cargo.toml", base))
            .send()
            .await
            .unwrap();
        assert!(res.status() == 403 || res.status() == 404);
    }

    // Live-fire: boots a real server against the REAL workspace/, runs/
    // and results/ roots (resolve_*_dir have no test override) and
    // remove_dir_all's them at cleanup. A run during a live wave deletes
    // a sibling trial's mounted workdir mid-trial (ENOENT under a live
    // bind mount) — the exact //deleted signature. Ignored by default;
    // run explicitly, never during a wave. Proper fix: test AppState
    // with temp dirs (TODO_EFFORT H7).
    #[tokio::test]
    #[ignore]
    async fn test_web_routes_lifecycle() {
        // Token isolation: the bearer gate reads SDB_UI_TOKEN from the
        // environment, and the production token is set in this shell's
        // env (or CI's). A tokenless probe would 401 (20:31 incident
        // masked the real failure as a 500 hunt). Scoped override keeps
        // the gate covered without touching the real secret.
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        let state = create_test_state();
        state
            .log_buffer
            .write()
            .unwrap()
            .push("[12:00:00] Initial log line".to_string());

        let app = UiServer::build_app(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {TEST_UI_TOKEN}")).unwrap(),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{}", port);

        // 1. GET /
        let res = client.get(&base).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let html = res.text().await.unwrap();
        assert!(html.contains("SubDollarBench"));

        // 2. GET /api/status
        let res = client
            .get(format!("{}/api/status", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let status_json: StatusResponse = res.json().await.unwrap();
        assert!(!status_json.is_running);
        assert!(status_json.current_run.is_none());

        // 3. GET /api/env
        let res = client
            .get(format!("{}/api/env", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let env_json: EnvironmentInfo = res.json().await.unwrap();
        assert!(!env_json.os.is_empty());

        // 4. GET /api/prompt/redis
        let res = client
            .get(format!("{}/api/prompt/redis", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 5. POST /api/prompt/test_task: unknown tasks must NOT sprout
        // directories (404, no write) — the prompt editor covers known
        // tasks only.
        let res = client
            .post(format!("{}/api/prompt/test_task", base))
            .body("# Test Task")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
        // 5b. POST /api/prompt/redis: known tasks still save. Round-trip
        // the EXISTING bytes (never a probe string): the graded prompt is
        // the model's input and a write probe must not mutate it (20:31
        // incident: `# Test Task` overwrote tasks/redis/prompt.md).
        let orig = client
            .get(format!("{}/api/prompt/redis", base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!orig.is_empty() && orig != "# Test Task");
        let res = client
            .post(format!("{}/api/prompt/redis", base))
            .body(orig)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(
            res.text().await.unwrap().contains("RULES_VERSION"),
            "save_prompt must warn about the re-stamp"
        );

        // 5b. GET /api/sandbox/omp
        let res = client
            .get(format!("{}/api/sandbox/omp", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let omp_status: crate::sandbox::OmpVersionStatus = res.json().await.unwrap();
        assert!(omp_status.installed_version.is_some() || !omp_status.image_exists);

        // 5a. GET /api/prompt with traversal rejected (S3)
        let res = client
            .get(format!("{}/api/prompt/..%2Fetc", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // 5b. POST /api/prompt with traversal rejected (S3)
        let res = client
            .post(format!("{}/api/prompt/..%2Fbad", base))
            .body("evil")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // 5c. GET /api/models with Bearer header (S4)
        let res = client
            .get(format!("{}/api/models", base))
            .header("Authorization", "Bearer dummy-test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 6. GET /api/leaderboard
        let res = client
            .get(format!("{}/api/leaderboard", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 7. GET /api/console
        let res = client
            .get(format!("{}/api/console", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("Initial log line"));

        // 8. GET /api/runs
        let res = client
            .get(format!("{}/api/runs", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 9. GET /api/runs/nonexistent_xyz
        let res = client
            .get(format!("{}/api/runs/nonexistent_xyz", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // 10. GET /api/runs/nonexistent_xyz/log
        let res = client
            .get(format!("{}/api/runs/nonexistent_xyz/log", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // 11. GET /api/runs/nonexistent_xyz/files
        let res = client
            .get(format!("{}/api/runs/nonexistent_xyz/files", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // 12. GET /api/runs/nonexistent_xyz/file?file=abc.txt
        let res = client
            .get(format!(
                "{}/api/runs/nonexistent_xyz/file?file=abc.txt",
                base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // 15. POST /api/run conflict test
        for w in 0..worker_count() as u8 {
            state.running_slots.lock().unwrap().insert(w);
        }
        state.is_running.store(true, Ordering::SeqCst);
        let run_req = RunRequest {
            model: "test_model".to_string(),
            task: "redis".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 5,
            eval_only: true,
            effort: Some("low".to_string()),
            harness: None,
            timeout_min: Some(1),
            trials: 1,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
            stop_on_pass: false,
            protocol: None,
            feedback_rounds: None,
        };
        let res = client
            .post(format!("{}/api/run", base))
            .json(&run_req)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);

        // 16. POST /api/run/stop when running
        let res = client
            .post(format!("{}/api/run/stop", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(!state.is_running.load(Ordering::SeqCst));

        // 17. POST /api/run/stop when not running
        let res = client
            .post(format!("{}/api/run/stop", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // 18. POST /api/run eval_only mode with multi-trials
        let eval_req = RunRequest {
            model: "openrouter/meta/muse-spark-1.3-contributor".to_string(),
            task: "redis".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 1,
            eval_only: true,
            effort: Some("low".to_string()),
            harness: None,
            timeout_min: Some(1),
            trials: 2,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
            stop_on_pass: false,
            protocol: None,
            feedback_rounds: None,
        };
        let res = client
            .post(format!("{}/api/run", base))
            .json(&eval_req)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !state.is_running.load(Ordering::SeqCst) {
                break;
            }
        }
        let status_res = client
            .get(format!("{}/api/status", base))
            .send()
            .await
            .unwrap();
        assert_eq!(status_res.status(), 200);
        let status_body: StatusResponse = status_res.json().await.unwrap();
        assert_eq!(
            status_body.last_run.as_ref().and_then(|r| r.total_subruns),
            Some(2)
        );

        // 19. GET /api/models
        let res = client
            .get(format!("{}/api/models", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 20. GET /api/stream
        let res = client
            .get(format!("{}/api/stream", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 21. Tests with valid archived runs & comparison
        let runs_dir = RunArchiver::resolve_runs_dir();
        let run_a_id = "test_compare_run_a";
        let run_b_id = "test_compare_run_b";
        let dir_a = runs_dir.join(run_a_id);
        let dir_b = runs_dir.join(run_b_id);
        let ws_a = dir_a.join("workspace");
        let ws_b = dir_b.join("workspace");
        let _ = fs::create_dir_all(&ws_a);
        let _ = fs::create_dir_all(&ws_b);

        fs::write(ws_a.join("main.rs"), "fn main() { println!(\"a\"); }").unwrap();
        fs::write(ws_b.join("main.rs"), "fn main() { println!(\"b\"); }").unwrap();
        fs::write(dir_a.join("console.log"), "Console log for A").unwrap();
        fs::write(dir_b.join("console.log"), "Console log for B").unwrap();

        let manifest_a = RunManifest {
            rounds: Vec::new(),
            run_id: run_a_id.to_string(),
            model: "model_a".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Rust".to_string(),
            effort: Some("low".to_string()),
            effort_effective: None,
            harness: None,
            started_at: "2026-09-04T12:00:00Z".to_string(),
            completed_at: "2026-09-04T12:01:00Z".to_string(),
            duration_seconds: 60.0,
            turns: Some(3),
            pass_rate: 75.0,
            passed_stages: 3,
            total_stages: 4,
            stages: Vec::new(),
            robustness: Vec::new(),
            throughput_req_sec: Some(40000.0),
            reference_throughput_req_sec: Some(71000.0),
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            egress_enforced: None,
            tokens: RunTokenUsage {
                prompt_tokens: 1000,
                cached_tokens: 500,
                completion_tokens: 200,
                total_tokens: 1200,
            },
            cost_usd: 0.05,
            cost_source: None,
            cost_formula_usd: None,
            live_spend_delta_usd: None,
            effective_cost_usd: Some(0.055),
            savings_percent: 25.0,
            efficiency_score: 15.0,
            throughput_score: Some(31.9),
            files: Vec::new(),
            canary_echoed: None,
            cutoff_turn1: None,
            env: None,
            git_commit: None,
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            trial_pass_rates: Vec::new(),
            is_published: None,
            method_version: Some("0.1.0".to_string()),
            temperature: None,
            top_p: None,
            seed: None,
            prompt_sha: None,
            omp_version: None,
            harness_version: None,

            rules_version: None,
            sqlite_version: None,
            cap_price_basis: None,
            protocol: None,
            solved: None,
            solved_at: None,
            best_stage_score: None,
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        };
        let manifest_b = RunManifest {
            rounds: Vec::new(),
            run_id: run_b_id.to_string(),
            model: "model_b".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Rust".to_string(),
            effort: Some("high".to_string()),
            effort_effective: None,
            harness: None,
            started_at: "2026-09-04T12:05:00Z".to_string(),
            completed_at: "2026-09-04T12:06:00Z".to_string(),
            duration_seconds: 50.0,
            turns: Some(4),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            stages: Vec::new(),
            robustness: Vec::new(),
            throughput_req_sec: Some(55000.0),
            reference_throughput_req_sec: Some(71000.0),
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            egress_enforced: None,
            tokens: RunTokenUsage {
                prompt_tokens: 1500,
                cached_tokens: 600,
                completion_tokens: 300,
                total_tokens: 1800,
            },
            cost_usd: 0.08,
            cost_source: None,
            cost_formula_usd: None,
            live_spend_delta_usd: None,
            effective_cost_usd: Some(0.085),
            savings_percent: 20.0,
            efficiency_score: 12.5,
            throughput_score: Some(31.9),
            files: Vec::new(),
            canary_echoed: None,
            cutoff_turn1: None,
            env: None,
            git_commit: None,
            total_trials: None,
            pass_at_1: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            trial_pass_rates: Vec::new(),
            is_published: None,
            method_version: Some("0.1.0".to_string()),
            temperature: None,
            top_p: None,
            seed: None,
            prompt_sha: None,
            omp_version: None,
            harness_version: None,

            rules_version: None,
            sqlite_version: None,
            cap_price_basis: None,
            protocol: None,
            solved: None,
            solved_at: None,
            best_stage_score: None,
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        };

        fs::write(
            dir_a.join("manifest.json"),
            serde_json::to_string_pretty(&manifest_a).unwrap(),
        )
        .unwrap();
        fs::write(
            dir_b.join("manifest.json"),
            serde_json::to_string_pretty(&manifest_b).unwrap(),
        )
        .unwrap();

        // Check GET /api/compare 404
        let res = client
            .get(format!(
                "{}/api/compare?run_a=nonexistent&run_b=neither",
                base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // Check GET /api/compare valid runs
        let res = client
            .get(format!(
                "{}/api/compare?run_a={}&run_b={}",
                base, run_a_id, run_b_id
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let compare_res: RunComparisonResponse = res.json().await.unwrap();
        assert_eq!(compare_res.metric_diff.pass_rate_delta, 25.0);
        assert_eq!(compare_res.metric_diff.tokens_delta, 600);
        assert!(!compare_res.file_diffs.is_empty());
        assert_eq!(compare_res.file_diffs[0].status, "modified");

        // Check GET /api/runs/:id
        let res = client
            .get(format!("{}/api/runs/{}", base, run_a_id))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Check GET /api/runs/:id/log
        let res = client
            .get(format!("{}/api/runs/{}/log", base, run_a_id))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("Console log for A"));

        // Check GET /api/runs/:id/files
        let res = client
            .get(format!("{}/api/runs/{}/files", base, run_a_id))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Check GET /api/runs/:id/file?file=main.rs
        let res = client
            .get(format!("{}/api/runs/{}/file?file=main.rs", base, run_a_id))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("println!(\"a\")"));

        // Check GET /api/runs/:id/file missing param or missing file
        let res = client
            .get(format!("{}/api/runs/{}/file", base, run_a_id))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        let res = client
            .get(format!(
                "{}/api/runs/{}/file?file=missing.txt",
                base, run_a_id
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // Check GET /api/compare missing run_b
        let res = client
            .get(format!(
                "{}/api/compare?run_a={}&run_b=missing_b",
                base, run_a_id
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // Check GET /api/stream
        let res = client
            .get(format!("{}/api/stream", base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 22. POST /api/run with runnable start.sh to test Docker candidate branch
        let ws_dir = resolve_workspace_dir();
        let _ = fs::create_dir_all(&ws_dir);
        fs::write(ws_dir.join("start.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(ws_dir.join("start.sh"), fs::Permissions::from_mode(0o755));
        }

        let run_wal_req = RunRequest {
            model: "openrouter/meta/muse-spark-1.3-contributor".to_string(),
            task: "wal".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 1,
            eval_only: true,
            effort: Some("low".to_string()),
            harness: None,
            timeout_min: Some(1),
            trials: 1,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
            stop_on_pass: false,
            protocol: None,
            feedback_rounds: None,
        };
        let res = client
            .post(format!("{}/api/run", base))
            .json(&run_wal_req)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !state.is_running.load(Ordering::SeqCst) {
                break;
            }
        }

        // Cleanup test directories and generated test runs
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
        let _ = fs::remove_dir_all(ws_dir);
        let _ = fs::remove_dir_all("tasks/test_task");
        let _ = fs::remove_dir_all(crate::config::get_repo_root().join("tasks/test_task"));

        // Remove only the two archives this test created. The previous sweep
        // deleted any `runs/` entry starting with `test_` or containing
        // `gemini-2.5-flash`, which raced every other test running in parallel
        // and destroyed real run archives whenever a live run matched.
        for id in [run_a_id, run_b_id] {
            let _ = fs::remove_dir_all(runs_dir.join(id));
            let _ = fs::remove_file(resolve_results_dir().join(format!("{id}.json")));
        }
    }

    #[test]
    fn test_helpers_resolve() {
        let task_p = resolve_task_path("redis");
        assert!(task_p.to_string_lossy().contains("redis"));
        let res_dir = resolve_results_dir();
        assert!(res_dir.to_string_lossy().contains("results"));
        let ws_dir = resolve_workspace_dir();
        assert!(
            ws_dir.is_absolute(),
            "resolve_workspace_dir must be absolute"
        );
        assert_eq!(validate_task_name("redis").unwrap(), "redis");
        assert!(validate_task_name("../bad").is_err());
        assert!(validate_task_name("bad/slash").is_err());
    }

    #[test]
    fn test_openrouter_reasoning_metadata_parsing() {
        let sample_json = r#"{
            "data": [
                {
                    "id": "google/gemini-3.8-flash",
                    "name": "Google: Gemini 3.8 Flash",
                    "context_length": 1048576,
                    "pricing": { "prompt": "0.00000015", "completion": "0.00000060" },
                    "created": 1740000000,
                    "supported_parameters": ["reasoning", "reasoning_effort"],
                    "reasoning": {
                        "mandatory": true,
                        "default_enabled": true,
                        "supported_efforts": ["high", "medium", "low"],
                        "default_effort": "medium",
                        "supports_max_tokens": false
                    }
                },
                {
                    "id": "meta/muse-spark-1.3-contributor",
                    "name": "Meta: Muse Spark 1.3",
                    "context_length": 131072,
                    "pricing": { "prompt": "0.00000020", "completion": "0.00000040" },
                    "created": 1740000100,
                    "supported_parameters": ["reasoning", "reasoning_effort"],
                    "reasoning": {
                        "mandatory": true,
                        "supported_efforts": ["max", "high", "medium", "low"],
                        "default_effort": "medium"
                    }
                },
                {
                    "id": "qwen/qwen3.8-flash",
                    "name": "Qwen: Qwen 3.8 Flash",
                    "context_length": 131072,
                    "pricing": { "prompt": "0.00000010", "completion": "0.00000030" },
                    "created": 1740000200,
                    "supported_parameters": ["reasoning"],
                    "reasoning": {
                        "supports_max_tokens": true
                    }
                }
            ]
        }"#;

        let parsed: OpenRouterResponse =
            serde_json::from_str(sample_json).expect("deserialize sample");
        assert_eq!(parsed.data.len(), 3);

        let gemini = &parsed.data[0];
        assert_eq!(gemini.id, "google/gemini-3.8-flash");
        let r = gemini.reasoning.as_ref().unwrap();
        assert_eq!(
            r.supported_efforts.as_ref().unwrap(),
            &vec!["high", "medium", "low"]
        );
        assert_eq!(r.default_effort.as_deref(), Some("medium"));

        let meta = &parsed.data[1];
        let mut meta_efforts = meta
            .reasoning
            .as_ref()
            .unwrap()
            .supported_efforts
            .clone()
            .unwrap();
        if meta.id.starts_with("meta/") {
            meta_efforts.retain(|e| e != "max");
        }
        assert_eq!(meta_efforts, vec!["high", "medium", "low"]);

        let qwen = &parsed.data[2];
        assert_eq!(
            qwen.reasoning.as_ref().unwrap().supports_max_tokens,
            Some(true)
        );
    }

    /// The GUI console is inline JS inside index.html: a typo there breaks
    /// the whole page and nothing in the Rust compile path would notice.
    /// Pins the Board/Run/Live markers (gui-spec.md §4), forbids the retired
    /// per-task views, and runs node --check on the extracted script
    /// (skipped when node is absent).
    #[test]
    fn test_gui_console_has_board_run_live_and_parses() {
        let html = include_str!("index.html");
        for marker in [
            "data-tab=\"board\"",
            "data-tab=\"run\"",
            "data-tab=\"live\"",
            "data-tab=\"admin\"",
            "id=\"tab-board\"",
            "id=\"tab-run\"",
            "id=\"runBody\"",
            "id=\"liveStream\"",
            "function renderBoard(",
            "function renderRun(",
            "function renderLive(",
            "function ladderStrip(",
            "function applyHash(",
            "function loadRunDetail(",
            // The live log is fetched tail-first, then incrementally by
            // offset (round-10 follow-up: a finished cell's log is tens of
            // thousands of lines).
            "tail=400",
            "after=${liveOffset}",
        ] {
            assert!(html.contains(marker), "console must contain {marker}");
        }
        for gone in [
            "matrixTableBody",
            "renderMatrix",
            "renderModels",
            "renderTasks",
            "loadLeaderboard",
            "campaignsList",
            "renderRuns",
            "data-tab=\"matrix\"",
            "data-tab=\"leaderboard\"",
            "data-tab=\"traceability\"",
            "data-tab=\"console\"",
        ] {
            assert!(!html.contains(gone), "retired view must be gone: {gone}");
        }
        // Extract the single inline <script> block (shared.js loads
        // separately via /static/shared.js) and parse-check it.
        let mut scripts = Vec::new();
        let mut rest = html;
        while let Some(s) = rest.find("<script>") {
            let body = &rest[s + "<script>".len()..];
            if let Some(e) = body.find("</script>") {
                scripts.push(&body[..e]);
                rest = &body[e + "</script>".len()..];
            } else {
                break;
            }
        }
        assert_eq!(scripts.len(), 1, "index.html must hold one inline script");
        let dir = std::env::temp_dir().join(format!("gui_js_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("gui.js");
        std::fs::write(&f, scripts[0]).unwrap();
        let out = std::process::Command::new("node")
            .arg("--check")
            .arg(&f)
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        match out {
            Ok(o) => assert!(
                o.status.success(),
                "node --check rejected the GUI JS: {}",
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(_) => eprintln!("node not found; skipping GUI JS parse check"),
        }
    }

    /// The DOM stubs the console boot smoke needs: every element is a
    /// benign object, fetch answers the read endpoints, timers are inert.
    const GUI_SMOKE_HARNESS: &str = r#"
const fs = require('fs');
const path = require('path');
const dir = __dirname;
// Elements are keyed by id so the render checks can read back what the
// console put in them (round-11: this smoke caught a TDZ reference in the
// Live card and a wall bar that fell back to "now" for a dead cell).
const els = {};
function makeEl(id) {
  return {
    id: id || '', style: {}, dataset: {}, children: [], value: '', textContent: '', innerHTML: '',
    className: '', checked: false, disabled: false,
    classList: { add(){}, remove(){}, toggle(){}, contains(){ return false; } },
    appendChild(){}, removeChild(){}, remove(){}, addEventListener(){}, removeEventListener(){},
    querySelector(){ return makeEl('q'); }, querySelectorAll(){ return []; },
    setAttribute(){}, getAttribute(){ return null; }, focus(){}, click(){}, insertBefore(){},
    scrollTo(){}, closest(){ return null; }, contains(){ return false; },
    getBoundingClientRect(){ return { top:0,left:0,width:0,height:0,bottom:0,right:0 }; },
    open(){}, close(){}, reset(){}, submit(){}, removeAttribute(){},
  };
}
function el(id) { if (!els[id]) els[id] = makeEl(id); return els[id]; }
const store = {};
global.localStorage = { getItem: k => (k in store ? store[k] : null), setItem: (k,v)=>{store[k]=String(v);}, removeItem: k=>{delete store[k];} };
global.document = {
  visibilityState: 'visible',
  getElementById: el, querySelector(){ return makeEl('q'); }, querySelectorAll(){ return []; },
  createElement(){ return makeEl('c'); }, addEventListener(){}, body: makeEl('body'), documentElement: makeEl('html'), cookie: '',
};
global.addEventListener = () => {};
global.removeEventListener = () => {};
global.window = global;
global.location = { hash: '#board', href: 'http://x/', pathname: '/', search: '' };
global.navigator = { userAgent: 'node' };
global.EventSource = class { constructor(){} close(){} addEventListener(){} };
global.setInterval = () => 0;
global.clearInterval = () => {};
global.alert = () => {};
global.confirm = () => true;
global.scrollTo = () => {};
global.fetch = async (url) => {
  const u = String(url || '');
  let body = { is_running: false, workers: {} };
  if (u.includes('/api/campaigns') && !u.includes('/log')) body = [];
  else if (u.includes('/api/queue') || u.includes('/api/runs') || u.includes('/api/models')) body = [];
  return { ok: true, status: 200, headers: { get: () => null }, json: async () => body, text: async () => '' };
};
process.on('unhandledRejection', e => { console.log('UNHANDLED:', e && e.message ? e.message : e); process.exit(1); });
try {
  eval(fs.readFileSync(path.join(dir, 'shared.js'), 'utf8'));
  eval(fs.readFileSync(path.join(dir, 'app.js'), 'utf8'));
} catch (e) { console.log('EVAL FAILED:', e.message); process.exit(1); }
const calls = [
  ['checkActiveRun'], ['refreshQueue'], ['refreshBoard'], ['loadRulesVersion'], ['applyHash'],
  ['checkBreaker'], ['fetchOmpStatus'], ['fetchOpencodeStatus'], ['fetchIntegrity'],
  ['updateTelemetryDisplay'], ['tickWorkerElapsed'], ['renderRunningSetup', null],
  ['renderWorkerCards', {}], ['inspectRun', 'ut_run'], ['loadPrompt', 'sql'],
];
for (const [name, ...args] of calls) {
  let fn;
  try { fn = eval(name); } catch (e) { console.log('UNDEFINED:', name); process.exit(1); }
  if (typeof fn !== 'function') { console.log('NOT A FUNCTION:', name); process.exit(1); }
  try {
    const r = fn(...args);
    if (r && typeof r.then === 'function') {
      r.catch(e => { console.log('ASYNC FAILED', name, e.message); process.exit(1); });
    }
  } catch (e) { console.log('CALL FAILED', name, e.message); process.exit(1); }
}
// The running branch of the status poller too (telemetry, worker tabs,
// elapsed timers): a wave must not surface a different set of references.
global.fetch = async (url) => ({
  ok: true, status: 200, headers: { get: () => null },
  json: async () => (String(url || '').includes('/api/status') ? {
    is_running: true,
    workers: { 'q1_w0': { run_id: 'r1', model: 'ut/m', task: 'sql', effort: 'high', started_at: '2026-10-17T09:00:00Z', turns: 3, cost_usd: 0.01, tokens: 100, subrun: 1, total_subruns: 1 } },
    current_run: { run_id: 'r1', model: 'ut/m', task: 'sql', effort: 'high', started_at: '2026-10-17T09:00:00Z', turns: 3, max_turns: 10, cost_usd: 0.01, tokens: 100, subrun: 1, total_subruns: 1 },
  } : (String(url || '').includes('/api/campaigns') && !String(url || '').includes('/log') ? [] : [])),
  text: async () => '',
});
try {
  const r = checkActiveRun();
  if (r && typeof r.then === 'function') r.catch(e => { console.log('RUNNING BRANCH FAILED:', e.message); process.exit(1); });
} catch (e) { console.log('RUNNING BRANCH CALL FAILED:', e.message); process.exit(1); }

// ---------------------------------------------------------------------------
// Fixture-driven render smoke: a *stalled, legacy* cell (no live block,
// finished_at == started_at, no records for the ticket it died in). The
// Board, Run and Live views must render it without throwing and with the
// round-11 fixes: dated stalls, an honest wall bar, `interrupted`, and the
// Cause clamp inside a td.
// ---------------------------------------------------------------------------
const cell = {
  id: 'campaign_fixture', state: 'stalled', stop_reason: '', current_stage: 8,
  updated_at: '2026-10-17T16:16:02Z', log_mtime: '2026-10-17T16:29:33Z',
  live: {},
  rollup: {
    model: 'openrouter/meta/muse-spark-1.3-contributor', harness: 'omp', effort: 'high',
    rules_version: '2026-10-17.16', total_cost_usd: 0.5455, budget_usd: 0.75, wall_min: 120,
    started_at: '2026-10-17T14:31:53Z', finished_at: '2026-10-17T14:31:53Z',
    points: 4.8, max_points: 26, reached: 5, regression_free_tickets: 4,
    over_budget: false, cap_reached: false,
    stages: [{
      id: 7, variant: 'full', status: 'failed', effective_cost_usd: 0.11, turns: 30,
      rounds: [{ round: 0, passed_stages: 7, total_stages: 8, turns: 20, cost_usd: 0.04 }],
      failure: { kind: 'regression', line: 'broke ticket 4', detail: 'long detail', stderr_tail: 'tail' },
    }],
  },
  ladder: [
    { id: 0, title: 'Genesis', status: 'pass', points: 1 },
    { id: 7, title: 'Aggregates', status: 'fail', points: 0, cause: 'broke ticket 4' },
    { id: 8, title: 'Joins', status: 'not_reached', points: 0, cause: 'not reached' },
  ],
};
const detail = Object.assign({}, cell, { budget_usd: 0.75, wall_min: 120, live_run: null });
global.fetch = async (url) => {
  const u = String(url || '');
  let body = {};
  if (u.includes('/log')) body = { offset: 0, lines: [] };
  else if (/\/api\/campaigns\/[^/?]+$/.test(u)) body = detail;
  else if (u.includes('/api/campaigns')) body = [cell];
  else if (u.includes('/api/status')) body = { is_running: false };
  else if (u.includes('/api/queue')) body = [];
  return { ok: true, status: 200, headers: { get: () => null }, json: async () => body, text: async () => '' };
};
(async () => {
  try {
    await refreshBoard();
    await loadRunDetail('campaign_fixture');
    renderLive();
    renderRun();
    renderBoard();
  } catch (e) {
    console.log('RENDER THREW:', e.stack.split('\n').slice(0, 3).join(' | '));
    process.exit(1);
  }
  const run = el('runBody').innerHTML;
  const live = el('liveBody').innerHTML;
  const board = el('boardBody').innerHTML;
  const fail = [];
  if (typeof rulesAtLeast !== 'function') fail.push('rules-at-least-missing');
  else if (!rulesAtLeast('2026-10-19.100', '2026-10-19.39')
           || rulesAtLeast('2026-10-18.50', '2026-10-19.39')) {
    fail.push('rules-version-order');
  }
  // R23-2: the Board launcher defaults the wall to 480 and its resolved
  // command prints the 120-minute per-ticket guard.
  const indexPage = fs.readFileSync(path.join(dir, 'index.html'), 'utf8');
  if (!indexPage.includes('id="campaignWall" value="480"')) fail.push('campaign-wall-default');
  if (typeof campaignCommand !== 'function') fail.push('campaign-command-missing');
  else {
    const c = campaignCommand('m', 'high', 0.99, 480, null, null, '');
    if (!c.includes('--wall-min 480') || !c.includes('--timeout-min 120')) fail.push('campaign-command:' + c);
  }
  if (!run.includes('interrupted')) fail.push('run-interrupted');
  if (!run.includes('class="causeCell"')) fail.push('cause-inner-div');
  if (/td class="hint causeCell"/.test(run)) fail.push('cause-td-clamped');
  if (!live.includes('17 Oct')) fail.push('live-dated-stall');
  if (/wall 0s|wall 19h|wall 2[0-9]h/.test(live)) fail.push('live-wall-now');
  if (!board.includes('17 Oct')) fail.push('board-dated-stall');
  // Round-18 Board: a running cell that only has the server's stub rollup
  // is listed, above the older-rules divider; one model under two rule
  // sets is two rows, never one group with a mixed median.
  window.__SDB_RULES__ = '2026-10-19.31';
  const boardFixture = [
    { id: 'campaign_live_cell', state: 'running', current_stage: 0, stop_reason: '',
      live: { ticket: 0, turn: 3, max_turns: 28 },
      rollup: { model: 'openrouter/ut/live', harness: 'omp', effort: 'max', rules_version: '2026-10-19.31',
                points: 0, max_points: 76, total_cost_usd: 0, stages: [] },
      ladder: [{ id: 0, title: 'Genesis', status: 'running', points: 0 }] },
    { id: 'campaign_new_cell', state: 'done', stop_reason: 'budget cap', live: {},
      rollup: { model: 'openrouter/ut/m', harness: 'omp', effort: 'high', rules_version: '2026-10-19.29',
                points: 34.6, max_points: 76, total_cost_usd: 0.75, stages: [] },
      ladder: [{ id: 0, title: 'Genesis', status: 'pass', points: 1 }] },
    { id: 'campaign_old_cell', state: 'done', stop_reason: 'ladder end', live: {},
      rollup: { model: 'openrouter/ut/m', harness: 'omp', effort: 'high', rules_version: '2026-10-18.17',
                points: 13, max_points: 26, total_cost_usd: 0.36, stages: [] },
      ladder: [{ id: 0, title: 'Genesis', status: 'pass', points: 1 }] },
  ];
  global.fetch = async (url) => {
    const u = String(url || '');
    const body = (u.includes('/api/campaigns') && !u.includes('/log')) ? boardFixture : {};
    return { ok: true, status: 200, headers: { get: () => null }, json: async () => body, text: async () => '' };
  };
  try { await refreshBoard(); renderBoard(); } catch (e) { console.log('BOARD THREW:', e.message); process.exit(1); }
  const b2 = el('boardBody').innerHTML;
  const iLive = b2.indexOf('campaign_live_cell');
  const iDiv = b2.indexOf('older rule sets');
  if (iLive < 0) fail.push('board-live-cell-missing');
  if (iLive >= 0 && iDiv >= 0 && iDiv < iLive) fail.push('board-live-cell-under-divider');
  if (!b2.includes('campaign_old_cell')) fail.push('board-rules-folded-into-one-group');
  if (b2.includes('\u00d72 runs') || b2.includes('×2 runs')) fail.push('board-grouped-across-rules');
  if (!b2.includes('0.0/76 pts')) fail.push('board-live-cell-max-points');

  // R2: career leaf fixture: phase 1, a faulted phase 2, a phase-2 refork
  // retry, and a running phase 3. The faulted cell is not in the career; the
  // leaf is the newest finished child (the retry), not the older child, and
  // the running cell hangs under the same root.
  const cbase = {model:'m',harness:'omp',effort:'high',rules_version:'2026-10-19.41',max_points:76};
  const p1 = {id:'campaign_career_p1', state:'done', ladder:[], rollup:Object.assign({}, cbase, {
    phase:1, started_at:'2026-10-21T00:00:00Z', career:{cells:['campaign_career_p1'],points:10,total_cost_usd:0.5}})};
  const old = {id:'campaign_career_old', state:'done', ladder:[], rollup:Object.assign({}, cbase, {
    phase:2, chain_from:'campaign_career_p1', started_at:'2026-10-21T00:10:00Z',
    career:{cells:['campaign_career_p1','campaign_career_old'],points:15,total_cost_usd:0.7}})};
  const fault = {id:'campaign_career_fault', state:'done', ladder:[], rollup:Object.assign({}, cbase, {
    phase:2, chain_from:'campaign_career_p1', started_at:'2026-10-21T00:20:00Z', harness_fault:'provider billing'})};
  const retry = {id:'campaign_career_retry', state:'done', ladder:[], rollup:Object.assign({}, cbase, {
    phase:2, chain_from:'campaign_career_p1', started_at:'2026-10-21T00:30:00Z',
    refork:{of:'campaign_career_old',reason:'cancelled retry'},
    career:{cells:['campaign_career_p1','campaign_career_retry'],points:18,total_cost_usd:0.8}})};
  const running = {id:'campaign_career_running', state:'running', ladder:[], rollup:Object.assign({}, cbase, {
    phase:3, chain_from:'campaign_career_retry', points:0, total_cost_usd:0.0})};
  const fullFault = {id:'campaign_full_fault', state:'done', ladder:[], rollup:Object.assign({}, cbase, {
    started_at:'2026-10-21T00:40:00Z', finished_at:'2026-10-21T00:50:00Z',
    harness_fault:'provider billing', points:0, total_cost_usd:0.1})};
  const cgroups = groupCells([p1, old, fault, retry, running, fullFault]);
  const cg = cgroups.find(g => g.career && g.cells.some(c => c.id === 'campaign_career_retry'));
  if (!cg) fail.push('career-group-missing');
  // S1: a faulted phase cell stays in the career (rendered as an indented
  // non-result), while the leaf and points still ignore it.
  if (cg && !cg.cells.some(c => c.id === 'campaign_career_fault')) fail.push('faulted-phase-missing');
  if (cg && !cg.cells.some(c => c.id === 'campaign_career_running')) fail.push('running-not-under-root');
  const cleaf = cg && careerLeaf(cg.cells);
  if (!cleaf || cleaf.id !== 'campaign_career_retry') fail.push('career-leaf:' + (cleaf && cleaf.id));
  if (cg && careerPoints(cg.cells) !== 18) fail.push('career-points:' + careerPoints(cg.cells));
  // S1: a faulted full-ladder cell stays a normal (non-career) row.
  const fg = cgroups.find(g => !g.career && g.cells.some(c => c.id === 'campaign_full_fault'));
  if (!fg) fail.push('faulted-full-row-missing');

  // S3: the phase view. A phase-1 cell draws four labelled groups with its
  // own score on P1; a career row merges the chain and summarises a
  // running phase 2 and the unrun phases 3 and 4.
  const phases = [{id:1,title:'Core',stages:[0,1,2,3,4,5,6],max_points:10},
    {id:2,title:'Relational',stages:[7,8,9,10],max_points:12},
    {id:3,title:'Engine',stages:[11,12,13,14,15],max_points:22},
    {id:4,title:'Hard',stages:[16,17,18,19,20],max_points:32}];
  const lad = st => Array.from({length: 21}, (_, i) => ({id: i, title: 't' + i, status: st(i), variant: 'full'}));
  const ph1 = {id:'campaign_ph_p1', state:'done', phases, ladder: lad(i => i <= 6 ? 'pass' : 'later_phase'),
    rollup: Object.assign({}, cbase, {phase:1, points:8.8, max_points:10, total_cost_usd:0.49,
      started_at:'2026-10-21T01:00:00Z', career:{cells:['campaign_ph_p1'], points:8.8, total_cost_usd:0.49}})};
  const ph2 = {id:'campaign_ph_p2', state:'running', phases,
    ladder: lad(i => i <= 6 ? 'carried' : i === 7 ? 'pass' : i === 8 ? 'running' : i <= 10 ? 'not_reached' : 'later_phase'),
    rollup: Object.assign({}, cbase, {phase:2, chain_from:'campaign_ph_p1', points:3, max_points:12, total_cost_usd:0.1})};
  const s1 = ladderStrip(ph1.ladder, ph1);
  if ((s1.match(/class="ladderPhase( own)?( later)?"/g) || []).length !== 4) fail.push('phase-groups');
  if (!s1.includes('8.8/10 ')) fail.push('phase-own-summary');
  if (!s1.includes('ladderPhase later')) fail.push('phase-later-dimmed');
  const cv = careerView(ph1, [ph1, ph2]);
  if (cv.ladder[3].status !== 'pass') fail.push('career-p1-from-owner');
  if (cv.ladder[8].status !== 'running') fail.push('career-p2-from-running');
  if (!String(cv.summaries[2]).startsWith('running')) fail.push('career-running-summary:' + cv.summaries[2]);
  if (!cv.running || cv.running.id !== 'campaign_ph_p2') fail.push('career-running-cell');
  if (cv.summaries[3] !== '\u2014/22') fail.push('career-unrun-summary:' + cv.summaries[3]);
  // F3: a waiting ladder ticket renders with the waiting legend and cause.
  if (!ladderLegend().includes('waiting')) fail.push('waiting-legend');
  const ws = ladderSquare({id:14, title:'Waiting', status:'waiting', variant:'', points:0, cause:'needs 11'}, []);
  if (!ws.includes('waiting') || !ws.includes('needs 11')) fail.push('waiting-square:' + ws);
  // R23 D5: the Board square shows why the agent stopped.
  const ss = ladderSquare({id:10, title:'Sold', status:'fail', variant:'full', points:0, stop_reason:'dollars', cause:'own stage'}, []);
  if (!ss.includes('stopped: dollars')) fail.push('stop-reason-square:' + ss);
  const pg = groupCells([ph1, ph2]).find(g => g.career && g.cells.some(c => c.id === 'campaign_ph_p2'));
  if (!pg || !pg.cells.some(c => c.id === 'campaign_ph_p1')) fail.push('running-phase-2-not-in-career');
  if (fail.length) { console.log('RENDER CHECKS FAILED:', fail.join(',')); process.exit(1); }
  console.log('smoke OK');
})();
"#;

    // ---------------------------------------------------------------------
    // Coverage additions: idle/synthetic-state route tests.
    //
    // Every test here binds 127.0.0.1:0 (never 3000/3001: the live engine
    // owns those) and never mutates the repo's bench.db / runs / results /
    // tasks / portal. Run/queue fixtures live in the per-test tempfile
    // store; mutating routes are only driven down validation, rejection, or
    // engine-proxy paths, never through a real dispatch.
    // ---------------------------------------------------------------------

    /// Bind ephemeral + serve `state`'s full router; returns the base URL.
    async fn serve_test_app(state: AppState) -> String {
        let app = UiServer::build_app(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    /// Client that always presents the shared probe token, so mutating
    /// routes pass the guard whether or not SDB_UI_TOKEN is configured.
    fn authed_client() -> reqwest::Client {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {TEST_UI_TOKEN}")).unwrap(),
        );
        reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap()
    }

    async fn response_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// Idle-state worker fixture for status/console route tests.
    fn active_run_info(run_id: &str) -> ActiveRunInfo {
        ActiveRunInfo {
            run_id: run_id.to_string(),
            model: "sdb-ut-model".to_string(),
            task: "redis".to_string(),
            effort: "high".to_string(),
            started_at: "2026-01-02T00:00:00Z".to_string(),
            turns: 1,
            max_turns: 100,
            cost_usd: 0.01,
            tokens: 42,
            duration_secs: None,
            subrun: Some(1),
            total_subruns: Some(1),
            budget_usd: 0.1,
            timeout_min: 1,
            trials: 1,
            eval_only: false,
            extend_if_unstable: false,
            max_trials: 9,
            review_model: None,
        }
    }

    /// Minimal valid manifest for DB-fixture route tests.
    fn test_manifest(id: &str) -> RunManifest {
        serde_json::from_value(serde_json::json!({
            "run_id": id,
            "model": "sdb-ut-model",
            "task": "redis",
            "status": "completed",
            "language": "Rust",
            "started_at": "2026-01-02T00:00:00Z",
            "completed_at": "2026-01-02T00:01:00Z",
            "duration_seconds": 60.0,
            "pass_rate": 100.0,
            "passed_stages": 4,
            "total_stages": 4,
            "stages": [],
            "throughput_req_sec": 1234.0,
            "tokens": {
                "prompt_tokens": 10,
                "cached_tokens": 0,
                "completion_tokens": 20,
                "total_tokens": 30
            },
            "cost_usd": 0.01,
            "savings_percent": 0.0,
            "efficiency_score": 1.0,
            "files": []
        }))
        .expect("test manifest must deserialize")
    }

    #[test]
    fn parse_openrouter_models_filters_prices_and_reasoning() {
        let payload: OpenRouterResponse = serde_json::from_value(serde_json::json!({
            "data": [
                {
                    "id": "free/model", "name": "Free", "context_length": 1000,
                    "created": 1, "pricing": {"prompt": "0", "completion": "0"}
                },
                {
                    "id": "cheap/model", "name": "Cheap",
                    "pricing": {"prompt": "0.0000001", "completion": "0.0000002"},
                    "supported_parameters": ["reasoning"]
                },
                {
                    "id": "maxed/model", "name": "Maxed",
                    "pricing": {"prompt": "0.00000005", "completion": "0.00000005"},
                    "reasoning": {"supports_max_tokens": true}
                },
                {
                    "id": "meta/max-model", "name": "Meta",
                    "pricing": {"prompt": "0.0000001", "completion": "0.0000001"},
                    "supported_parameters": ["reasoning_effort"],
                    "reasoning": {
                        "mandatory": true,
                        "supported_efforts": ["max", "high"],
                        "default_effort": "high"
                    }
                },
                {
                    "id": "expensive/model", "name": "Expensive",
                    "pricing": {"prompt": "0.001", "completion": "0.001"}
                },
                {"id": "unpriced/model", "name": "No pricing at all"},
                {
                    "id": "batch/model:batch", "name": "Batch only",
                    "pricing": {"prompt": "0", "completion": "0"}
                },
                {
                    "id": "default-on/model", "name": "Default-on",
                    "pricing": {"prompt": "0.0000001", "completion": "0.0000001"},
                    "reasoning": {"default_enabled": true}
                }
            ]
        }))
        .unwrap();

        let models = parse_openrouter_models(payload);

        // Batch variants skipped, expensive/unpriced hidden, cheapest first.
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "free/model",
                "maxed/model",
                "meta/max-model",
                "default-on/model",
                "cheap/model"
            ]
        );
        let free = &models[0];
        assert_eq!(free.prompt_price_per_m, 0.0);
        assert_eq!(free.context_length, 1000);
        assert!(!free.is_reasoning);
        assert!(free.supported_efforts.is_empty());
        // meta/ "max" is clamped; mandatory reasoning is surfaced.
        let meta = models.iter().find(|m| m.id == "meta/max-model").unwrap();
        assert_eq!(meta.supported_efforts, vec!["high"]);
        assert_eq!(meta.default_effort.as_deref(), Some("high"));
        assert!(meta.reasoning_mandatory);
        assert!(meta.is_reasoning);
        // supports_max_tokens alone marks reasoning.
        let maxed = models.iter().find(|m| m.id == "maxed/model").unwrap();
        assert!(maxed.supports_max_tokens);
        assert!(maxed.is_reasoning);
        // default_enabled alone marks reasoning too.
        let default_on = models.iter().find(|m| m.id == "default-on/model").unwrap();
        assert!(default_on.is_reasoning);
        // A reasoning param is enough without a reasoning object.
        let cheap = models.iter().find(|m| m.id == "cheap/model").unwrap();
        assert!(cheap.is_reasoning);
    }

    #[tokio::test]
    async fn read_routes_idle_state_smoke() {
        let state = create_test_state();
        // DB fixture keeps /api/runs on the store path (no real-dir scan).
        state
            .store
            .upsert_run_manifest(&test_manifest("sdb_ut_read_fixture"))
            .unwrap();
        let base = serve_test_app(state.clone()).await;
        let client = authed_client();

        // Static shell + shared leaf JS.
        let res = client.get(&base).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("SubDollarBench"));
        let res = client
            .get(format!("{base}/static/shared.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("javascript"));

        // Status/int env/integrity on idle state.
        let res = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let status: StatusResponse = res.json().await.unwrap();
        assert!(!status.is_running);
        assert!(!status.proxy);
        assert!(!status.db_fallback);

        let res = client.get(format!("{base}/api/env")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let env: EnvironmentInfo = res.json().await.unwrap();
        assert!(!env.os.is_empty());

        let res = client
            .get(format!("{base}/api/integrity"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let integrity: serde_json::Value = res.json().await.unwrap();
        assert_eq!(integrity["ok"], serde_json::Value::Bool(true));
        assert_eq!(integrity["orphans"], 0);

        // Queue is empty and redacted; console empty with a total header.
        let res = client
            .get(format!("{base}/api/queue"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let queue: Vec<QueuedRun> = res.json().await.unwrap();
        assert!(queue.is_empty());

        let res = client
            .get(format!("{base}/api/console"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers()
                .get("X-Console-Total")
                .unwrap()
                .to_str()
                .unwrap(),
            "0"
        );

        // DB-backed run listing + detail.
        let res = client.get(format!("{base}/api/runs")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let runs: Vec<serde_json::Value> = res.json().await.unwrap();
        let fixture = runs
            .iter()
            .find(|r| r["run_id"] == "sdb_ut_read_fixture")
            .expect("store fixture must be listed");
        assert_eq!(fixture["portal_hidden"], serde_json::Value::Bool(false));
        assert_eq!(fixture["poisoned"], serde_json::Value::Bool(false));
        let res = client
            .get(format!("{base}/api/runs/sdb_ut_read_fixture"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let detail: serde_json::Value = res.json().await.unwrap();
        assert_eq!(detail["run_id"], "sdb_ut_read_fixture");

        // Unknown-run guards across the run subroutes.
        for path in [
            "/api/runs/sdb_ut_missing",
            "/api/runs/sdb_ut_missing/log",
            "/api/runs/sdb_ut_missing/review",
            "/api/runs/sdb_ut_missing/files",
            "/api/runs/sdb_ut_missing/file?file=main.rs",
        ] {
            let res = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(res.status(), 404, "{path} must 404 on a missing run");
        }
        for path in ["/api/runs/..%2Fetc/files", "/api/runs/..%2Fetc/review"] {
            let res = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(res.status(), 400, "{path} must reject traversal");
        }

        // Prompt read + guard (read-only: never a write probe).
        let res = client
            .get(format!("{base}/api/prompt/redis"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(!res.text().await.unwrap().is_empty());
        let res = client
            .get(format!("{base}/api/prompt/..%2Fetc"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // Compare + campaign guards.
        let res = client
            .get(format!("{base}/api/compare?run_a=..%2Fx&run_b=y"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        let res = client
            .get(format!("{base}/api/compare?run_a=x&run_b=..%2Fy"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        let res = client
            .get(format!("{base}/api/compare?run_a=none_a&run_b=none_b"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
        let res = client
            .get(format!("{base}/api/campaigns"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let res = client
            .get(format!("{base}/api/campaigns/..%2Fetc/log"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        let res = client
            .get(format!("{base}/api/campaigns/sdb_ut_missing_campaign/log"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);

        // Sandbox harness status (docker-image probe only; no containers).
        let res = client
            .get(format!("{base}/api/sandbox/opencode"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        let res = client.get(format!("{base}/nope")).send().await.unwrap();
        assert_eq!(res.status(), 404);
    }

    /// The console is inline JS: a dangling reference left behind by a
    /// cleanup (round-7: `liveTelemetry`, `tickWorkerElapsed`) only shows up
    /// in a browser, where it repeats on every poll. This evaluates
    /// shared.js + the inline script under a DOM stub, runs the boot path
    /// and the 4 s pollers, and fails on any ReferenceError. Skipped when
    /// node is absent.
    #[test]
    fn test_gui_console_boots_under_a_dom_stub() {
        let html = include_str!("index.html");
        let mut scripts = Vec::new();
        let mut rest = html;
        while let Some(s) = rest.find("<script>") {
            let body = &rest[s + "<script>".len()..];
            if let Some(e) = body.find("</script>") {
                scripts.push(&body[..e]);
                rest = &body[e + "</script>".len()..];
            } else {
                break;
            }
        }
        assert_eq!(scripts.len(), 1, "index.html must hold one inline script");
        let dir = std::env::temp_dir().join(format!("gui_smoke_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shared.js"), include_str!("shared.js")).unwrap();
        std::fs::write(dir.join("app.js"), scripts[0]).unwrap();
        std::fs::write(dir.join("index.html"), html).unwrap();
        std::fs::write(dir.join("harness.js"), GUI_SMOKE_HARNESS).unwrap();
        let out = std::process::Command::new("node")
            .arg(dir.join("harness.js"))
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        match out {
            Ok(o) => assert!(
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains("smoke OK"),
                "console boot smoke failed:\n{}\n{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(_) => eprintln!("node not found; skipping GUI boot smoke"),
        }
    }

    /// gui-spec.md §4.3: every log line falls into one of the Live view's
    /// chips; representative cell-8 lines pin the mapping.
    #[test]
    fn test_campaign_log_kinds() {
        use super::log_kind;
        let cases = [
            // Verbatim from cell 8's and the muse cell's live.log, with the
            // real `[HH:MM:SS] ` timestamps (round-8 finding 6).
            ("[11:07:20]   Stage 8: Joins -> FAIL ErrorClass: expected", "verdict"),
            ("[11:17:46] [FEEDBACK] Round 1 test result: 2 of 6 stages passed (33.3%)", "verdict"),
            ("[09:40:12] [THOUGHT] Let me start by understanding", "agent"),
            ("[09:40:14] [LIVE] Turn 6/9 | Spent: $0.0035", "telemetry"),
            ("[09:40:14] [TELEMETRY] turn=6 max_turns=15 spent=0.0035 tokens=1000", "telemetry"),
            ("[09:40:15] [TOKENS] Turn usage - input: 3852, output: 66, cached: 28416", "telemetry"),
            ("[09:40:15] [OMP] [BENCH-POLICY] turn=6/9 idle=false", "telemetry"),
            ("[09:40:16] [WAITING] Turn 7/9 in progress: Model is reasoning... (elapsed: 3s)", "telemetry"),
            ("[09:40:15] [ACTION] read", "tools"),
            ("[09:40:16]   > [server.py#2381]", "tools"),
            ("[09:40:17] [RESULT] read (OK)", "tools"),
            ("[09:40:18] [SETUP] Waiting for server port 5480", "setup"),
            ("[09:40:19]   | # Automated Verification Failure Report", "setup"),
            ("[09:40:20] [VERIFY] Checking anti-cheat compliance", "verifier"),
            ("[09:40:21]     159 statements graded (seed 11663210981208198265)", "verifier"),
            ("[14:31:53] ─── ticket 0 [genesis] Genesis: tables, insert, select (acceptance [0], budget $0.20) ───", "verdict"),
            // The pre-fix fixtures (no timestamps) still classify.
            ("  Stage 8: Joins -> FAIL ErrorClass: expected", "verdict"),
            ("[UPGRADE] sentinel from stage 4 verified", "verdict"),
            ("[PERF] point p50 0.2 ms scan vs 0.1 ms index", "verdict"),
            (
                "[FEEDBACK] Round 1 test result: 0 of 6 stages passed (0.0%)",
                "verdict",
            ),
            (
                "  s8  [relational] Joins  FAIL 2/6 ($0.1425, 30 turns)",
                "verdict",
            ),
            (
                "─── ticket 8 [relational] Joins (acceptance [0, 1, 3]) ───",
                "verdict",
            ),
            ("[THOUGHT] Let me start by understanding", "agent"),
            ("[LIVE] Turn 6/9 | Spent: $0.0035", "telemetry"),
            ("[ACTION] read", "tools"),
            ("  path: /workspace/server.py:966-1265", "verifier"),
            ("[RESULT] read (OK)", "tools"),
            ("[SETUP] Waiting for server port 5480", "setup"),
            ("[WARN] Port 5480 not responding within 25s", "setup"),
            ("[VERIFY] Checking anti-cheat compliance", "verifier"),
            (
                "    159 statements graded (seed 11663210981208198265)",
                "verifier",
            ),
            (
                "[DIAGNOSTICS] Candidate container runtime output:",
                "verifier",
            ),
        ];
        for (line, want) in cases {
            assert_eq!(log_kind(line), want, "line: {line:?}");
        }
        // A real live.log must yield both default chips (round-8 finding 6:
        // the default Live view was blank because every line classified as
        // verifier). This is the suite's one *intentional* read of the real
        // runs/ directory (read-only, never locking); everything else is
        // isolated via SDB_RUNS_DIR (round-9 finding 4). Skips when no
        // campaign log is on disk.
        let runs = crate::config::get_repo_root().join("runs");
        let real = std::fs::read_dir(&runs).ok().and_then(|rd| {
            rd.flatten().find_map(|e| {
                let p = e.path().join("live.log");
                std::fs::read_to_string(&p)
                    .ok()
                    .filter(|t| t.lines().count() > 50)
            })
        });
        match real {
            Some(text) => {
                let kinds: std::collections::HashSet<&str> = text.lines().map(log_kind).collect();
                assert!(
                    kinds.contains("verdict"),
                    "a real live.log must have verdict lines"
                );
                assert!(
                    kinds.contains("agent"),
                    "a real live.log must have agent lines"
                );
            }
            None => eprintln!("no live.log on disk; skipping the real-log kind check"),
        }
    }

    /// gui-spec.md §5.4: the detail endpoint carries the full ladder
    /// (attempted, skipped and not-reached tickets) and backfills rounds
    /// and classified failures for a cell that predates them.
    /// Serializes the two tests that exercise the campaign lock protocol:
    /// they share the isolated runs dir, so they must not overlap.
    static CAMPAIGN_LOCK_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Round-13: the console board applies the same hide/poison rule as the
    /// portal exporter (a poisoned cell is not a measurement anywhere).
    #[tokio::test]
    async fn test_campaign_board_skips_hidden_and_poisoned_cells() {
        isolate_test_runs_dir();
        let runs = crate::report::archive::RunArchiver::resolve_runs_dir();
        let make = |name: &str| {
            let d = runs.join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("campaign.json"),
                serde_json::json!({
                    "campaign_id": name, "name": "sdb-sql", "model": "m", "harness": "omp",
                    "effort": "high", "task": "sql", "started_at": "2026-10-18T00:00:00Z",
                    "finished_at": "2026-10-18T00:30:00Z", "stages": [], "reached": 1,
                    "regression_free_tickets": 1, "points": 1.0, "max_points": 36.0,
                    "rules_version": crate::report::rules::RULES_VERSION,
                    "total_cost_usd": 0.01, "over_budget": false, "over_wall": false
                })
                .to_string(),
            )
            .unwrap();
            d
        };
        let vis = make("campaign_sdb-sql_vis_20261018");
        let poisoned = make("campaign_sdb-sql_poison_20261018");
        let hidden = make("campaign_sdb-sql_hide_20261018");
        fs::write(poisoned.join(crate::report::portal::POISONED_FILE), "").unwrap();
        fs::write(hidden.join(crate::report::portal::PORTAL_HIDDEN_FILE), "").unwrap();
        let Json(cells) = get_campaigns().await;
        let ids: Vec<String> = cells
            .iter()
            .filter_map(|c| c["id"].as_str().map(str::to_string))
            .collect();
        assert!(ids.iter().any(|i| i.contains("vis")), "{ids:?}");
        assert!(!ids.iter().any(|i| i.contains("poison")), "{ids:?}");
        assert!(!ids.iter().any(|i| i.contains("hide")), "{ids:?}");
        for d in [vis, poisoned, hidden] {
            let _ = fs::remove_dir_all(d);
        }
    }

    /// Round-12 P1: the portal output is isolated the same way, so no test
    /// can overwrite the served board (the legacy exporter used to blank it
    /// from an isolated runs dir after any pipeline run).
    #[test]
    fn test_tests_write_the_portal_to_a_temp_dir() {
        isolate_test_runs_dir();
        let dir = crate::report::archive::RunArchiver::resolve_portal_dir();
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "tests must not write the real portal: {dir:?}"
        );
    }

    /// Round-8 finding 1: the suite points at an isolated runs dir, so a
    /// server request never reads, writes or deletes anything in the real
    /// runs/ directory — in particular a live cell's lock.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_server_never_touches_a_live_lock() {
        let _serial = CAMPAIGN_LOCK_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = create_test_state();
        let path = crate::campaign::campaign_lock_path();
        assert!(
            path.starts_with(std::env::temp_dir()),
            "tests must be isolated from the real runs dir: {path:?}"
        );
        let _ = std::fs::remove_file(&path);
        let lock = crate::campaign::CampaignLock::try_acquire_at(&path, "ut-guard").expect("lock");
        let before = std::fs::read_to_string(&path).unwrap();
        let base = serve_test_app(state).await;
        let client = authed_client();
        // /api/status reports it and the POST refuses on it...
        let body: serde_json::Value = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["campaign"]["campaign_id"], "ut-guard");
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "ut/m"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
        // ...without touching the file.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "the lock file must be byte-identical"
        );
        drop(lock);
    }

    /// gui-spec.md §4.4: starting a campaign validates its input and
    /// refuses to overlap a busy engine (spawning happens only when idle).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // a test-only serialization guard
    async fn test_start_campaign_validates_and_respects_busy_engine() {
        let _serial = CAMPAIGN_LOCK_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A live cell (a CLI campaign) legitimately owns the real lock, and
        // the validation path needs an idle engine: skip rather than fight
        // a campaign that is running.
        if crate::campaign::live_lock_holder().is_some() {
            eprintln!("skip: a campaign is running (the lock is live)");
            return;
        }
        let state = create_test_state();
        state
            .is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let base = serve_test_app(state).await;
        let client = authed_client();
        // Busy engine: conflict before any spawn.
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "ut/model"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
        // Empty model: bad request.
        let idle = create_test_state();
        let base = serve_test_app(idle).await;
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "   "}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        // A model with whitespace can never resolve: rejected too.
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "openrouter/a b"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        // An unknown effort is rejected, not passed through.
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "ut/m", "effort": "bogus"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        // A budget above $5 needs an explicit confirmation.
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "ut/m", "budget_usd": 9.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        // Round-7 finding 1: a live campaign lock refuses a second start,
        // and /api/status reports the very fact the 409 is based on. The
        // lock is taken and released inside this test so no other test can
        // race it.
        let path = crate::campaign::campaign_lock_path();
        let _ = std::fs::remove_file(&path);
        let lock = crate::campaign::CampaignLock::try_acquire_at(&path, "ut-live").expect("lock");
        let body: serde_json::Value = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["campaign"]["campaign_id"], "ut-live");
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "ut/m"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
        drop(lock);
        let body: serde_json::Value = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(body["campaign"].is_null(), "released lock reports null");
        let _ = std::fs::remove_file(&path);
    }

    /// Round-8 finding 8: a running cell with no rollup yet answers the
    /// detail endpoint with the campaign's ladder (its current ticket
    /// running), not a 404.
    #[tokio::test]
    async fn test_campaign_detail_running_without_a_rollup() {
        let base = serve_test_app(create_test_state()).await;
        let client = authed_client();
        let id = format!("campaign_sdb-sql_ut_live_{}", std::process::id());
        let dir = crate::report::archive::RunArchiver::resolve_runs_dir().join(&id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("status.json"),
            serde_json::json!({
                "state": "running",
                "current_stage": 5,
                "updated_at": chrono::Utc::now().to_rfc3339(),
                "rollup": serde_json::Value::Null,
                "started_at": "2026-10-17T14:31:53Z",
                "wall_cap_min": 120,
                "live": {
                    "campaign": "sdb-sql", "model": "ut/live-model", "harness": "omp",
                    "effort": "high", "campaign_budget": 0.75,
                    "ticket": 5, "title": "Crash safety", "type": "resilience",
                    "variant": "full", "round": 0, "phase": "agent", "turn": 3,
                    "max_turns": 28, "ticket_spent": 0.01, "ticket_budget": 0.20
                }
            })
            .to_string(),
        )
        .unwrap();
        let res = client
            .get(format!("{base}/api/campaigns/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["live_run"], true);
        assert_eq!(body["state"], "running");
        assert_eq!(body["rollup"]["model"], "ut/live-model");
        assert_eq!(body["budget_usd"], 0.75);
        assert_eq!(body["stop_reason"], "");
        let ladder = body["ladder"].as_array().expect("ladder");
        assert_eq!(
            ladder.iter().find(|e| e["id"] == 5).unwrap()["status"],
            "running"
        );
        assert_eq!(
            ladder.iter().find(|e| e["id"] == 0).unwrap()["status"],
            "not_reached"
        );
        // The list endpoint pins it first, ahead of every finished cell.
        let list: Vec<serde_json::Value> = client
            .get(format!("{base}/api/campaigns"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            list.first().map(|c| c["id"].clone()),
            Some(serde_json::json!(id.clone()))
        );
        // ...and carries the stub rollup the Board groups and renders from
        // (round-18 GUI: without it the running cell vanished from the
        // Board until its first ticket was recorded).
        let first = list.first().expect("the live cell");
        assert_eq!(first["rollup"]["model"], "ut/live-model");
        assert_eq!(
            first["rollup"]["rules_version"],
            crate::report::rules::RULES_VERSION
        );
        assert_eq!(first["rollup"]["max_points"], 76.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R22-5 acceptance: the Board list and the detail endpoint agree on a
    /// graded-crash relabel even when the stage already has recorded rounds.
    #[tokio::test]
    async fn r22_5_board_and_detail_agree_on_a_graded_crash_relabel() {
        let base = serve_test_app(create_test_state()).await;
        let client = authed_client();
        let id = format!("campaign_sdb-sql_ut_relabel_{}", std::process::id());
        let dir = crate::report::archive::RunArchiver::resolve_runs_dir().join(&id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("s8_full")).unwrap();
        let round = serde_json::json!({
            "round": 0, "passed_stages": 0, "total_stages": 1,
            "pass_rate": 0.0, "turns": 30, "cost_usd": 0.02,
            "duration_seconds": 900.0, "stop_reason": "crash",
            "derived": false
        });
        let stage = serde_json::json!({
            "id": 8, "title": "Joins", "type": "feature", "passed": false,
            "pass_rate": 0.0, "passed_stages": 0, "total_stages": 1,
            "regressed": [], "regression_severity": 0,
            "effective_cost_usd": 0.10, "api_cost_usd": 0.02,
            "turns": 30, "duration_seconds": 900.0, "status": "fault",
            "variant": "full", "tier": 3, "points": 0.0,
            "run_id": format!("{id}_s8_full"), "git_commit": null,
            "git_stat": null, "rounds": [round], "failure": null
        });
        let rollup = serde_json::json!({
            "campaign_id": id,
            "name": "sdb-sql",
            "model": "ut/model",
            "harness": "omp",
            "effort": "high",
            "task": "sql",
            "started_at": "2026-10-17T09:25:21Z",
            "finished_at": "2026-10-17T10:26:00Z",
            "stages": [stage],
            "reached": 7,
            "regression_free_tickets": 0,
            "total_cost_usd": 0.02,
            "over_budget": false,
            "over_wall": false,
            "rules_version": crate::report::rules::RULES_VERSION,
            "budget_usd": 0.99,
            "wall_min": 30,
            "points": 0.0,
            "max_points": 76.0
        });
        std::fs::write(dir.join("campaign.json"), rollup.to_string()).unwrap();
        std::fs::write(
            dir.join("s8_full").join("manifest.json"),
            serde_json::json!({"status": "agent_failed"}).to_string(),
        )
        .unwrap();

        let detail: serde_json::Value = client
            .get(format!("{base}/api/campaigns/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            detail["rollup"]["stages"][0]["status"], "failed",
            "detail: {detail}"
        );
        let list: Vec<serde_json::Value> = client
            .get(format!("{base}/api/campaigns"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let cell = list
            .iter()
            .find(|c| c["id"] == serde_json::json!(id))
            .expect("the relabel fixture in the list");
        assert_eq!(
            cell["rollup"]["stages"][0]["status"], "failed",
            "list: {cell}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_campaign_detail_ladder_and_backfill() {
        let base = serve_test_app(create_test_state()).await;
        let client = authed_client();
        let id = format!("campaign_sdb-sql_ut_detail_{}", std::process::id());
        let dir = crate::report::archive::RunArchiver::resolve_runs_dir().join(&id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("s8_full")).unwrap();
        // A cell that passed ticket 0, failed ticket 8 and ran out of
        // budget before the rest (cell-8-shaped).
        let rollup = serde_json::json!({
            "campaign_id": id,
            "name": "sdb-sql",
            "model": "ut/model",
            "harness": "omp",
            "effort": "high",
            "task": "sql",
            "started_at": "2026-10-17T09:25:21Z",
            "finished_at": "2026-10-17T11:26:00Z",
            "stages": [
                {"id": 0, "title": "Genesis", "type": "genesis", "passed": true,
                 "pass_rate": 100.0, "passed_stages": 1, "total_stages": 1,
                 "regressed": [], "regression_severity": 0, "effective_cost_usd": 0.1,
                 "turns": 28, "duration_seconds": 600.0, "status": "passed",
                 "variant": "full", "tier": 1, "points": 1.0,
                 "run_id": format!("{id}_s0_full"), "git_commit": null, "git_stat": null},
                {"id": 8, "title": "Joins", "type": "feature", "passed": false,
                 "pass_rate": 33.3, "passed_stages": 2, "total_stages": 6,
                 "regressed": [3, 4, 5], "regression_severity": 3,
                 "effective_cost_usd": 0.14, "turns": 30, "duration_seconds": 900.0,
                 "status": "failed", "variant": "full", "tier": 3, "points": 0.0,
                 "run_id": format!("{id}_s8_full"), "git_commit": null, "git_stat": null}
            ],
            "reached": 0,
            "regression_free_tickets": 1,
            "total_cost_usd": 0.24,
            "over_budget": true,
            "over_wall": false,
            "rules_version": "2026-10-17.15",
            "budget_usd": 0.25,
            "wall_min": 30,
            "points": 0.24 * 0.0,
            "max_points": 26.0
        });
        std::fs::write(dir.join("campaign.json"), rollup.to_string()).unwrap();
        // An old-style manifest (no rounds): the backfill derives the round
        // and classifies the failure.
        let manifest = serde_json::json!({
            "run_id": format!("{id}_s8_full"),
            "model": "ut/model",
            "task": "sql",
            "status": "failed_tests",
            "language": "python",
            "started_at": "2026-10-17T10:00:00Z",
            "completed_at": "2026-10-17T10:15:00Z",
            "duration_seconds": 900.0,
            "turns": 30,
            "pass_rate": 33.3,
            "passed_stages": 2,
            "total_stages": 6,
            "stages": [
                {"stage": 3, "name": "Stage 3", "passed": false,
                 "error": "Kind: expected OK 2, got OK 0"},
                {"stage": 8, "name": "Stage 8", "passed": false,
                 "error": "ErrorClass: expected ERR no_column, got ERR syntax"}
            ],
            "throughput_req_sec": null
        });
        std::fs::write(
            dir.join("s8_full").join("manifest.json"),
            manifest.to_string(),
        )
        .unwrap();

        let res = client
            .get(format!("{base}/api/campaigns/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["stop_reason"], "budget cap");
        assert_eq!(body["budget_usd"], 0.25);
        let ladder = body["ladder"].as_array().expect("ladder");
        // The declared ladder grows with the campaign; every declared
        // ticket is present, attempted or not.
        assert!(ladder.len() >= 12, "every ticket is on the ladder");
        assert_eq!(ladder[0]["status"], "pass");
        assert_eq!(ladder[8]["status"], "fail");
        // Ticket 8's failure is the manifest's failing stage 8 (owns its
        // stage) even though the old record carried no rounds.
        let rec = body["rollup"]["stages"][1].clone();
        assert_eq!(rec["rounds"][0]["derived"], true);
        assert_eq!(rec["failure"]["kind"], "own_stage");
        assert!(rec["failure"]["line"]
            .as_str()
            .unwrap()
            .contains("ERR syntax"));
        // Ticket 9 needs 3: skipped; ticket 11 needs [3,4]: skipped too;
        // ticket 1 is runnable but was never reached.
        let find = |id: u32| {
            ladder
                .iter()
                .find(|e| e["id"] == id)
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(find(1)["status"], "not_reached");
        assert_eq!(find(1)["cause"], "not reached: budget cap");
        assert_eq!(find(9)["status"], "skipped");
        assert_eq!(find(9)["cause"], "skipped: needs 3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_poison_and_visibility_missing_run_404() {
        let base = serve_test_app(create_test_state()).await;
        let client = authed_client();
        for path in [
            "/api/runs/sdb_ut_missing/poison",
            "/api/runs/sdb_ut_missing/portal",
        ] {
            let res = client
                .post(format!("{base}{path}"))
                .json(&serde_json::json!({"poisoned": true, "hidden": true}))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 404, "{path} must 404 before any write");
        }
        let res = client
            .post(format!("{base}/api/runs/..%2Fetc/poison"))
            .json(&serde_json::json!({"poisoned": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn console_worker_tail_and_since_cursors() {
        let state = create_test_state();
        for i in 0..10 {
            push_console_line(&state.log_buffer, format!("L{i}"));
        }
        let fetch = |worker: Option<u8>, tail: Option<usize>, since: Option<usize>| {
            let state = state.clone();
            async move {
                let req = axum::extract::Request::builder()
                    .uri("/api/console")
                    .body(axum::body::Body::empty())
                    .unwrap();
                let resp = get_active_console(
                    State(state),
                    Query(ConsoleQuery {
                        worker,
                        tail,
                        since,
                    }),
                    req,
                )
                .await;
                let total = resp
                    .headers()
                    .get("X-Console-Total")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                (total, response_text(resp).await)
            }
        };
        // Full buffer and explicit tail.
        let (total, body) = fetch(None, None, None).await;
        assert_eq!(total, "10");
        assert!(body.starts_with("L0\nL1"));
        assert!(body.ends_with("L9"));
        let (_, body) = fetch(None, Some(3), None).await;
        assert_eq!(body, "L7\nL8\nL9");
        // Unknown worker: empty body, total 0.
        let (total, body) = fetch(Some(7), None, None).await;
        assert_eq!(total, "0");
        assert!(body.is_empty());
        // Delta cursors.
        let (_, body) = fetch(None, None, Some(7)).await;
        assert_eq!(body, "L7\nL8\nL9");
        let (_, body) = fetch(None, None, Some(10)).await;
        assert!(body.starts_with("L0"), "past-end cursor re-syncs the tail");
        // Known cursor below the tail window re-sends the window, not a hole.
        let (_, body) = fetch(None, Some(3), Some(1)).await;
        assert_eq!(body, "L7\nL8\nL9");
    }

    #[tokio::test]
    async fn queue_crud_over_http_paused_and_validation() {
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        let state = create_test_state();
        let base = serve_test_app(state.clone()).await;
        let client = authed_client();

        // Pause first: enqueue's kick becomes a no-op, so no dispatch.
        let res = client
            .post(format!("{base}/api/queue/pause"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.json::<serde_json::Value>().await.unwrap()["paused"],
            true
        );
        assert!(state.queue_paused.load(Ordering::SeqCst));

        let res = client
            .post(format!("{base}/api/queue"))
            .json(&queue_test_req())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let first: serde_json::Value = res.json().await.unwrap();
        assert_eq!(first["started"], false);
        assert_eq!(first["position"], 1);
        let id = first["id"].as_str().unwrap().to_string();

        // Identical pending cell dedupes instead of double-spending.
        let res = client
            .post(format!("{base}/api/queue"))
            .json(&queue_test_req())
            .send()
            .await
            .unwrap();
        let dup: serde_json::Value = res.json().await.unwrap();
        assert_eq!(dup["deduped"], true);
        assert_eq!(dup["id"].as_str().unwrap(), id);

        // Remove unknown -> 404; remove pending -> 200.
        let res = client
            .delete(format!("{base}/api/queue/q999"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
        let res = client
            .delete(format!("{base}/api/queue/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("Removed"));

        // Clear keeps pending rows, drops finished history.
        let mut req = queue_test_req();
        req.trials = 2;
        let res = client
            .post(format!("{base}/api/queue"))
            .json(&req)
            .send()
            .await
            .unwrap();
        let id2 = res.json::<serde_json::Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut req = queue_test_req();
        req.trials = 3;
        let res = client
            .post(format!("{base}/api/queue"))
            .json(&req)
            .send()
            .await
            .unwrap();
        let id3 = res.json::<serde_json::Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        {
            let mut q = state.queue.lock().unwrap();
            q.iter_mut().find(|i| i.id == id3).unwrap().status = QueueStatus::Done;
        }
        let res = client
            .delete(format!("{base}/api/queue"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("Cleared 1"));
        assert_eq!(test_queue_of(&state).await.len(), 1);

        // Unknown task and malformed JSON are rejected before any dispatch.
        let mut bad = queue_test_req();
        bad.task = "no-such-task".to_string();
        let res = client
            .post(format!("{base}/api/queue"))
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        assert!(res.text().await.unwrap().contains("Unknown task"));
        let res = client
            .post(format!("{base}/api/queue"))
            .header("content-type", "application/json")
            .body("{ not json")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        assert_eq!(test_queue_of(&state).await.len(), 1);

        // Resume with only a parked pending row would kick it, so clear
        // first; resume itself must flip the flag and report ok.
        assert!(remove_queued_run(State(state.clone()), AxumPath(id2))
            .await
            .is_ok());
        let res = client
            .post(format!("{base}/api/queue/resume"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.json::<serde_json::Value>().await.unwrap()["paused"],
            false
        );
        assert!(!state.queue_paused.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn start_run_validation_over_http() {
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        let state = create_test_state();
        let base = serve_test_app(state.clone()).await;
        let client = authed_client();

        // Unknown task: slot is claimed then released, nothing dispatches.
        let mut req = queue_test_req();
        req.task = "no-such-task".to_string();
        let res = client
            .post(format!("{base}/api/run"))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        assert!(res.text().await.unwrap().contains("Unknown task"));
        assert!(state.running_slots.lock().unwrap().is_empty());
        assert!(!state.is_running.load(Ordering::SeqCst));

        // Malformed JSON is rejected by the extractor.
        let res = client
            .post(format!("{base}/api/run"))
            .header("content-type", "application/json")
            .body("{ nope")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // All workers busy -> 409 without touching the request.
        for w in 0..worker_count() as u8 {
            state.running_slots.lock().unwrap().insert(w);
        }
        state.is_running.store(true, Ordering::SeqCst);
        let res = client
            .post(format!("{base}/api/run"))
            .json(&queue_test_req())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
        state.running_slots.lock().unwrap().clear();
        state.is_running.store(false, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn csrf_and_method_guards_over_http() {
        let base = serve_test_app(create_test_state()).await;
        let client = reqwest::Client::new();
        let authed = authed_client();

        // Cross-site mutations are rejected before any handler/body parse.
        let res = client
            .post(format!("{base}/api/queue/resume"))
            .header("Origin", "https://evil.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 403);
        let res = client
            .post(format!("{base}/api/login"))
            .header("Referer", "https://evil.example.com/pwn")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 403);
        // Reads stay open regardless of Origin.
        let res = client
            .get(format!("{base}/api/status"))
            .header("Origin", "https://evil.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        // Method mismatches are 405s (past the guard), unknown routes 404.
        for (method, path) in [
            ("PUT", "/api/queue"),
            ("GET", "/api/run"),
            ("POST", "/api/status"),
            ("DELETE", "/api/run"),
        ] {
            let res = authed
                .request(method.parse().unwrap(), format!("{base}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 405, "{method} {path}");
        }
        let res = authed
            .post(format!("{base}/api/does-not-exist"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    fn test_cookie_of(resp: &reqwest::Response) -> String {
        resp.headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|c| c.strip_prefix("sdb_session="))
            .map(|c| c.split(';').next().unwrap_or("").to_string())
            .expect("session cookie")
    }

    // Env serialization deliberately spans awaits: the vars must stay
    // pinned for the requests under test. No handler takes TEST_ENV_LOCK,
    // so the guard cannot deadlock the runtime.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn login_session_bearer_and_logout_gates() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev: Vec<(&str, Option<String>)> = ["SDB_UI_TOKEN", "SDB_UI_USER", "SDB_UI_PASS"]
            .into_iter()
            .map(|k| (k, std::env::var(k).ok()))
            .collect();
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        std::env::set_var("SDB_UI_USER", "sdb-ut-user");
        std::env::set_var("SDB_UI_PASS", "sdb-ut-pass");

        let base = serve_test_app(create_test_state()).await;
        let client = reqwest::Client::new();
        let restore = move || {
            for (k, v) in prev {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        };

        // Unauthenticated /api/me advertises a required login.
        let res = client.get(format!("{base}/api/me")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let me: serde_json::Value = res.json().await.unwrap();
        assert_eq!(me["login"], false);
        assert_eq!(me["login_required"], true);

        // Wrong password -> 401; right one mints a session cookie.
        let res = client
            .post(format!("{base}/api/login"))
            .json(&serde_json::json!({"user": "sdb-ut-user", "pass": "wrong"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
        let res = client
            .post(format!("{base}/api/login"))
            .json(&serde_json::json!({"user": "sdb-ut-user", "pass": "sdb-ut-pass"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        let cookie = test_cookie_of(&res);

        // Session cookie satisfies /api/me and the mutating gate.
        let res = client
            .get(format!("{base}/api/me"))
            .header("Cookie", format!("sdb_session={cookie}"))
            .send()
            .await
            .unwrap();
        let me: serde_json::Value = res.json().await.unwrap();
        assert_eq!(me["login"], true);
        assert_eq!(me["user"], "sdb-ut-user");
        let res = client
            .post(format!("{base}/api/queue/resume"))
            .header("Cookie", format!("sdb_session={cookie}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // No session/bearer while login is configured -> 401 login hint.
        let res = client
            .post(format!("{base}/api/queue/resume"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
        assert!(res.text().await.unwrap().contains("login required"));
        // Bearer still works alongside login.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {TEST_UI_TOKEN}")).unwrap(),
        );
        let res = client
            .post(format!("{base}/api/queue/resume"))
            .headers(headers)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Logout drops the server-side session.
        let res = client
            .post(format!("{base}/api/logout"))
            .header("Cookie", format!("sdb_session={cookie}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        let res = client
            .get(format!("{base}/api/me"))
            .header("Cookie", format!("sdb_session={cookie}"))
            .send()
            .await
            .unwrap();
        let me: serde_json::Value = res.json().await.unwrap();
        assert_eq!(me["login"], false);

        restore();
    }

    /// Mock engine for GUI-proxy route tests: canned, deterministic, and
    /// entirely in-process (ephemeral port; never 3000/3001).
    async fn serve_mock_engine() -> String {
        async fn queue_post(Json(b): Json<serde_json::Value>) -> Response {
            if b["model"] == "fail-me" {
                return (StatusCode::INTERNAL_SERVER_ERROR, "engine refused").into_response();
            }
            Json(serde_json::json!({"id": "qe1", "started": false, "position": 1})).into_response()
        }
        let app = Router::new()
            .route(
                "/api/status",
                axum::routing::get(|| async { Json(serde_json::json!({"proxied": true})) }),
            )
            .route(
                "/api/queue",
                axum::routing::get(|| async { Json(serde_json::json!([])) })
                    .post(queue_post)
                    .delete(|| async { "Cleared 0" }),
            )
            .route(
                "/api/queue/:id",
                axum::routing::delete(|| async { "Removed qe1 from queue" }),
            )
            .route(
                "/api/campaigns",
                axum::routing::post(|Json(_b): Json<serde_json::Value>| async {
                    Json(serde_json::json!({"proxied_campaign": true}))
                }),
            )
            .route(
                "/api/queue/pause",
                axum::routing::post(|| async {
                    Json(serde_json::json!({"ok": true, "paused": true}))
                }),
            )
            .route(
                "/api/queue/resume",
                axum::routing::post(|| async {
                    Json(serde_json::json!({"ok": true, "paused": false}))
                }),
            )
            .route(
                "/api/console",
                axum::routing::get(|| async { ([("x-console-total", "1")], "engine line") }),
            )
            .route(
                "/api/run/stop",
                axum::routing::post(|| async { "Benchmark run stopped" }),
            )
            .route(
                "/api/run",
                axum::routing::post(|| async { "Benchmark started" }),
            )
            .route(
                "/api/runs/:id/extend",
                axum::routing::post(|| async { Json(serde_json::json!({"started": "parent1"})) }),
            );
        serve_test_app_on(app).await
    }

    async fn serve_test_app_on(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn proxy_mode_reads_mutations_and_errors() {
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        let engine = serve_mock_engine().await;
        let mut state = create_test_state();
        state.engine_url = engine;
        let base = serve_test_app(state.clone()).await;
        let client = authed_client();

        // Reads pass through (get_status + get_queue + console header copy).
        let res = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.json::<serde_json::Value>().await.unwrap()["proxied"],
            true
        );
        let res = client
            .get(format!("{base}/api/queue"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), "[]");
        let res = client
            .get(format!("{base}/api/console"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers()
                .get("x-console-total")
                .unwrap()
                .to_str()
                .unwrap(),
            "1"
        );
        assert_eq!(res.text().await.unwrap(), "engine line");

        // Round-7 finding 1: the campaign POST is proxied like the rest.
        let res = client
            .post(format!("{base}/api/campaigns"))
            .json(&serde_json::json!({"model": "ut/m", "budget_usd": 0.1}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.json::<serde_json::Value>().await.unwrap()["proxied_campaign"],
            true
        );

        // Mutations forward and parse engine responses.
        let res = client
            .post(format!("{base}/api/queue/resume"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.json::<serde_json::Value>().await.unwrap()["paused"],
            false
        );
        let res = client
            .post(format!("{base}/api/queue/pause"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let res = client
            .post(format!("{base}/api/queue"))
            .json(&queue_test_req())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.json::<serde_json::Value>().await.unwrap()["id"], "qe1");
        // Engine-side refusal keeps the engine status.
        let mut fail = queue_test_req();
        fail.model = "fail-me".to_string();
        let res = client
            .post(format!("{base}/api/queue"))
            .json(&fail)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 500);
        assert_eq!(res.text().await.unwrap(), "engine refused");
        let res = client
            .delete(format!("{base}/api/queue/qe1"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("Removed qe1"));
        let res = client
            .delete(format!("{base}/api/queue"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), "Cleared 0");
        let res = client
            .post(format!("{base}/api/run"))
            .json(&queue_test_req())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), "Benchmark started");
        let res = client
            .post(format!("{base}/api/runs/parent1/extend"))
            .json(&serde_json::json!({"trials": 3}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.json::<serde_json::Value>().await.unwrap()["started"],
            "parent1"
        );
        let res = client
            .post(format!("{base}/api/run/stop"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Unreachable engine: 502, never a local dispatch.
        let mut dead = create_test_state();
        dead.engine_url = "http://127.0.0.1:9".to_string();
        let dead_base = serve_test_app(dead.clone()).await;
        let res = client
            .get(format!("{dead_base}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 502);
        assert!(res.text().await.unwrap().contains("engine unreachable"));
        let res = client
            .post(format!("{dead_base}/api/queue/resume"))
            .send()
            .await
            .unwrap();
        // Proxy handlers translate an unreachable engine into a 200
        // {"ok": false} envelope for the GUI (the status code is not
        // forwarded) — pin that shape so a silent local fallback can't
        // masquerade as a successful remote resume.
        assert_eq!(res.status(), 200);
        assert_eq!(res.json::<serde_json::Value>().await.unwrap()["ok"], false);
        assert!(state.queue.lock().unwrap().is_empty());
    }

    /// Read from `sock` until the accumulated bytes contain `needle` or
    /// `wait` elapses. Used by the raw-socket SSE test (reqwest's stream
    /// feature is deliberately not enabled).
    async fn read_until(
        sock: &mut tokio::net::TcpStream,
        buf: &mut Vec<u8>,
        needle: &str,
        wait: Duration,
    ) -> bool {
        use tokio::io::AsyncReadExt;
        let deadline = tokio::time::Instant::now() + wait;
        let mut chunk = [0u8; 4096];
        loop {
            if String::from_utf8_lossy(buf).contains(needle) {
                return true;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match tokio::time::timeout(left, sock.read(&mut chunk)).await {
                Ok(Ok(0)) => return false,
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                _ => return String::from_utf8_lossy(buf).contains(needle),
            }
        }
    }

    #[tokio::test]
    async fn stream_logs_initial_live_and_lag_notice() {
        use tokio::io::AsyncWriteExt;
        let mut state = create_test_state();
        state
            .log_buffer
            .write()
            .unwrap()
            .push("[initial] boot line".to_string());
        let (tx, _keep_rx) = broadcast::channel(2);
        state.log_sender = tx;
        let base = serve_test_app(state.clone()).await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        sock.write_all(b"GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut seen_raw = Vec::new();
        assert!(
            read_until(
                &mut sock,
                &mut seen_raw,
                "text/event-stream",
                Duration::from_secs(3)
            )
            .await,
            "SSE route must answer with text/event-stream"
        );
        let head = String::from_utf8_lossy(&seen_raw);
        assert!(head.contains("HTTP/1.1 200"), "head: {head}");
        assert!(
            read_until(
                &mut sock,
                &mut seen_raw,
                "[initial] boot line",
                Duration::from_secs(3)
            )
            .await,
            "SSE replay must carry the buffered console tail"
        );

        // Live broadcast lines reach the stream.
        state.log_sender.send("[live] now".to_string()).unwrap();
        assert!(
            read_until(
                &mut sock,
                &mut seen_raw,
                "[live] now",
                Duration::from_secs(3)
            )
            .await,
            "live broadcast line must reach the stream"
        );

        // Overflow the 2-slot channel without reading between sends: the
        // stream must surface a catch-up notice instead of silent drops.
        for i in 0..8 {
            let _ = state.log_sender.send(format!("burst {i}"));
        }
        assert!(
            read_until(
                &mut sock,
                &mut seen_raw,
                "STREAM NOTICE",
                Duration::from_secs(3)
            )
            .await,
            "lagging stream must emit the catch-up notice; saw: {}",
            String::from_utf8_lossy(&seen_raw)
        );
    }

    #[tokio::test]
    async fn stream_logs_proxy_snapshot_and_engine_down_notice() {
        use tokio::io::AsyncWriteExt;
        // Engine up: the one-shot console snapshot is replayed as SSE data.
        let engine = serve_mock_engine().await;
        let mut state = create_test_state();
        state.engine_url = engine;
        let base = serve_test_app(state).await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        sock.write_all(b"GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        assert!(
            read_until(&mut sock, &mut buf, "engine line", Duration::from_secs(3)).await,
            "proxy SSE must replay the engine console snapshot"
        );

        // Engine down: a clear notice, never a hang or empty stream.
        let mut dead = create_test_state();
        dead.engine_url = "http://127.0.0.1:9".to_string();
        let dead_base = serve_test_app(dead).await;
        let dead_port: u16 = dead_base.rsplit(':').next().unwrap().parse().unwrap();
        let mut dsock = tokio::net::TcpStream::connect(("127.0.0.1", dead_port))
            .await
            .unwrap();
        dsock
            .write_all(b"GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut dbuf = Vec::new();
        assert!(
            read_until(
                &mut dsock,
                &mut dbuf,
                "engine console down",
                Duration::from_secs(3)
            )
            .await,
            "dead proxy engine must surface the down notice"
        );
    }

    #[test]
    fn compare_workspaces_status_matrix_and_missing_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "sdb_ut_compare_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("same.rs"), "fn main() {}\n").unwrap();
        fs::write(b.join("same.rs"), "fn main() {}\n").unwrap();
        fs::write(a.join("changed.rs"), "a\nb\nc\n").unwrap();
        fs::write(b.join("changed.rs"), "a\nx\nc\n").unwrap();
        fs::write(a.join("gone.rs"), "old\n").unwrap();
        fs::write(b.join("new.rs"), "new\n").unwrap();
        // Nested path exercises the recursive workspace scan.
        fs::create_dir_all(a.join("src")).unwrap();
        fs::create_dir_all(b.join("src")).unwrap();
        fs::write(a.join("src/lib.rs"), "one\n").unwrap();
        fs::write(b.join("src/lib.rs"), "two\n").unwrap();

        let entries = compare_workspaces(&a, &b);
        let by = |p: &str| entries.iter().find(|e| e.path == p).unwrap();
        assert_eq!(by("same.rs").status, "identical");
        assert!(by("same.rs").diff.is_empty());
        assert_eq!(by("changed.rs").status, "modified");
        let changed = by("changed.rs").diff.clone();
        assert!(changed.contains("-b") && changed.contains("+x"));
        assert_eq!(by("gone.rs").status, "removed_in_b");
        assert_eq!(by("gone.rs").lines_a, 1);
        assert_eq!(by("gone.rs").lines_b, 0);
        assert_eq!(by("new.rs").status, "added_in_b");
        assert_eq!(by("new.rs").lines_b, 1);
        assert_eq!(by("src/lib.rs").status, "modified");
        // Sorted paths keep the GUI diff order stable.
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted);
        // Both sides missing -> empty; one side missing -> added/removed.
        assert!(compare_workspaces(&dir.join("nope_a"), &dir.join("nope_b")).is_empty());
        let one = compare_workspaces(&a, &dir.join("nope_b"));
        assert!(one.iter().all(|e| e.status == "removed_in_b"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_current_run_derives_from_worker_map() {
        let state = create_test_state();
        state
            .worker_runs
            .write()
            .unwrap()
            .insert(2, active_run_info("redis_sdb-ut_20260102_w2"));
        let resp = get_status(State(state.clone())).await;
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["is_running"], true);
        assert_eq!(v["current_run"]["run_id"], "redis_sdb-ut_20260102_w2");
        assert_eq!(v["current_run"]["turns"], 1);
        assert_eq!(v["workers"]["2"]["tokens"], 42);
        assert_eq!(v["queue_paused"], false);
    }

    #[tokio::test]
    async fn stop_run_idle_full_and_queue_only_paths() {
        // Nothing running -> 400.
        let state = create_test_state();
        let resp = stop_run(
            State(state.clone()),
            Query(StopQuery { run: String::new() }),
        )
        .await;
        assert_eq!(resp.status(), 400);
        assert!(!state.is_running.load(Ordering::SeqCst));

        // Full stop: flag cleared, Running queue rows cancelled, logs written.
        let state = create_test_state();
        state.queue.lock().unwrap().push_back(QueuedRun {
            id: "q_ut_stop_me".to_string(),
            enqueued_at: "2026-01-02T00:00:00Z".to_string(),
            status: QueueStatus::Running,
            note: None,
            refusals: 0,
            not_before: None,
            kind: QueueKind::Run(queue_test_req()),
        });
        state.is_running.store(true, Ordering::SeqCst);
        let resp = stop_run(
            State(state.clone()),
            Query(StopQuery { run: String::new() }),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(!state.is_running.load(Ordering::SeqCst));
        assert!(state.cancel_requested.load(Ordering::SeqCst));
        assert_eq!(
            state.queue.lock().unwrap()[0].status,
            QueueStatus::Cancelled
        );
        assert!(state
            .log_buffer
            .read()
            .unwrap()
            .iter()
            .any(|l| l.contains("stopped by user")));

        // Fragment naming no live cell -> 404 (engine flagged busy).
        let state = create_test_state();
        state.is_running.store(true, Ordering::SeqCst);
        let resp = stop_run(
            State(state.clone()),
            Query(StopQuery {
                run: "sdb-ut-no-such-run".to_string(),
            }),
        )
        .await;
        assert_eq!(resp.status(), 404);

        // Queue-only match (no worker_runs): cancels the row, no sandbox
        // cleanup path is touched because `hits` is empty.
        let state = create_test_state();
        state.is_running.store(true, Ordering::SeqCst);
        state.queue.lock().unwrap().push_back(QueuedRun {
            id: "sdb_ut_cell_20260102".to_string(),
            enqueued_at: "2026-01-02T00:00:00Z".to_string(),
            status: QueueStatus::Running,
            note: None,
            refusals: 0,
            not_before: None,
            kind: QueueKind::Run(queue_test_req()),
        });
        let resp = stop_run(
            State(state.clone()),
            Query(StopQuery {
                run: "sdb_ut_cell".to_string(),
            }),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            state.queue.lock().unwrap()[0].status,
            QueueStatus::Cancelled
        );
        assert!(state.worker_runs.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn integrity_reports_refused_queue_shapes() {
        let state = create_test_state();
        let mut done = test_queue_item("q_refused_final");
        done.status = QueueStatus::Done;
        done.note = Some("refused-final (4 refusals): load gate".to_string());
        let mut parked = test_queue_item("q_refused_parked");
        parked.note = Some("refused (attempt 1/3): load gate".to_string());
        let mut other = test_queue_item("q_plain");
        other.status = QueueStatus::Done;
        other.note = Some("5/9 trials passing".to_string());
        {
            let mut q = state.queue.lock().unwrap();
            q.push_back(done);
            q.push_back(parked);
            q.push_back(other);
        }
        let Json(body) = get_integrity(State(state.clone())).await;
        assert!(!body.ok, "a terminally refused cell needs a human");
        assert_eq!(body.refused_final, 1);
        assert_eq!(body.pending_refused, 1);
        assert_eq!(body.orphans, 0);
        assert_eq!(body.junk_parents, 0);
    }

    #[tokio::test]
    async fn extend_invalid_id_rejected_over_http() {
        std::env::set_var("SDB_UI_TOKEN", TEST_UI_TOKEN);
        let base = serve_test_app(create_test_state()).await;
        let client = authed_client();
        let res = client
            .post(format!("{base}/api/runs/..%2Fetc/extend"))
            .json(&serde_json::json!({"trials": 3}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn unified_diff_lookahead_covers_insert_and_delete_runs() {
        let ins = generate_unified_diff("m.rs", "a\nb\nc\n", "a\ninserted\nb\nc\n");
        assert!(
            !ins.lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---")),
            "pure insert: {ins}"
        );
        assert!(ins.contains("+inserted"));
        let del = generate_unified_diff("m.rs", "a\nb\nc\n", "a\nc\n");
        assert!(del.contains("-b"));
        assert!(
            !del.lines()
                .any(|l| l.starts_with('+') && !l.starts_with("+++")),
            "pure delete: {del}"
        );
        // A farther-apart insertion exercises the 2..=5 lookahead window.
        let far = generate_unified_diff("m.rs", "a\nb\nc\nd\n", "a\nb\nc\nx\nd\n");
        assert!(far.contains("+x"));
    }

    // Env serialization deliberately spans the handler await; no code
    // path in the handler takes TEST_ENV_LOCK.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn balance_check_without_key_stays_offline() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::set_var("OPENROUTER_API_KEY", "");
        let state = create_test_state();
        let Json(body) = balance_check_handler(State(state)).await;
        assert_eq!(body["ok"], false);
        assert_eq!(body["reason"], "no API key configured");
        match prev {
            Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
    }

    #[test]
    fn clean_run_id_and_host_normalization_edges() {
        assert_eq!(clean_run_id("abc").as_deref(), Some("abc"));
        assert!(clean_run_id("").is_none());
        assert!(clean_run_id("a/b").is_none());
        assert!(clean_run_id(".").is_none());
        assert_eq!(host_without_port("[::1]:3000"), "::1");
        assert_eq!(host_without_port("[::1]"), "::1");
        assert_eq!(host_without_port("Example.COM:8080"), "example.com");
        assert_eq!(host_without_port("bench.example.com"), "bench.example.com");
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("bench.example.com"));
        assert_eq!(
            origin_host_from_url("https://user@bench.example.com:9443/path"),
            "bench.example.com"
        );
    }
}
