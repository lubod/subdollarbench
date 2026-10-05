//! Portal static files served by the web UI.
//!
//! Pure move from `server.rs` (2026-10-05 split): zero behavior change.

use axum::{
    extract::Path as AxumPath,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use std::path::{Path, PathBuf};
pub fn resolve_portal_dir() -> PathBuf {
    crate::config::get_repo_root().join("portal")
}

pub fn portal_content_type(path: &Path, bytes: &[u8]) -> &'static str {
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

pub async fn serve_portal_index() -> Response {
    serve_portal(AxumPath(String::new())).await
}

pub async fn serve_portal(AxumPath(sub): AxumPath<String>) -> Response {
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
