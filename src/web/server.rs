use crate::config::TaskType;
use crate::pipeline::{BenchmarkConfig, BenchmarkPipeline, PipelineLogger};
use crate::report::{
    assess_stability, is_hidden, set_hidden, BenchmarkRunResult, EnvironmentInfo, FileInfo,
    LeaderboardManager, PortalExporter, RunArchiver, RunManifest, Stability, Store,
};
use crate::sandbox::SandboxManager;
use axum::{
    extract::{Path as AxumPath, Query, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Redirect, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

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

#[derive(Clone)]
pub struct AppState {
    pub log_sender: broadcast::Sender<String>,
    pub is_running: Arc<AtomicBool>,
    pub cancel_requested: Arc<AtomicBool>,
    pub current_run: Arc<RwLock<Option<ActiveRunInfo>>>,
    pub last_run: Arc<RwLock<Option<ActiveRunInfo>>>,
    pub log_buffer: Arc<RwLock<Vec<String>>>,
    pub task_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pub queue: Arc<std::sync::Mutex<VecDeque<QueuedRun>>>,
    pub queue_seq: Arc<AtomicU64>,
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
    /// True when the server runs on a temp-dir fallback DB instead of
    /// repo-root bench.db (queue history not durable). Defaults false for
    /// older clients that omit the field.
    #[serde(default)]
    pub db_fallback: bool,
}

/// Read-through from the engine in GUI-proxy mode.
/// Returns Some(response) when proxied; None in single-process mode.
async fn proxy_read(state: &AppState, path: &str, query: &str) -> Option<Response> {
    if state.engine_url.is_empty() {
        return None;
    }
    let url = format!("{}{}{}", state.engine_url.trim_end_matches('/'), path, query);
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

async fn get_status(State(state): State<AppState>) -> Response {
    if let Some(proxied) = proxy_read(&state, "/api/status", "").await {
        return proxied;
    }
    let is_running = state.is_running.load(Ordering::SeqCst);
    let current_run = state.current_run.read().unwrap().clone();
    let last_run = state.last_run.read().unwrap().clone();
    Json(StatusResponse {
        is_running,
        current_run,
        last_run,
        db_fallback: state.db_fallback,
    })
    .into_response()
}

fn default_trials() -> u32 {
    9
}

fn default_max_trials() -> u32 {
    9
}

fn default_max_turns() -> u32 {
    100
}

fn default_budget() -> f64 {
    1.00
}

fn default_timeout_min() -> Option<u64> {
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
}

/// Last-resort in-memory Store when even the temp fallback cannot open
/// (e.g. read-only filesystem): creates a temp-dir DB that is never read
/// back. Queue dispatch never blocks on persistence.
fn tempfile_mem_store() -> Store {
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
fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// Best-effort mirror of an in-memory queue transition into bench.db.
/// Memory stays dispatch truth; DB failures are logged, never fatal.
fn db_mirror(state: &AppState, what: &str, f: impl FnOnce(&Store)) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&state.store))) {
        Ok(()) => {}
        Err(_) => eprintln!("[QUEUE-DB] {what}: store panicked (memory still authoritative)"),
    }
}

/// Mirror helper that logs DB-side errors without touching dispatch.
fn db_try(what: &str, r: anyhow::Result<()>) {
    if let Err(e) = r {
        eprintln!("[QUEUE-DB] {what}: {e:#}");
    }
}

/// Mark a queue item finished in both memory and the DB mirror.
/// Broadcast a status/console line AND mirror it into the in-memory console
/// buffer so late SSE joiners (replay) and /api/console readers see the same
/// lifecycle tail (DONE/STOP/STATUS/[EXTEND]/[MULTI-TRIAL]) that live
/// subscribers got over the channel.
fn send_status(tx: &broadcast::Sender<String>, log_buf: &Arc<RwLock<Vec<String>>>, line: String) {
    push_console_line(log_buf, line.clone());
    let _ = tx.send(line);
}

fn mark_queue_done(state: &AppState, queue_id: &str) {
    {
        let mut q = state.queue.lock().unwrap();
        if let Some(item) = q.iter_mut().find(|i| i.id == queue_id) {
            item.status = QueueStatus::Done;
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

/// Replay durable queue rows into memory at startup. Running rows reset to
/// Pending (the previous process died mid-dispatch); finished history is
/// not replayed. Returns the replayed items in enqueue order.
fn replay_queue(store: &Store) -> Vec<QueuedRun> {
    let mut items = store.load_queue(false);
    for item in items.iter_mut() {
        if item.status == QueueStatus::Running {
            item.status = QueueStatus::Pending;
        }
    }
    items
}

#[cfg(test)]
pub fn test_queue_item(id: &str) -> QueuedRun {
    QueuedRun {
        id: id.to_string(),
        enqueued_at: "2026-09-25T00:00:00Z".to_string(),
        status: QueueStatus::Pending,
        kind: QueueKind::Run(RunRequest {
            model: "test-model".to_string(),
            task: "dns".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 5,
            eval_only: true,
            effort: Some("low".to_string()),
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
fn queue_seq_start(store: &Store) -> u64 {
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

fn resolve_results_dir() -> PathBuf {
    crate::config::get_repo_root().join("results")
}

fn resolve_portal_dir() -> PathBuf {
    crate::config::get_repo_root().join("portal")
}

fn portal_content_type(path: &Path, bytes: &[u8]) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "md" => "text/markdown; charset=utf-8",
        "log" | "txt" => "text/plain; charset=utf-8",
        _ => {
            // Render text sources inline; only true binaries download.
            if std::str::from_utf8(bytes).is_ok_and(|t| !t.contains('\0')) {
                "text/plain; charset=utf-8"
            } else {
                "application/octet-stream"
            }
        }
    }
}

async fn serve_portal_index() -> Response {
    serve_portal(AxumPath(String::new())).await
}

async fn serve_portal(AxumPath(sub): AxumPath<String>) -> Response {
    let root = resolve_portal_dir();
    let mut full = root.clone();
    for comp in Path::new(&sub).components() {
        match comp {
            std::path::Component::Normal(part) => full.push(part),
            _ => return (StatusCode::FORBIDDEN, "portal: invalid path").into_response(),
        }
    }
    if full.is_dir() {
        if !sub.is_empty() && !sub.ends_with("/") {
            return Redirect::permanent(&format!("/portal/{}/", sub)).into_response();
        }
        full.push("index.html");
    }
    match std::fs::read(&full) {
        Ok(bytes) => {
            let ct = portal_content_type(&full, &bytes);
            ([("content-type", ct)], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "portal: not found").into_response(),
    }
}

#[allow(dead_code)]
fn resolve_workspace_dir() -> PathBuf {
    let ws = crate::config::get_repo_root().join("workspace");
    let _ = std::fs::create_dir_all(&ws);
    ws
}

pub struct UiServer;

impl UiServer {
    pub fn build_app(state: AppState) -> Router {
        Router::new()
            .route("/", get(serve_index))
            .route("/api/models", get(get_models))
            .route("/api/prompt/:task", get(get_prompt).post(save_prompt))
            .route("/api/leaderboard", get(get_leaderboard))
            .route("/api/status", get(get_status))
            .route("/api/run", post(start_run))
            .route("/api/run/stop", post(stop_run))
            .route(
                "/api/queue",
                get(get_queue).post(enqueue_run).delete(clear_queue),
            )
            .route("/api/queue/:id", delete(remove_queued_run))
            .route("/api/compare", get(compare_runs))
            .route("/api/stream", get(stream_logs))
            .route("/api/console", get(get_active_console))
            .route("/api/runs", get(get_runs))
            .route("/api/runs/:id", get(get_run_detail))
            .route("/api/runs/:id/log", get(get_run_log))
            .route("/api/runs/:id/files", get(get_run_files))
            .route("/api/runs/:id/file", get(get_run_file))
            .route("/portal", get(|| async { Redirect::permanent("/portal/") }))
            .route("/portal/", get(serve_portal_index))
            .route("/api/portal/rebuild", post(rebuild_portal))
            .route("/api/runs/:id/portal", post(set_run_portal_visibility))
            .route("/api/runs/:id/extend", post(extend_parent_run))
            .route("/api/runs/:id/review", get(get_run_review))
            .route("/api/runs/:id", delete(delete_run))
            .route("/portal/*path", get(serve_portal))
            .route("/api/sandbox/omp", get(get_sandbox_omp_status))
            .route("/api/sandbox/omp/update", post(update_sandbox_omp))
            .route("/api/env", get(get_env))
            .route_layer(axum::middleware::from_fn(csrf_guard))
            .with_state(state)
    }

    pub async fn start(host: &str, port: u16, engine_url: &str) -> anyhow::Result<()> {
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
                eprintln!("[QUEUE-DB] open {} failed ({e:#}); falling back to temp-dir DB", db_path.display());
                let fallback = std::env::temp_dir().join(format!("sdb-queue-fallback-{}.db", std::process::id()));
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
            println!("[QUEUE-DB] replayed {} pending item(s) from bench.db", replayed.len());
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
            store,
            db_fallback,
            engine_url: engine_url.to_string(),
        };

        // Replay auto-dispatch (engine only; proxy never kicks).
        // Pending work that survived the restart runs without a manual kick.
        // is_running starts false; the memory queue stays authoritative and
        // the DB mirror is best-effort in kick_runner.
        if proxy_mode {
            println!("[QUEUE-DB] proxy mode: auto-dispatch disabled (engine owns it)");
        }
        if !proxy_mode {
            if let Some(first_id) = state
                .queue
                .lock()
                .unwrap()
                .iter()
                .find(|i| i.status == QueueStatus::Pending)
                .map(|i| i.id.clone())
            {
                let kick_state = state.clone();
                tokio::spawn(async move {
                    kick_runner(&kick_state, &first_id).await;
                });
            }
        }

        let app = Self::build_app(state);

        // Background check and auto-update of sandbox OMP on server startup
        tokio::spawn(async {
            crate::sandbox::SandboxManager::prune_stale_networks();
            if let Err(e) = crate::sandbox::omp_updater::auto_update_if_needed().await {
                eprintln!("[OMP Startup Check Warning] {}", e);
            }
        });

        let addr = format!("{}:{}", host, port);
        println!("🚀 SubDollarBench GUI listening on: http://{}", addr);
        if host == "0.0.0.0" || host == "127.0.0.1" {
            println!("   Local URL:  http://localhost:{}", port);
        }

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

async fn serve_index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn get_models(
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Vec<SubDollarModel>> {
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

    let api_key = header_key
        .filter(|k| !k.is_empty())
        .or_else(|| {
            std::env::var("OPENROUTER_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
        })
        .or_else(|| params.get("key").filter(|k| !k.trim().is_empty()).cloned());

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

                if (prompt_p + comp_p) / 2.0 <= 1.0 && (prompt_p > 0.0 || comp_p > 0.0) {
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
        }
    }

    models.sort_by(|a, b| {
        ((a.prompt_price_per_m + a.completion_price_per_m) / 2.0)
            .partial_cmp(&((b.prompt_price_per_m + b.completion_price_per_m) / 2.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Json(models)
}

async fn get_prompt(AxumPath(task): AxumPath<String>) -> Result<String, StatusCode> {
    validate_task_name(&task)?;
    let path = resolve_task_path(&task);
    Ok(fs::read_to_string(path).unwrap_or_else(|_| "# Task prompt not found".to_string()))
}

async fn save_prompt(
    AxumPath(task): AxumPath<String>,
    body: String,
) -> Result<&'static str, StatusCode> {
    validate_task_name(&task)?;
    let path = resolve_task_path(&task);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(path, body) {
        Ok(_) => Ok("Prompt saved successfully"),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Deserialize)]
pub struct LeaderboardQuery {
    pub task: Option<String>,
}

async fn get_leaderboard(Query(query): Query<LeaderboardQuery>) -> Json<Vec<serde_json::Value>> {
    let r_dir = resolve_results_dir();
    let mut list = LeaderboardManager::load_all(r_dir.to_str().unwrap_or("./results"));
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
                v
            })
            .collect(),
    )
}

fn refresh_portal() {
    let root = crate::config::get_repo_root();
    let _ = PortalExporter::export(
        RunArchiver::resolve_runs_dir().to_str().unwrap_or("./runs"),
        resolve_results_dir().to_str().unwrap_or("./results"),
        root.join("portal").to_str().unwrap_or("./portal"),
        "",
        200,
    );
}

/// Same-origin guard for mutating endpoints. Browser navigations always
/// attach Origin (or Referer for same-origin form/DELETE submissions);
/// curl/CLI send neither and stay allowed. Loopback origins are always
/// trusted; anything else is rejected as cross-site.
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
    let host = candidate
        .split("://")
        .last()
        .unwrap_or(candidate)
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "127.0.0.1" || host == "localhost" || host == "::1" || host == "[::1]"
}

fn csrf_reject() -> Response {
    (
        axum::http::StatusCode::FORBIDDEN,
        "Cross-origin request rejected (Origin/Referer check)",
    )
        .into_response()
}

/// Axum layer guarding every mutating endpoint (POST/PUT/PATCH/DELETE).
/// Browser cross-site requests always carry Origin; curl/CLI send neither
/// (allowed) and loopback Origins are trusted. Rejection happens before
/// dispatch, so direct handler unit tests (no Origin) stay green.
async fn csrf_guard(req: Request, next: middleware::Next) -> Response {
    let mutating = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if mutating && !csrf_origin_allowed(req.headers()) {
        return csrf_reject();
    }
    next.run(req).await
}

fn clean_run_id(run_id: &str) -> Option<String> {
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
    v
}

async fn get_run_review(AxumPath(run_id): AxumPath<String>) -> Response {
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
async fn delete_run(State(state): State<AppState>, AxumPath(run_id): AxumPath<String>) -> Response {
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

async fn set_run_portal_visibility(
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

async fn rebuild_portal() -> Response {
    let root = crate::config::get_repo_root();
    match PortalExporter::export(
        RunArchiver::resolve_runs_dir().to_str().unwrap_or("./runs"),
        resolve_results_dir().to_str().unwrap_or("./results"),
        root.join("portal").to_str().unwrap_or("./portal"),
        "",
        200,
    ) {
        Ok(summary) => Json(serde_json::json!({"run_count": summary.run_count})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("portal export failed: {}", e),
        )
            .into_response(),
    }
}

async fn get_runs(State(state): State<AppState>) -> Json<Vec<serde_json::Value>> {
    // DB-first (same pattern as load_all); FS fallback until migration.
    let from_db = state.store.load_run_manifests();
    let list = if from_db.is_empty() {
        RunArchiver::list_runs(&RunArchiver::resolve_runs_dir())
    } else {
        from_db
    };
    Json(list.iter().map(manifest_with_portal_flag).collect())
}

async fn get_run_detail(
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

async fn get_run_log(AxumPath(run_id): AxumPath<String>) -> Response {
    let r_dir = RunArchiver::resolve_runs_dir();
    match RunArchiver::get_console_log(&r_dir, &run_id) {
        Some(log) => log.into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "Console log not found").into_response(),
    }
}

async fn get_run_files(
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<Vec<FileInfo>>, Response> {
    let r_dir = RunArchiver::resolve_runs_dir();
    let ws_dir = r_dir.join(&run_id).join("workspace");
    if ws_dir.exists() {
        Ok(Json(RunArchiver::scan_workspace_files(&ws_dir)))
    } else {
        Err((axum::http::StatusCode::NOT_FOUND, "Workspace not found").into_response())
    }
}

async fn get_run_file(
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
struct ConsoleQuery {
    /// Max lines to return (newest). Defaults to the buffer cap.
    tail: Option<usize>,
    /// Lines the client already holds; returns only newer lines plus a
    /// `X-Console-Total` header so the client can detect truncation gaps.
    since: Option<usize>,
}

async fn get_active_console(
    State(state): State<AppState>,
    Query(q): Query<ConsoleQuery>,
    request: axum::extract::Request,
) -> Response {
    if !state.engine_url.is_empty() {
        let qs = request.uri().query().map(|x| format!("?{x}")).unwrap_or_default();
        if let Some(proxied) = proxy_read(&state, "/api/console", &qs).await {
            return proxied;
        }
    }
    let buf = state.log_buffer.read().unwrap();
    let total = buf.len();
    let cap = q.tail.unwrap_or(MAX_CONSOLE_LINES).min(total.max(1));
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

async fn get_env() -> Json<EnvironmentInfo> {
    Json(EnvironmentInfo::detect())
}

async fn stop_run(State(state): State<AppState>) -> Response {
    if let Some(proxied) = proxy_mutation(&state, "POST", "/api/run/stop", None).await {
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

    state.cancel_requested.store(true, Ordering::SeqCst);
    let tx = state.log_sender.clone();
    let log_buf = state.log_buffer.clone();
    send_status(
        &tx,
        &log_buf,
        "[USER ACTION] Cancellation requested. Terminating sandbox containers...".to_string(),
    );

    let maybe_handle = state.task_handle.lock().unwrap().take();
    if let Some(handle) = maybe_handle {
        handle.abort();
    }

    let maybe_run = state.current_run.read().unwrap().clone();
    if let Some(info) = maybe_run {
        let sandbox = SandboxManager::with_id(&info.run_id);
        sandbox.cleanup_instance();
    } else {
        let sandbox = SandboxManager::new();
        sandbox.cleanup_instance();
    }

    *state.current_run.write().unwrap() = None;
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
    state.is_running.store(false, Ordering::SeqCst);
    send_status(
        &tx,
        &log_buf,
        "[DONE] Benchmark run was stopped by user.".to_string(),
    );

    (axum::http::StatusCode::OK, "Benchmark run stopped").into_response()
}

async fn compare_runs(
    Query(query): Query<CompareQuery>,
) -> Result<Json<RunComparisonResponse>, Response> {
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

    let ws_a = runs_dir.join(&query.run_a).join("workspace");
    let ws_b = runs_dir.join(&query.run_b).join("workspace");

    let files_a = if ws_a.exists() {
        RunArchiver::scan_workspace_files(&ws_a)
    } else {
        Vec::new()
    };
    let files_b = if ws_b.exists() {
        RunArchiver::scan_workspace_files(&ws_b)
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

    Ok(Json(RunComparisonResponse {
        run_a: manifest_a,
        run_b: manifest_b,
        metric_diff,
        file_diffs,
    }))
}

/// Max lines retained in the in-memory live console. Bounds per-fetch
/// payloads, SSE replay on connect, and steady-state memory while a run
/// streams. Full history stays in runs/<id>/console.log on disk.
const MAX_CONSOLE_LINES: usize = 3000;

fn push_console_line(log_buf: &Arc<RwLock<Vec<String>>>, line: String) {
    let mut buf = log_buf.write().unwrap();
    buf.push(line);
    let overflow = buf.len().saturating_sub(MAX_CONSOLE_LINES);
    if overflow > 0 {
        buf.drain(..overflow);
    }
}

struct WebLogger {
    tx: broadcast::Sender<String>,
    log_buf: Arc<RwLock<Vec<String>>>,
    current_run: Arc<RwLock<Option<ActiveRunInfo>>>,
    trial_idx: u32,
    total_trials: u32,
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

struct ExtBatchSpec {
    model: String,
    task: TaskType,
    effort: String,
    max_turns: u32,
    budget_usd: f64,
    timeout_min: u64,
    api_key: Option<String>,
    eval_only: bool,
    save_results: bool,
    review_model: Option<String>,
    parent_run_id: String,
    start_idx: u32,
    count: u32,
    display_total: u32,
}

struct ExtBatchOut {
    results: Vec<BenchmarkRunResult>,
    passing: usize,
    drift: Option<bool>,
}

/// Run one extension batch (fresh batch reference, suffix-numbered trial dirs).
/// Shared by auto-extend and the extend endpoint; never touches the runner slot.
async fn run_extension_batch(
    tx: &broadcast::Sender<String>,
    log_buf: &Arc<RwLock<Vec<String>>>,
    current_run: &Arc<RwLock<Option<ActiveRunInfo>>>,
    cancel: &Arc<AtomicBool>,
    batch_log: &(dyn Fn(&str) + Sync),
    spec: ExtBatchSpec,
) -> ExtBatchOut {
    let ext_batch_ref = if spec.count > 1 {
        match crate::bench::batch_ref::BatchRef::start(&spec.task, &spec.parent_run_id, batch_log) {
            Ok(b) => b,
            Err(e) => {
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
    for i in 0..spec.count {
        if cancel.load(Ordering::SeqCst) {
            send_status(tx, log_buf, "[STOP] Extension cancelled by user.".to_string());
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
            max_turns: spec.max_turns,
            budget_usd: spec.budget_usd,
            timeout_min: spec.timeout_min,
            api_key: spec.api_key.clone(),
            workdir: crate::config::get_repo_root().join("workspace"),
            eval_only: spec.eval_only,
            run_id: Some(run_id.clone()),
            save_results: spec.save_results,
            review_model: spec.review_model.clone(),
            batch_reference: ext_batch_ref.as_ref().map(|b| b.reference()),
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
                logger.log(&format!(
                    "[SUBRUN ERROR] Extension subrun {} failed: {}",
                    trial_idx, e
                ));
                break;
            }
        }
    }
    let drift = ext_batch_ref.map(|b| b.finish(batch_log).drift);
    ExtBatchOut {
        results,
        passing,
        drift,
    }
}

/// Pop the next pending queue item and spawn it. Returns true when an item
/// was spawned. Callers must own the runner slot (chain holder or kick CAS
/// winner). Extends re-validate at dispatch: a parent stabilized by an
/// earlier item in the same chain is marked Done with a log line, never run.
fn pump_queue(state: &AppState) -> bool {
    let next = {
        let mut q = state.queue.lock().unwrap();
        q.iter_mut()
            .find(|i| i.status == QueueStatus::Pending)
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
                format!("[QUEUE] Starting queued run {id}..."),
            );
            spawn_run_body(state, req, Some(id));
            true
        }
        Some((QueueKind::Extend(ext), id)) => {
            spawn_extend_body(state, ext, id);
            true
        }
        None => false,
    }
}

/// Spawn an extend with fresh dispatch-time validation. The runner slot is
/// already held by the caller; this fn never releases it — the spawned task
/// hands off via pump_queue or stores false when idle, like run bodies.
fn spawn_extend_body(state: &AppState, ext: ExtendRequest, queue_id: String) {
    let state_clone = state.clone();
    let handle = tokio::spawn(execute_extend_body(state_clone, ext, queue_id));
    *state.task_handle.lock().unwrap() = Some(handle);
}

/// Queued-extend worker: re-read the parent under the held slot, skip when
/// it no longer needs trials, else run the extension batch and re-merge.
/// Trial numbering derives from the fresh subrun count, so chained extends
/// on one parent never collide on trial dirs.
async fn execute_extend_body(state: AppState, ext: ExtendRequest, queue_id: String) {
    let tx = state.log_sender.clone();
    let log_buf = state.log_buffer.clone();
    let finish_item = |state: &AppState, note: &str| {
        mark_queue_done(state, &queue_id);
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
                &format!("[EXTEND-QUEUE] {queue_id}: parent '{}' gone; skipping.", ext.parent_run_id),
            );
            *state.task_handle.lock().unwrap() = None;
            if !pump_queue(&state) {
                state.is_running.store(false, Ordering::SeqCst);
                send_status(&tx, &log_buf, "[DONE]".to_string());
            }
            return;
        }
    };
    if data.subruns.len() < 2 {
        finish_item(
            &state,
            &format!("[EXTEND-QUEUE] {queue_id}: '{}' is not multi-trial; skipping.", ext.parent_run_id),
        );
        *state.task_handle.lock().unwrap() = None;
        if !pump_queue(&state) {
            state.is_running.store(false, Ordering::SeqCst);
            send_status(&tx, &log_buf, "[DONE]".to_string());
        }
        return;
    }
    let (stab, reason) = assess_stability(&data.subruns);
    if stab == Stability::Stable {
        finish_item(
            &state,
            &format!("[EXTEND-QUEUE] {queue_id}: '{}' already stable ({reason}); skipping.", ext.parent_run_id),
        );
        *state.task_handle.lock().unwrap() = None;
        if !pump_queue(&state) {
            state.is_running.store(false, Ordering::SeqCst);
            send_status(&tx, &log_buf, "[DONE]".to_string());
        }
        return;
    }
    let task_type = match data.task.as_str() {
        "redis" => TaskType::Redis,
        "http" => TaskType::Http,
        "dns" => TaskType::Dns,
        "pdf" => TaskType::Pdf,
        "wal" => TaskType::Wal,
        "proxy" => TaskType::Proxy,
        "pubsub" => TaskType::Pubsub,
        "binproto" => TaskType::Binproto,
        _ => {
            finish_item(
                &state,
                &format!("[EXTEND-QUEUE] {queue_id}: unknown task '{}'; skipping.", data.task),
            );
            *state.task_handle.lock().unwrap() = None;
            if !pump_queue(&state) {
                state.is_running.store(false, Ordering::SeqCst);
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
    )
    .await;
    if !out.results.is_empty() {
        let mut combined = data.subruns.clone();
        combined.extend(out.results);
        let passing = combined.iter().filter(|r| r.pass_rate >= 100.0).count();
        let merged = LeaderboardManager::consolidate_multi_trial_run(
            &ext.parent_run_id,
            &combined,
            true,
            out.drift,
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
    mark_queue_done(&state, &queue_id);
    *state.task_handle.lock().unwrap() = None;
    if !pump_queue(&state) {
        state.is_running.store(false, Ordering::SeqCst);
        send_status(&tx, &log_buf, "[DONE]".to_string());
    }
}

fn spawn_run_body(state: &AppState, req: RunRequest, queue_id: Option<String>) {
    // A previous `stop` must never poison the next run: the cancel flag is
    // per-run, cleared at dispatch. (Stale global flag once ate 15 queued
    // cells with 0-trial Dones.)
    state.cancel_requested.store(false, Ordering::SeqCst);
    let state_clone = state.clone();
    let handle = tokio::spawn(execute_run_body(state_clone, req, queue_id));
    *state.task_handle.lock().unwrap() = Some(handle);
}

async fn execute_run_body(state: AppState, req: RunRequest, queue_id: Option<String>) {
    let tx = state.log_sender.clone();
    let run_log_buf = state.log_buffer.clone();
    let mut total_trials = req.trials.max(1);
    let mut passing_trials = 0;
    let mut subrun_results = Vec::new();
    let run_timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let parent_run_id = format!(
        "{}_{}_{}",
        req.task,
        req.model.replace(['/', ':'], "_"),
        run_timestamp
    );

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
            "dns" => TaskType::Dns,
            "pdf" => TaskType::Pdf,
            "wal" => TaskType::Wal,
            "proxy" => TaskType::Proxy,
            "pubsub" => TaskType::Pubsub,
        "binproto" => TaskType::Binproto,
            _ => TaskType::Redis,
        };
        match crate::bench::batch_ref::BatchRef::start(&task_type_pre, &parent_run_id, &batch_log) {
            Ok(b) => b,
            Err(e) => {
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
        if state.cancel_requested.load(Ordering::SeqCst) {
            send_status(&tx, &run_log_buf, "[STOP] Run cancelled by user before next trial.".to_string());
            break;
        }

        let started_at = Utc::now().to_rfc3339();
        let run_id = if total_trials > 1 {
            format!("{}_trial{}", parent_run_id, trial_idx)
        } else {
            parent_run_id.clone()
        };

        let effort_setting = req.effort.clone().unwrap_or_else(|| "high".to_string());

        *state.current_run.write().unwrap() = Some(ActiveRunInfo {
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
        });
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
            "dns" => TaskType::Dns,
            "pdf" => TaskType::Pdf,
            "wal" => TaskType::Wal,
            "proxy" => TaskType::Proxy,
            "pubsub" => TaskType::Pubsub,
        "binproto" => TaskType::Binproto,
            _ => TaskType::Redis,
        };

        let repo_root = crate::config::get_repo_root();
        let work_path = repo_root.join("workspace");

        let pipe_config = BenchmarkConfig {
            model: req.model.clone(),
            task: task_type,
            effort: effort_setting.clone(),
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
                logger.log(&format!(
                    "[SUBRUN ERROR] Subrun {}/{} failed: {}",
                    trial_idx, total_trials, e
                ));
                break;
            }
        }
    }

    let batch_drift = batch_ref.map(|b| b.finish(&batch_log).drift);

    let mut merged_opt = if total_trials > 1 && !subrun_results.is_empty() {
        LeaderboardManager::consolidate_multi_trial_run(
            &parent_run_id,
            &subrun_results,
            req.save_results.unwrap_or(true),
            batch_drift,
        )
    } else {
        None
    };

    if req.extend_if_unstable && total_trials >= 2 {
        let max_total = req.max_trials.max(total_trials);
        let effort_setting = req.effort.clone().unwrap_or_else(|| "high".to_string());
        let task_type_ext = match req.task.as_str() {
            "redis" => TaskType::Redis,
            "http" => TaskType::Http,
            "dns" => TaskType::Dns,
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
                send_status(&tx, &run_log_buf, format!("[EXTEND] stable: {}; done.", reason));
                break;
            }
            let add = (max_total - total_trials).min(3);
            send_status(
                &tx,
                &run_log_buf,
                format!("[EXTEND] unstable ({}); running {} more trial(s)...", reason, add),
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
            )
            .await;
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
            merged_opt = LeaderboardManager::consolidate_multi_trial_run(
                &parent_run_id,
                &subrun_results,
                req.save_results.unwrap_or(true),
                out.drift,
            );
        }
    }

    if subrun_results.is_empty() {
        send_status(
            &tx,
            &run_log_buf,
            "[STATUS] No trials completed (cancelled or all errored); nothing persisted.".to_string(),
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
        send_status(&tx, &run_log_buf, "[STATUS] All trials passed (100% pass rate).".to_string());
    } else if passing_trials > 0 {
        send_status(
            &tx,
            &run_log_buf,
            format!("[STATUS] Partial pass: {}/{} trials passed.", passing_trials, total_trials),
        );
    } else {
        send_status(
            &tx,
            &run_log_buf,
            "[STATUS] Benchmark completed with 0 passing trials (0.0% pass rate).".to_string(),
        );
    }

    if let Ok(mut cur_lock) = state.current_run.write() {
        *state.last_run.write().unwrap() = cur_lock.clone();
        *cur_lock = None;
    }
    *state.task_handle.lock().unwrap() = None;
    if let Some(qid) = queue_id {
        mark_queue_done(&state, &qid);
    }
    // Runner-slot handoff: only the slot holder spawns the next run.
    // is_running stays true across the handoff; stored false only when idle.
    if pump_queue(&state) {
        return;
    }
    state.is_running.store(false, Ordering::SeqCst);
    send_status(&tx, &run_log_buf, "[DONE]".to_string());
}

#[derive(Deserialize, Serialize)]
struct ExtendBody {
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
async fn extend_parent_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ExtendBody>,
) -> Result<Json<serde_json::Value>, Response> {
    if let Some(proxied) = proxy_mutation(&state, "POST", &format!("/api/runs/{id}/extend"), Some(serde_json::to_value(&body).unwrap_or_default())).await {
        let status = proxied.status();
        if status.is_success() {
            let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024).await.unwrap_or_default();
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
        "dns" => TaskType::Dns,
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
    };
    let state_clone = state.clone();
    let clean_for_task = clean.clone();
    state.cancel_requested.store(false, Ordering::SeqCst);
    let handle = tokio::spawn(async move {
        state_clone.cancel_requested.store(false, Ordering::SeqCst);
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
        )
        .await;
        if !out.results.is_empty() {
            let mut combined = data.subruns.clone();
            combined.extend(out.results);
            let passing = combined.iter().filter(|r| r.pass_rate >= 100.0).count();
            let merged = LeaderboardManager::consolidate_multi_trial_run(
                &clean_for_task,
                &combined,
                true,
                out.drift,
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
        *state_clone.task_handle.lock().unwrap() = None;
        if !pump_queue(&state_clone) {
            state_clone.is_running.store(false, Ordering::SeqCst);
            send_status(&tx, &ext_log_buf, "[DONE]".to_string());
        }
    });
    *state.task_handle.lock().unwrap() = Some(handle);
    Ok(Json(serde_json::json!({
        "started": clean,
    })))
}

async fn get_queue(State(state): State<AppState>) -> Response {
    if let Some(proxied) = proxy_read(&state, "/api/queue", "").await {
        return proxied;
    }
    Json::<Vec<QueuedRun>>(state.queue.lock().unwrap().iter().cloned().collect()).into_response()
}

/// Try to start the head of the queue. Returns true when a run was spawned
/// or the item is already picked up. Bounded retries cover the
/// pop-None/store-false handoff gap at the end of a run body.
async fn kick_runner(state: &AppState, want_id: &str) -> bool {
    for _ in 0..50 {
        let head = {
            let mut q = state.queue.lock().unwrap();
            match q.iter().find(|i| i.id == want_id).map(|i| i.status) {
                None => return true,
                Some(s) if s != QueueStatus::Pending => return true,
                _ => {}
            }
            if state.is_running.load(Ordering::SeqCst) {
                None
            } else if state
                .is_running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                None
            } else {
                let next = q
                    .iter_mut()
                    .find(|i| i.status == QueueStatus::Pending)
                    .map(|item| {
                        item.status = QueueStatus::Running;
                        (item.kind.clone(), item.id.clone())
                    });
                if next.is_none() {
                    state.is_running.store(false, Ordering::SeqCst);
                }
                next
            }
        };
        match head {
            Some((kind, id)) => {
                let now = now_rfc3339();
                db_try(
                    &format!("mark {id} Running"),
                    state.store.update_queue_status(&id, "Running", Some(&now), None),
                );
                match kind {
                    QueueKind::Run(req) => spawn_run_body(state, req, Some(id)),
                    QueueKind::Extend(ext) => spawn_extend_body(state, ext, id),
                }
                return true;
            }
            None => {}
        }
        if !state.is_running.load(Ordering::SeqCst) {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// Push a queue item, returning its id. Callers own kick/start decisions.
fn push_queue_item(state: &AppState, kind: QueueKind) -> String {
    let id = format!("q{}", state.queue_seq.fetch_add(1, Ordering::SeqCst) + 1);
    let evicted: Vec<String> = {
        let mut q = state.queue.lock().unwrap();
        q.push_back(QueuedRun {
            id: id.clone(),
            enqueued_at: Utc::now().to_rfc3339(),
            status: QueueStatus::Pending,
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
    if let Some(item) = state.queue.lock().unwrap().iter().find(|i| i.id == id).cloned() {
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

/// Forward a mutation to the engine in GUI-proxy mode.
/// Returns Some(response) when proxying applied; None in single-process mode.
async fn proxy_mutation(
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

async fn enqueue_run(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Result<Json<serde_json::Value>, Response> {
    if let Some(proxied) = proxy_mutation(&state, "POST", "/api/queue", Some(serde_json::to_value(&req).unwrap_or_default())).await {
        let status = proxied.status();
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            return Ok(Json(v));
        }
        return Err((status, text).into_response());
    }
    if !matches!(
        req.task.as_str(),
        "redis" | "http" | "dns" | "pdf" | "wal" | "proxy" | "pubsub" | "binproto"
    ) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "Unknown task '{}': expected redis, http, dns, pdf, wal, proxy, pubsub, or binproto",
                req.task
            ),
        )
            .into_response());
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

async fn remove_queued_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<String, Response> {
    if let Some(proxied) = proxy_mutation(&state, "DELETE", &format!("/api/queue/{id}"), None).await {
        let status = proxied.status();
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() { return Ok(text); }
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

async fn clear_queue(State(state): State<AppState>) -> String {
    if let Some(proxied) = proxy_mutation(&state, "DELETE", "/api/queue", None).await {
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024).await.unwrap_or_default();
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

async fn start_run(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Result<String, Response> {
    if let Some(proxied) = proxy_mutation(&state, "POST", "/api/run", Some(serde_json::to_value(&req).unwrap_or_default())).await {
        let status = proxied.status();
        let bytes = axum::body::to_bytes(proxied.into_body(), 1024 * 1024).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if status.is_success() { return Ok(text); }
        return Err((status, text).into_response());
    }
    if state
        .is_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err((
            axum::http::StatusCode::CONFLICT,
            "Benchmark is already running",
        )
            .into_response());
    }

    if !matches!(
        req.task.as_str(),
        "redis" | "http" | "dns" | "pdf" | "wal" | "proxy" | "pubsub" | "binproto"
    ) {
        state.is_running.store(false, Ordering::SeqCst);
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "Unknown task '{}': expected redis, http, dns, pdf, wal, proxy, pubsub, or binproto",
                req.task
            ),
        )
            .into_response());
    }

    state.cancel_requested.store(false, Ordering::SeqCst);
    spawn_run_body(&state, req, None);

    Ok("Benchmark started".to_string())
}

async fn stream_logs(
    State(state): State<AppState>,
) -> Sse<std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>>> {
    if !state.engine_url.is_empty() {
        // Proxy mode: one-shot snapshot of the engine console tail as SSE
        // events (the GUI also polls /api/console every 2s = the live path).
        let url = format!("{}/api/console?tail=200", state.engine_url.trim_end_matches('/'));
        let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default();
        let text = match client.get(&url).send().await {
            Ok(resp) => resp.text().await.unwrap_or_else(|_| "[STREAM NOTICE] engine console unreadable".to_string()),
            Err(_) => "[STREAM NOTICE] engine console down".to_string(),
        };
        let evts: Vec<Result<Event, Infallible>> = text.lines().map(|l| Ok(Event::default().data(l.to_string()))).collect();
        return Sse::new(Box::pin(tokio_stream::iter(evts)) as std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>>).keep_alive(KeepAlive::default());
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
    Sse::new(Box::pin(combined) as std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>>).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
pub struct UpdateOmpRequest {
    pub version: Option<String>,
}

async fn get_sandbox_omp_status() -> Json<crate::sandbox::OmpVersionStatus> {
    let status = crate::sandbox::omp_updater::check_status(false).await;
    Json(status)
}

async fn update_sandbox_omp(
    payload: Option<Json<UpdateOmpRequest>>,
) -> Result<Json<crate::sandbox::OmpVersionStatus>, (StatusCode, String)> {
    let target = payload.and_then(|p| p.0.version);
    match crate::sandbox::omp_updater::update_sandbox_image(target.as_deref()).await {
        Ok(status) => Ok(Json(status)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::RunTokenUsage;
    use tokio::net::TcpListener;

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
            timeout_min: None,
            trials: 1,
            extend_if_unstable: false,
            max_trials: 1,
            save_results: Some(false),
            review_model: None,
        };
        spawn_run_body(&state, req, Some("q-test-cancel".to_string()));
        assert!(!state.cancel_requested.load(Ordering::SeqCst));
    }

    async fn test_queue_of(state: &AppState) -> Vec<QueuedRun> {
        let resp = get_queue(State(state.clone())).await;
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn create_test_state() -> AppState {
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
            store: Arc::new(tempfile_mem_store()),
            db_fallback: false,
            engine_url: String::new(),
        }
    }

    #[tokio::test]
    async fn status_response_reports_db_fallback_flag() {
        // get_status surfaces AppState::db_fallback so the GUI can show a
        // degraded-durability banner; default (false) keeps old shape.
        let mut state = create_test_state();
        assert!(!state.db_fallback);
        let resp = get_status(State(state.clone())).await;
        {
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v.get("db_fallback"), Some(&serde_json::Value::Bool(false)));
        }
        state.db_fallback = true;
        let resp = get_status(State(state)).await;
        {
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
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
        store.enqueue_run(&crate::web::server::test_queue_item("q8")).expect("enqueue");
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
            task: "dns".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 5,
            eval_only: true,
            effort: Some("low".to_string()),
            timeout_min: Some(1),
            trials: 1,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
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
            })).unwrap(),
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
        assert!(remove_queued_run(State(state.clone()), AxumPath(q[0].id.clone()))
            .await
            .is_ok());
        let _ = std::fs::remove_file(real.join("qp_qtest_unstable.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_queue_remove_and_clear() {
        let state = create_test_state();
        state.is_running.store(true, Ordering::SeqCst);
        let a = enqueue_run(State(state.clone()), Json(queue_test_req()))
            .await
            .unwrap()
            .0;
        let b = enqueue_run(State(state.clone()), Json(queue_test_req()))
            .await
            .unwrap()
            .0;
        let ida = a["id"].as_str().unwrap().to_string();
        let idb = b["id"].as_str().unwrap().to_string();
        assert!(
            remove_queued_run(State(state.clone()), AxumPath(ida.clone()))
                .await
                .is_ok()
        );
        // Removing twice -> not found.
        assert!(remove_queued_run(State(state.clone()), AxumPath(ida))
            .await
            .is_err());
        // Running items cannot be removed.
        {
            let mut q = state.queue.lock().unwrap();
            q.iter_mut().find(|i| i.id == idb).unwrap().status = QueueStatus::Running;
        }
        assert!(remove_queued_run(State(state.clone()), AxumPath(idb))
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
    }

    #[tokio::test]
    async fn test_log_buffer_mirrors_lifecycle_tail() {
        // send_status mirrors every lifecycle line into the console buffer
        // so late SSE joiners and /api/console readers see DONE/STOP tails.
        let (tx, _) = broadcast::channel(16);
        let buf = Arc::new(RwLock::new(Vec::new()));
        send_status(&tx, &buf, "[DONE]".to_string());
        send_status(&tx, &buf, "[STATUS] All trials passed (100% pass rate).".to_string());
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

    #[tokio::test]
    async fn test_run_delete_guards() {
        let state = create_test_state();
        let app = UiServer::build_app(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
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

    #[tokio::test]
    async fn test_web_routes_lifecycle() {
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

        let client = reqwest::Client::new();
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

        // 5. POST /api/prompt/test_task
        let res = client
            .post(format!("{}/api/prompt/test_task", base))
            .body("# Test Task")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), "Prompt saved successfully");

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
        state.is_running.store(true, Ordering::SeqCst);
        let run_req = RunRequest {
            model: "test_model".to_string(),
            task: "redis".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 5,
            eval_only: true,
            effort: Some("low".to_string()),
            timeout_min: Some(1),
            trials: 1,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
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
            model: "google/gemini-2.5-flash".to_string(),
            task: "redis".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 1,
            eval_only: true,
            effort: Some("low".to_string()),
            timeout_min: Some(1),
            trials: 2,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
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
            run_id: run_a_id.to_string(),
            model: "model_a".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Rust".to_string(),
            effort: Some("low".to_string()),
            started_at: "2026-09-04T12:00:00Z".to_string(),
            completed_at: "2026-09-04T12:01:00Z".to_string(),
            duration_seconds: 60.0,
            turns: Some(3),
            pass_rate: 75.0,
            passed_stages: 3,
            total_stages: 4,
            stages: Vec::new(),
            throughput_req_sec: Some(40000.0),
            reference_throughput_req_sec: Some(71000.0),
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            tokens: RunTokenUsage {
                prompt_tokens: 1000,
                cached_tokens: 500,
                completion_tokens: 200,
                total_tokens: 1200,
            },
            cost_usd: 0.05,
            effective_cost_usd: Some(0.055),
            savings_percent: 25.0,
            efficiency_score: 15.0,
            throughput_score: Some(31.9),
            files: Vec::new(),
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
            rules_version: None,
        };
        let manifest_b = RunManifest {
            run_id: run_b_id.to_string(),
            model: "model_b".to_string(),
            task: "redis".to_string(),
            status: "completed".to_string(),
            language: "Rust".to_string(),
            effort: Some("high".to_string()),
            started_at: "2026-09-04T12:05:00Z".to_string(),
            completed_at: "2026-09-04T12:06:00Z".to_string(),
            duration_seconds: 50.0,
            turns: Some(4),
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            stages: Vec::new(),
            throughput_req_sec: Some(55000.0),
            reference_throughput_req_sec: Some(71000.0),
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            tokens: RunTokenUsage {
                prompt_tokens: 1500,
                cached_tokens: 600,
                completion_tokens: 300,
                total_tokens: 1800,
            },
            cost_usd: 0.08,
            effective_cost_usd: Some(0.085),
            savings_percent: 20.0,
            efficiency_score: 12.5,
            throughput_score: Some(31.9),
            files: Vec::new(),
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
            rules_version: None,
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

        // 22. POST /api/run with runnable start.sh to test Docker candidate branch with DNS task
        let ws_dir = resolve_workspace_dir();
        let _ = fs::create_dir_all(&ws_dir);
        fs::write(ws_dir.join("start.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(ws_dir.join("start.sh"), fs::Permissions::from_mode(0o755));
        }

        let run_dns_req = RunRequest {
            model: "google/gemini-2.5-flash".to_string(),
            task: "dns".to_string(),
            api_key: None,
            budget_usd: 0.10,
            max_turns: 1,
            eval_only: true,
            effort: Some("low".to_string()),
            timeout_min: Some(1),
            trials: 1,
            save_results: Some(false),
            review_model: None,
            extend_if_unstable: false,
            max_trials: 9,
        };
        let res = client
            .post(format!("{}/api/run", base))
            .json(&run_dns_req)
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

        // Clean any gemini/test artifacts from runs/ and results/
        if let Ok(entries) = fs::read_dir(&runs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        let r_dir = resolve_results_dir();
        if let Ok(entries) = fs::read_dir(&r_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("gemini-2.5-flash") || name.starts_with("test_") {
                    let _ = fs::remove_file(entry.path());
                }
            }
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
}
