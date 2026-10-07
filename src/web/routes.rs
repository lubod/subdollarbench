//! Route-table construction for the web UI.
//!
//! Pure move from `server.rs` (2026-10-05 split): zero behavior change.
//! Handler + guard fns stay in `server.rs`; this module only wires them.

use super::portal_proxy::{serve_portal, serve_portal_index};
use super::server::{
    balance_check_handler, clear_queue, compare_runs, csrf_guard, delete_run, enqueue_run,
    extend_parent_run, get_active_console, get_env, get_leaderboard, get_models, get_prompt,
    get_queue, get_run_detail, get_run_file, get_run_files, get_run_log, get_run_review, get_runs,
    get_sandbox_omp_status, get_sandbox_opencode_status, get_status, login, logout, me,
    pause_queue_handler, rebuild_portal, remove_queued_run, resume_queue_handler, save_prompt,
    serve_index, serve_shared_js, set_run_poison, set_run_portal_visibility, start_run, stop_run,
    stream_logs, update_sandbox_omp, AppState,
};
use axum::{
    response::Redirect,
    routing::{delete, get, post},
    Router,
};

/// Build the full web router. Called by [`super::server::UiServer::build_app`].
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
        .route("/api/queue/pause", post(pause_queue_handler))
        .route("/api/queue/resume", post(resume_queue_handler))
        .route("/api/balance/check", get(balance_check_handler))
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
        .route("/api/runs/:id/poison", post(set_run_poison))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/runs/:id/extend", post(extend_parent_run))
        .route("/api/runs/:id/review", get(get_run_review))
        .route("/api/runs/:id", delete(delete_run))
        .route("/portal/*path", get(serve_portal))
        .route("/static/shared.js", get(serve_shared_js))
        .route("/api/sandbox/omp", get(get_sandbox_omp_status))
        .route("/api/sandbox/opencode", get(get_sandbox_opencode_status))
        .route("/api/sandbox/omp/update", post(update_sandbox_omp))
        .route("/api/env", get(get_env))
        .route_layer(axum::middleware::from_fn(csrf_guard))
        .with_state(state)
}
