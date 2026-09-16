use crate::config::TaskType;
use crate::pipeline::{BenchmarkConfig, BenchmarkPipeline, PipelineLogger};
use crate::report::{
    EnvironmentInfo, FileInfo, LeaderboardManager, RunArchiver, RunManifest,
    is_hidden, set_hidden, PortalExporter,
};
use crate::sandbox::SandboxManager;
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Redirect, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
}

#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    pub is_running: bool,
    pub current_run: Option<ActiveRunInfo>,
    pub last_run: Option<ActiveRunInfo>,
}

async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let is_running = state.is_running.load(Ordering::SeqCst);
    let current_run = state.current_run.read().unwrap().clone();
    let last_run = state.last_run.read().unwrap().clone();
    Json(StatusResponse {
        is_running,
        current_run,
        last_run,
    })
}

fn default_trials() -> u32 {
    10
}

fn default_max_turns() -> u32 {
    50
}

fn default_budget() -> f64 {
    1.00
}

fn default_timeout_min() -> Option<u64> {
    Some(30)
}

#[derive(Debug, Serialize, Deserialize)]
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
            .route("/api/runs/:id/review", get(get_run_review))
            .route("/api/runs/:id", delete(delete_run))
            .route("/portal/*path", get(serve_portal))
            .route("/api/sandbox/omp", get(get_sandbox_omp_status))
            .route("/api/sandbox/omp/update", post(update_sandbox_omp))
            .route("/api/env", get(get_env))
            .with_state(state)
    }

    pub async fn start(host: &str, port: u16) -> anyhow::Result<()> {
        let repo = crate::config::get_repo_root();
        if repo.exists() {
            let _ = std::env::set_current_dir(&repo);
        }
        let (log_sender, _) = broadcast::channel(4096);
        let state = AppState {
            log_sender,
            is_running: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            current_run: Arc::new(RwLock::new(None)),
            last_run: Arc::new(RwLock::new(None)),
            log_buffer: Arc::new(RwLock::new(Vec::new())),
            task_handle: Arc::new(std::sync::Mutex::new(None)),
        };

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
                    .map(|params| params.iter().any(|p| p == "reasoning" || p == "reasoning_effort"))
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

                let reasoning_mandatory = reasoning_obj
                    .and_then(|r| r.mandatory)
                    .unwrap_or(false);

                let is_reasoning = has_reasoning_param
                    || !supported_efforts.is_empty()
                    || supports_max_tokens
                    || reasoning_mandatory
                    || reasoning_obj.and_then(|r| r.default_enabled).unwrap_or(false);

                if prompt_p <= 1.0 && comp_p <= 5.0 && (prompt_p > 0.0 || comp_p > 0.0) {
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
        a.prompt_price_per_m
            .partial_cmp(&b.prompt_price_per_m)
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
    Json(list.into_iter().map(|r| {
        let mut v = serde_json::to_value(&r).unwrap_or(serde_json::Value::Null);
        v["portal_hidden"] = serde_json::Value::Bool(is_hidden(&runs_dir, &r.id));
        v
    }).collect())
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

fn clean_run_id(run_id: &str) -> Option<String> {
    let name = Path::new(run_id).file_name()?.to_str()?;
    if name.is_empty() || name == "." || name != run_id {
        return None;
    }
    Some(name.to_string())
}

fn manifest_with_portal_flag(m: &RunManifest) -> serde_json::Value {
    let mut v = serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
    v["portal_hidden"] = serde_json::Value::Bool(is_hidden(&RunArchiver::resolve_runs_dir(), &m.run_id));
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
async fn delete_run(AxumPath(run_id): AxumPath<String>) -> Response {
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

async fn get_runs() -> Json<Vec<serde_json::Value>> {
    let r_dir = RunArchiver::resolve_runs_dir();
    Json(RunArchiver::list_runs(&r_dir).iter().map(manifest_with_portal_flag).collect())
}

async fn get_run_detail(AxumPath(run_id): AxumPath<String>) -> Result<Json<serde_json::Value>, Response> {
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

async fn get_active_console(State(state): State<AppState>) -> Response {
    let buf = state.log_buffer.read().unwrap();
    buf.join("\n").into_response()
}



async fn get_env() -> Json<EnvironmentInfo> {
    Json(EnvironmentInfo::detect())
}

async fn stop_run(State(state): State<AppState>) -> Response {
    if !state.is_running.load(Ordering::SeqCst) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "No benchmark is currently running",
        )
            .into_response();
    }

    state.cancel_requested.store(true, Ordering::SeqCst);
    let tx = state.log_sender.clone();
    let _ = tx.send(
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
    state.is_running.store(false, Ordering::SeqCst);
    let _ = tx.send("[DONE] Benchmark run was stopped by user.".to_string());

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

async fn start_run(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Result<String, Response> {
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
        "redis" | "http" | "dns" | "pdf"
    ) {
        state.is_running.store(false, Ordering::SeqCst);
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!("Unknown task '{}': expected redis, http, dns, or pdf", req.task),
        )
            .into_response());
    }

    state.cancel_requested.store(false, Ordering::SeqCst);
    let tx = state.log_sender.clone();
    let state_clone = state.clone();

    let handle = tokio::spawn(async move {
        let total_trials = req.trials.max(1);
        let mut passing_trials = 0;
        let mut subrun_results = Vec::new();
        let run_timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let parent_run_id = format!(
            "{}_{}_{}",
            req.task,
            req.model.replace(['/', ':'], "_"),
            run_timestamp
        );

        let batch_log = |m: &str| {
            let _ = tx.send(m.to_string());
        };
        // Batch reference: one shared denominator for all trials.
        // Fallback is live per-trial measurement inside the pipeline.
        let batch_ref = if total_trials > 1 {
            let task_type_pre = match req.task.as_str() {
                "redis" => TaskType::Redis,
                "http" => TaskType::Http,
                "dns" => TaskType::Dns,
                "pdf" => TaskType::Pdf,
                _ => TaskType::Redis,
            };
            match crate::bench::batch_ref::BatchRef::start(
                &task_type_pre,
                &parent_run_id,
                &batch_log,
            ) {
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
            if state_clone.cancel_requested.load(Ordering::SeqCst) {
                let _ = tx.send("[STOP] Run cancelled by user before next trial.".to_string());
                break;
            }

            let started_at = Utc::now().to_rfc3339();
            let run_id = if total_trials > 1 {
                format!("{}_trial{}", parent_run_id, trial_idx)
            } else {
                parent_run_id.clone()
            };

            let effort_setting = req.effort.clone().unwrap_or_else(|| "high".to_string());

            *state_clone.current_run.write().unwrap() = Some(ActiveRunInfo {
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
            });
            if trial_idx == 1 {
                state_clone.log_buffer.write().unwrap().clear();
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
                            line_to_log = format!("[LIVE] Subrun {}/{} | {}", self.trial_idx, self.total_trials, &clean[7..]);
                        } else if clean.starts_with("[TELEMETRY] ") && !clean.contains("subrun=") {
                            line_to_log = format!("{} subrun={} total_subruns={}", clean, self.trial_idx, self.total_trials);
                        }

                        let formatted = format!("[{}] {}", ts, line_to_log);
                        self.log_buf.write().unwrap().push(formatted.clone());
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
                                    if let Some(t) = turns { info.turns = t; }
                                    if let Some(mt) = max_turns { info.max_turns = mt; }
                                    if let Some(s) = spent { info.cost_usd = s; }
                                    if let Some(tok) = tokens { info.tokens = tok; }
                                    info.subrun = subrun.or(Some(self.trial_idx));
                                    info.total_subruns = total_subruns.or(Some(self.total_trials));
                                }
                            }
                        }
                    }
                }
            }

            let logger = Arc::new(WebLogger {
                tx: tx.clone(),
                log_buf: state_clone.log_buffer.clone(),
                current_run: state_clone.current_run.clone(),
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
                Some(state_clone.cancel_requested.clone()),
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
                    if state_clone.cancel_requested.load(Ordering::SeqCst) {
                        logger.log("[PIPELINE] Run cancelled by user.");
                        break;
                    }
                    logger.log(&format!("[SUBRUN ERROR] Subrun {}/{} failed: {}", trial_idx, total_trials, e));
                    break;
                }
            }
        }

        let batch_drift = batch_ref.map(|b| b.finish(&batch_log).drift);

        if total_trials > 1 && !subrun_results.is_empty() {
            let _ = LeaderboardManager::consolidate_multi_trial_run(
                &parent_run_id,
                &subrun_results,
                req.save_results.unwrap_or(true),
                batch_drift,
            );
        }



        if total_trials > 1 {
            let pass_at_1 = compute_pass_at_k(total_trials as usize, passing_trials, 1) * 100.0;
            let pass_at_k =
                compute_pass_at_k(total_trials as usize, passing_trials, total_trials as usize)
                    * 100.0;
            let _ = tx.send(format!(
                "[MULTI-TRIAL] Trials: {}, Passing: {}, Pass@1: {:.1}%, Pass@{}: {:.1}%",
                total_trials, passing_trials, pass_at_1, total_trials, pass_at_k
            ));
        }

        if passing_trials == total_trials as usize && total_trials > 0 {
            let _ = tx.send("[STATUS] All trials passed (100% pass rate).".to_string());
        } else if passing_trials > 0 {
            let _ = tx.send(format!(
                "[STATUS] Partial pass: {}/{} trials passed.",
                passing_trials, total_trials
            ));
        } else {
            let _ = tx.send(
                "[STATUS] Benchmark completed with 0 passing trials (0.0% pass rate).".to_string(),
            );
        }

        if let Ok(mut cur_lock) = state_clone.current_run.write() {
            *state_clone.last_run.write().unwrap() = cur_lock.clone();
            *cur_lock = None;
        }
        *state_clone.task_handle.lock().unwrap() = None;
        state_clone.is_running.store(false, Ordering::SeqCst);
        let _ = tx.send("[DONE]".to_string());
    });
    *state.task_handle.lock().unwrap() = Some(handle);

    Ok("Benchmark started".to_string())
}

async fn stream_logs(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
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
        Err(_) => {
            Some(Ok(Event::default().data("[STREAM NOTICE] Event burst: log stream caught up (full log preserved in console)")))
        }
    });

    let combined = initial_stream.chain(live_stream);
    Sse::new(combined).keep_alive(KeepAlive::default())
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
        }
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
        };
        let res = client
            .post(format!("{}/api/run", base))
            .json(&eval_req)
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
        let status_res = client
            .get(format!("{}/api/status", base))
            .send()
            .await
            .unwrap();
        assert_eq!(status_res.status(), 200);
        let status_body: StatusResponse = status_res.json().await.unwrap();
        assert_eq!(status_body.last_run.as_ref().and_then(|r| r.total_subruns), Some(2));

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
            is_published: None,
            method_version: Some("0.1.0".to_string()),
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
            is_published: None,
            method_version: Some("0.1.0".to_string()),
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

        let parsed: OpenRouterResponse = serde_json::from_str(sample_json).expect("deserialize sample");
        assert_eq!(parsed.data.len(), 3);

        let gemini = &parsed.data[0];
        assert_eq!(gemini.id, "google/gemini-3.8-flash");
        let r = gemini.reasoning.as_ref().unwrap();
        assert_eq!(r.supported_efforts.as_ref().unwrap(), &vec!["high", "medium", "low"]);
        assert_eq!(r.default_effort.as_deref(), Some("medium"));

        let meta = &parsed.data[1];
        let mut meta_efforts = meta.reasoning.as_ref().unwrap().supported_efforts.clone().unwrap();
        if meta.id.starts_with("meta/") {
            meta_efforts.retain(|e| e != "max");
        }
        assert_eq!(meta_efforts, vec!["high", "medium", "low"]);

        let qwen = &parsed.data[2];
        assert_eq!(qwen.reasoning.as_ref().unwrap().supports_max_tokens, Some(true));
    }
}
