//! Thin HTTP client for a running `ui` engine.
//!
//! The CLI historically drove benchmarks foreground against `./runs`,
//! bypassing the web GUI's in-memory queue. These helpers let CLI commands
//! talk to the live engine instead (status, queue, enqueue, stop, run
//! inspection). Only the `async` fns perform I/O, and only on explicit user
//! commands. URL/payload builders and all formatters are pure and unit-tested.

use anyhow::{Context, Result};

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:3000";

/// Resolve `--server` (or `SDB_SERVER`) to a base URL without trailing slash.
pub fn resolve_server(arg: Option<&str>) -> String {
    let raw = match arg {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => std::env::var("SDB_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string()),
    };
    raw.trim_end_matches('/').to_string()
}

/// Join a base server URL and an `/api/...` path.
pub fn endpoint(server: &str, path: &str) -> String {
    format!("{}{}", resolve_server(Some(server)), path)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client builds")
}

/// Bearer for mutating engine calls, from `SDB_UI_TOKEN`. Same source the
/// server's `csrf_guard` layer checks, so CLI and curl agree. Empty when
/// unset (tokenless local dev stays working).
pub fn ui_token() -> Option<String> {
    std::env::var("SDB_UI_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
}

fn auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match ui_token() {
        Some(t) => req.header("Authorization", format!("Bearer {}", t.trim())),
        None => req,
    }
}

fn check(url: &str, status: reqwest::StatusCode, body: String) -> Result<String> {
    if status.is_success() {
        Ok(body)
    } else {
        anyhow::bail!("{url} -> {status}: {body}")
    }
}

async fn get_text_raw(url: String) -> Result<(reqwest::header::HeaderMap, String)> {
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.text().await.unwrap_or_default();
    let body = check(&url, status, body)?;
    Ok((headers, body))
}

fn parse_json(body: String) -> serde_json::Value {
    serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body))
}

/// GET a JSON endpoint (`/api/status`, `/api/queue`, ...).
pub async fn get_json(server: &str, path: &str) -> Result<serde_json::Value> {
    let url = endpoint(server, path);
    let (_, body) = get_text_raw(url).await?;
    Ok(parse_json(body))
}

/// GET a text endpoint (`/api/runs/:id/log`, `/api/prompt/:task`, ...).
pub async fn get_text(server: &str, path: &str) -> Result<String> {
    let url = endpoint(server, path);
    let (_, body) = get_text_raw(url).await?;
    Ok(body)
}

/// POST a JSON body, parse the JSON reply.
pub async fn post_json(
    server: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let url = endpoint(server, path);
    let resp = auth(client().post(&url))
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let text = check(&url, status, text)?;
    Ok(parse_json(text))
}

/// POST with no body; the stop/rebuild endpoints answer plain text or JSON.
pub async fn post_empty(server: &str, path: &str) -> Result<String> {
    let url = endpoint(server, path);
    let resp = auth(client().post(&url))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    check(&url, status, body)
}

/// POST a raw text body (prompt save); answers plain text.
pub async fn post_text(server: &str, path: &str, body: String) -> Result<String> {
    let url = endpoint(server, path);
    let resp = auth(client().post(&url))
        .header("content-type", "text/plain")
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    check(&url, status, text)
}

/// DELETE an endpoint; answers plain text (`Removed q4 from queue`) or JSON.
pub async fn delete_text(server: &str, path: &str) -> Result<String> {
    let url = endpoint(server, path);
    let resp = auth(client().delete(&url))
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    check(&url, status, body)
}

/// Live console tail. Returns `(total_pushed_lines, new_body)`: pass the
/// previous total back as `since` to poll incrementally.
pub async fn get_console(
    server: &str,
    tail: usize,
    since: Option<usize>,
) -> Result<(usize, String)> {
    let mut url = format!("{}?tail={}", endpoint(server, "/api/console"), tail);
    if let Some(s) = since {
        url.push_str(&format!("&since={s}"));
    }
    let (headers, body) = get_text_raw(url).await?;
    let total = headers
        .get("X-Console-Total")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    Ok((total, body))
}

/// One workspace file via the server (handles `?file=` encoding).
pub async fn get_file_text(server: &str, run_id: &str, file: &str) -> Result<String> {
    let base = endpoint(server, &format!("/api/runs/{run_id}/file"));
    let url = format!("{base}?file={}", url_encode(file));
    let (_, body) = get_text_raw(url).await?;
    Ok(body)
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build a `/api/queue` run payload from CLI run flags.
#[allow(clippy::too_many_arguments)]
pub fn run_request(
    model: &str,
    task: &str,
    effort: &str,
    max_turns: u32,
    budget_usd: f64,
    timeout_min: u64,
    api_key: Option<&str>,
    eval_only: bool,
    trials: u32,
    extend_if_unstable: bool,
    max_trials: u32,
    no_save: bool,
    review_model: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "task": task,
        "effort": effort,
        "max_turns": max_turns,
        "budget_usd": budget_usd,
        "timeout_min": timeout_min,
        "api_key": api_key,
        "eval_only": eval_only,
        "trials": trials.max(1),
        "extend_if_unstable": extend_if_unstable,
        "max_trials": max_trials,
        "save_results": !no_save,
        "review_model": review_model,
    })
}

/// Build a `/api/runs/:id/extend` body. Always queues behind the live job:
/// a foreground extend would fight the engine over docker and task ports.
pub fn extend_body(
    trials: u32,
    max_turns: Option<u32>,
    budget_usd: Option<f64>,
    timeout_min: Option<u64>,
    eval_only: Option<bool>,
) -> serde_json::Value {
    let mut v = serde_json::json!({ "trials": trials.max(1), "queue": true });
    if let Some(n) = max_turns {
        v["max_turns"] = n.into();
    }
    if let Some(b) = budget_usd {
        v["budget_usd"] = b.into();
    }
    if let Some(t) = timeout_min {
        v["timeout_min"] = t.into();
    }
    if let Some(e) = eval_only {
        v["eval_only"] = e.into();
    }
    v
}

// --- pure formatters -------------------------------------------------------

fn js<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|s| s.as_str()).unwrap_or("-")
}

fn ju(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|n| n.as_u64()).unwrap_or(0)
}

fn jf(v: &serde_json::Value, key: &str) -> f64 {
    v.get(key).and_then(|n| n.as_f64()).unwrap_or(0.0)
}

fn short_model(model: &str) -> &str {
    if model == "-" {
        "-"
    } else {
        model.rsplit('/').next().unwrap_or(model)
    }
}

/// One-line human summary of a queue item's `kind` (`Run` or `Extend`).
pub fn queue_summary(kind: &serde_json::Value) -> String {
    if let Some(r) = kind.get("Run") {
        let effort = r.get("effort").and_then(|e| e.as_str()).unwrap_or("-");
        format!(
            "run {} {} {} x{}",
            short_model(js(r, "model")),
            js(r, "task"),
            effort,
            r.get("trials").and_then(|n| n.as_u64()).unwrap_or(0)
        )
    } else if let Some(e) = kind.get("Extend") {
        format!(
            "extend {} +{}",
            js(e, "parent_run_id"),
            e.get("trials").and_then(|n| n.as_u64()).unwrap_or(0)
        )
    } else {
        kind.to_string()
    }
}

fn fmt_run_info(prefix: &str, c: &serde_json::Value) -> String {
    let trial = match (
        c.get("subrun").and_then(|n| n.as_u64()),
        c.get("total_subruns").and_then(|n| n.as_u64()),
    ) {
        (Some(s), Some(t)) => format!(" trial {s}/{t}"),
        _ => String::new(),
    };
    format!(
        "{prefix}: {} [{} {} {}]{trial} turn {}/{} cost ${:.4} tokens {}\n",
        js(c, "run_id"),
        short_model(js(c, "model")),
        js(c, "task"),
        c.get("effort").and_then(|e| e.as_str()).unwrap_or("-"),
        ju(c, "turns"),
        ju(c, "max_turns"),
        jf(c, "cost_usd"),
        ju(c, "tokens"),
    )
}

/// Human rendering of `GET /api/status`.
pub fn format_status(v: &serde_json::Value) -> String {
    let mut out = String::new();
    let running = v
        .get("is_running")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    out.push_str(&format!(
        "engine: {}\n",
        if running { "RUNNING" } else { "idle" }
    ));
    match v.get("current_run") {
        Some(c) if !c.is_null() => out.push_str(&fmt_run_info("current", c)),
        _ => out.push_str("current: -\n"),
    }
    match v.get("last_run") {
        Some(c) if !c.is_null() => out.push_str(&fmt_run_info("last", c)),
        _ => out.push_str("last: -\n"),
    }
    out
}

/// Human rendering of `GET /api/queue`.
pub fn format_queue(v: &serde_json::Value) -> String {
    let items = v.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "queue: empty\n".to_string();
    }
    let mut out = format!(
        "{:<5} {:<9} {:<20} {}\n",
        "id", "status", "enqueued", "what"
    );
    for q in &items {
        out.push_str(&format!(
            "{:<5} {:<9} {:<20} {}\n",
            js(q, "id"),
            js(q, "status"),
            js(q, "enqueued_at").get(..19).unwrap_or("-"),
            q.get("kind").map(queue_summary).unwrap_or_default(),
        ));
    }
    out
}

/// Substring filters for the merged runs view. `None` disables a filter;
/// `status` matches exactly (case-insensitive), the rest are substrings.
pub fn match_manifest(
    m: &serde_json::Value,
    model: Option<&str>,
    task: Option<&str>,
    effort: Option<&str>,
    status: Option<&str>,
) -> bool {
    let contains = |key: &str, pat: &str| {
        m.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase().contains(&pat.to_lowercase()))
            .unwrap_or(false)
    };
    if let Some(p) = model {
        if !contains("model", p) {
            return false;
        }
    }
    if let Some(p) = task {
        if !contains("task", p) {
            return false;
        }
    }
    if let Some(p) = effort {
        if !contains("effort", p) {
            return false;
        }
    }
    if let Some(p) = status {
        let s = m.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !s.eq_ignore_ascii_case(p) {
            return false;
        }
    }
    true
}

/// Merged engine view: live queue head plus archived manifests newest-first.
#[allow(clippy::too_many_arguments)]
pub fn format_ps(
    queue: &serde_json::Value,
    runs: &serde_json::Value,
    model: Option<&str>,
    task: Option<&str>,
    effort: Option<&str>,
    status: Option<&str>,
    limit: usize,
) -> String {
    let mut out = String::new();
    let items = queue.as_array().cloned().unwrap_or_default();
    let live: Vec<_> = items
        .iter()
        .filter(|q| matches!(js(q, "status"), "Running" | "Pending"))
        .collect();
    let done = items.len() - live.len();
    out.push_str(&format!(
        "queue: {} live ({} finished history)\n",
        live.len(),
        done
    ));
    for q in &live {
        out.push_str(&format!(
            "  {:<5} {:<9} {}\n",
            js(q, "id"),
            js(q, "status"),
            q.get("kind").map(queue_summary).unwrap_or_default(),
        ));
    }
    let mut manifests: Vec<&serde_json::Value> = runs
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| match_manifest(m, model, task, effort, status))
                .collect()
        })
        .unwrap_or_default();
    manifests.sort_by(|a, b| js(b, "started_at").cmp(js(a, "started_at")));
    out.push_str(&format!(
        "{:<64} {:<22} {:<7} {:<8} {:<16} {:>6}\n",
        "run", "model", "effort", "task", "status", "pass%"
    ));
    for m in manifests.iter().take(limit.max(1)) {
        let pass = m.get("pass_rate").and_then(|n| n.as_f64()).unwrap_or(0.0);
        out.push_str(&format!(
            "{:<64} {:<22} {:<7} {:<8} {:<16} {:>5.1}%\n",
            js(m, "run_id"),
            short_model(js(m, "model"))
                .get(..22.min(short_model(js(m, "model")).len()))
                .unwrap_or("-"),
            m.get("effort").and_then(|e| e.as_str()).unwrap_or("-"),
            js(m, "task"),
            js(m, "status"),
            pass,
        ));
    }
    out
}

/// One filtered/sorted row of the model catalog (pure; mirrors the GUI tab).
fn model_row(m: &serde_json::Value) -> String {
    let efforts = m
        .get("supported_efforts")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();
    let brain = if m
        .get("is_reasoning")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
    {
        if efforts.is_empty() {
            if m.get("supports_max_tokens")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                " [tokens]"
            } else {
                " [reasoning]"
            }
        } else {
            &format!(" [{efforts}]")
        }
    } else {
        ""
    };
    let dflt = m
        .get("default_effort")
        .and_then(|e| e.as_str())
        .unwrap_or("-");
    let created = m.get("created").and_then(|n| n.as_i64()).unwrap_or(0);
    let date = if created > 0 {
        chrono::DateTime::from_timestamp(created, 0)
            .map(|d| d.format("%F").to_string())
            .unwrap_or_else(|| "-".into())
    } else {
        "-".into()
    };
    format!(
        "{:<42} {:>9.4} {:>9.4} {:>10} {:<7} {:<10} {}{}",
        js(m, "id"),
        jf(m, "prompt_price_per_m"),
        jf(m, "completion_price_per_m"),
        m.get("context_length")
            .and_then(|n| n.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into()),
        date,
        if efforts.is_empty() { "-" } else { &efforts },
        dflt,
        brain,
    )
}

/// Family predicate mirroring the GUI tab (`claude` matches the vendor too).
fn match_family(m: &serde_json::Value, family: &str) -> bool {
    let id = js(m, "id").to_lowercase();
    let name = js(m, "name").to_lowercase();
    let f = family.to_lowercase();
    if f == "all" || f.is_empty() {
        return true;
    }
    if f == "claude" {
        return id.contains("claude") || id.contains("anthropic") || name.contains("claude");
    }
    if f == "glm" {
        return id.contains("glm")
            || id.contains("zhipu")
            || id.contains("thudm")
            || name.contains("glm");
    }
    id.contains(&f) || name.contains(&f)
}

/// Filtered + sorted rendering of `GET /api/models` (mirrors the GUI tab).
/// `sort`: cheapest (default) | expensive | newest | name. Unknown -> cheapest.
pub fn format_models_filtered(
    v: &serde_json::Value,
    family: Option<&str>,
    search: Option<&str>,
    sort: &str,
    limit: usize,
) -> String {
    let mut items = v.as_array().cloned().unwrap_or_default();
    if let Some(f) = family {
        items.retain(|m| match_family(m, f));
    }
    if let Some(q) = search {
        let q = q.to_lowercase();
        items.retain(|m| {
            js(m, "id").to_lowercase().contains(&q) || js(m, "name").to_lowercase().contains(&q)
        });
    }
    match sort {
        "newest" => items.sort_by(|a, b| {
            b.get("created")
                .and_then(|n| n.as_i64())
                .unwrap_or(0)
                .cmp(&a.get("created").and_then(|n| n.as_i64()).unwrap_or(0))
        }),
        "expensive" => items.sort_by(|a, b| {
            jf(b, "prompt_price_per_m")
                .partial_cmp(&jf(a, "prompt_price_per_m"))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        "name" => items.sort_by(|a, b| js(a, "id").cmp(js(b, "id"))),
        _ => items.sort_by(|a, b| {
            jf(a, "prompt_price_per_m")
                .partial_cmp(&jf(b, "prompt_price_per_m"))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }
    let mut out = format!(
        "{} model(s) < $1.00/M\n{:<42} {:>9} {:>9} {:>10} {:<7} {:<10} {}\n",
        items.len(),
        "model",
        "in $/M",
        "out $/M",
        "context",
        "released",
        "efforts",
        "default",
    );
    for m in items.iter().take(limit.max(1)) {
        out.push_str(&model_row(m));
        out.push('\n');
    }
    out
}

/// Human rendering of `GET /api/models` (unfiltered; kept for back-compat).
pub fn format_models(v: &serde_json::Value) -> String {
    format_models_filtered(v, None, None, "cheapest", usize::MAX)
}

/// Human rendering of `GET /api/runs/:id` (manifest + portal flag).
pub fn format_detail(v: &serde_json::Value) -> String {
    let mut out = format!(
        "run:     {}\nmodel:   {}\ntask:    {}  effort: {}\nstatus:  {}\nstages:  {}/{} ({:.1}%)\n",
        js(v, "run_id"),
        js(v, "model"),
        js(v, "task"),
        v.get("effort").and_then(|e| e.as_str()).unwrap_or("-"),
        js(v, "status"),
        ju(v, "passed_stages"),
        ju(v, "total_stages"),
        v.get("pass_rate").and_then(|n| n.as_f64()).unwrap_or(0.0),
    );
    if let Some(t) = v.get("total_trials").and_then(|n| n.as_u64()) {
        if t > 1 {
            out.push_str(&format!("trials:  {t} merged\n"));
        }
    }
    if v.get("portal_hidden")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
    {
        out.push_str("portal:  hidden\n");
    }
    out.push_str(&format!(
        "turns:   {}  spent: {:.1}s  cost: ${:.4}\n",
        v.get("turns")
            .and_then(|n| n.as_u64())
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".into()),
        v.get("duration_seconds")
            .and_then(|n| n.as_f64())
            .unwrap_or(0.0),
        jf(v, "cost_usd"),
    ));
    out
}

/// Human rendering of `GET /api/compare?run_a=&run_b=`.
pub fn format_compare(v: &serde_json::Value) -> String {
    let mut out = String::new();
    if let (Some(a), Some(b)) = (v.get("run_a"), v.get("run_b")) {
        for (label, m) in [("a", a), ("b", b)] {
            out.push_str(&format!(
                "{label}: {} ({:.1}%)\n",
                js(m, "run_id"),
                m.get("pass_rate").and_then(|n| n.as_f64()).unwrap_or(0.0),
            ));
        }
    }
    if let Some(d) = v.get("metric_diff") {
        out.push_str(&format!(
            "delta: pass {:+.1}%  cost {:+.4}  tokens {:+}  duration {:+.1}s\n",
            d.get("pass_rate_delta")
                .and_then(|n| n.as_f64())
                .unwrap_or(0.0),
            d.get("cost_delta").and_then(|n| n.as_f64()).unwrap_or(0.0),
            d.get("tokens_delta").and_then(|n| n.as_i64()).unwrap_or(0),
            d.get("duration_delta")
                .and_then(|n| n.as_f64())
                .unwrap_or(0.0),
        ));
    }
    let files = v
        .get("file_diffs")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    out.push_str(&format!("files: {} diffed\n", files.len()));
    for f in files.iter().take(30) {
        out.push_str(&format!("  [{}] {}\n", js(f, "status"), js(f, "path")));
    }
    out
}

/// Keep the last `tail` lines; optionally only stage-result lines.
pub fn render_log(body: &str, tail: usize, stages_only: bool) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.len().saturating_sub(tail.max(1));
    lines[start..]
        .iter()
        .filter(|l| {
            !stages_only || (l.contains("Stage") || l.contains("PASS") || l.contains("FAIL"))
        })
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Process-wide guard serializing `SDB_SERVER` env access in tests.
/// `std::env` is process-global: parallel tests that set/remove
/// `SDB_SERVER` race each other, so every env-touching test must hold this.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the env lock AND snapshot `SDB_SERVER`; on drop the lock releases
/// and the previous value (or absence) is restored, even on panic.
/// Usage: `let _guard = crate::remote::keep_env();` as the first line.
#[cfg(test)]
pub(crate) fn keep_env() -> impl Drop {
    struct Guard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("SDB_SERVER", v),
                None => std::env::remove_var("SDB_SERVER"),
            }
        }
    }
    let lock = TEST_ENV_LOCK.lock().unwrap();
    let prev = std::env::var("SDB_SERVER").ok();
    Guard { _lock: lock, prev }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_server_default_and_env() {
        let _guard = keep_env();
        std::env::remove_var("SDB_SERVER");
        assert_eq!(resolve_server(None), DEFAULT_SERVER);
        assert_eq!(resolve_server(Some("http://x:3000/")), "http://x:3000");
        std::env::set_var("SDB_SERVER", "http://env:4000/");
        assert_eq!(resolve_server(None), "http://env:4000");
        assert_eq!(resolve_server(Some("http://flag:5000")), "http://flag:5000");
        assert_eq!(resolve_server(Some("")), "http://env:4000");
    }

    #[test]
    fn test_endpoint_join() {
        assert_eq!(
            endpoint("http://h:3000/", "/api/status"),
            "http://h:3000/api/status"
        );
    }

    #[test]
    fn test_run_request_payload() {
        let v = run_request(
            "openrouter/m",
            "redis",
            "high",
            100,
            1.0,
            30,
            None,
            false,
            9,
            false,
            9,
            false,
            None,
        );
        assert_eq!(v["model"], "openrouter/m");
        assert_eq!(v["task"], "redis");
        assert_eq!(v["trials"], 9);
        assert_eq!(v["save_results"], true);
        assert!(v["api_key"].is_null());
        let v2 = run_request(
            "m",
            "wal",
            "low",
            10,
            0.2,
            5,
            Some("k"),
            true,
            0,
            true,
            3,
            true,
            Some("r"),
        );
        assert_eq!(v2["trials"], 1);
        assert_eq!(v2["save_results"], false);
        assert_eq!(v2["review_model"], "r");
    }

    #[test]
    fn test_extend_body_queues() {
        let v = extend_body(6, None, None, None, None);
        assert_eq!(v["trials"], 6);
        assert_eq!(v["queue"], true);
        assert!(v.get("max_turns").is_none());
        let v2 = extend_body(3, Some(50), Some(0.5), Some(10), Some(true));
        assert_eq!(v2["max_turns"], 50);
        assert_eq!(v2["eval_only"], true);
    }

    #[test]
    fn test_queue_summary() {
        let run = serde_json::json!({"Run": {
            "model": "openrouter/meta/muse-spark", "task": "redis",
            "effort": "xhigh", "trials": 9,
        }});
        assert_eq!(queue_summary(&run), "run muse-spark redis xhigh x9");
        let ext = serde_json::json!({"Extend": {
            "parent_run_id": "wal_m_20260916_152544", "trials": 6,
        }});
        assert_eq!(queue_summary(&ext), "extend wal_m_20260916_152544 +6");
    }

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("a/b.py"), "a/b.py");
        assert_eq!(url_encode("a b.py"), "a%20b.py");
    }

    #[test]
    fn test_format_status() {
        let v = serde_json::json!({
            "is_running": true,
            "current_run": {"run_id": "r1", "model": "openrouter/m", "task": "redis",
                "effort": "max", "turns": 3, "max_turns": 100, "cost_usd": 0.01,
                "tokens": 100, "subrun": 2, "total_subruns": 9},
            "last_run": null,
        });
        let s = format_status(&v);
        assert!(s.contains("RUNNING"));
        assert!(s.contains("r1"));
        assert!(s.contains("trial 2/9"));
        assert!(s.contains("last: -"));
    }

    #[test]
    fn test_format_queue() {
        let v = serde_json::json!([
            {"id": "q1", "enqueued_at": "2026-09-22T00:00:00+00:00",
             "status": "Done", "kind": {"Run": {"model": "m", "task": "wal", "effort": "high", "trials": 3}}},
        ]);
        let s = format_queue(&v);
        assert!(s.contains("q1") && s.contains("run m wal high x3"));
        assert!(format_queue(&serde_json::json!([])).contains("empty"));
    }

    #[test]
    fn test_match_manifest() {
        let m = serde_json::json!({"model": "openrouter/m", "task": "redis",
            "effort": "max", "status": "failed_tests"});
        assert!(match_manifest(&m, Some("openrouter"), None, None, None));
        assert!(!match_manifest(&m, Some("glm"), None, None, None));
        assert!(match_manifest(
            &m,
            None,
            Some("REDIS"),
            Some("max"),
            Some("failed_tests")
        ));
        assert!(!match_manifest(&m, None, None, None, Some("completed")));
        let no_effort = serde_json::json!({"model": "m", "task": "t", "status": "s"});
        assert!(!match_manifest(&no_effort, None, None, Some("max"), None));
    }

    #[test]
    fn test_render_log() {
        let body = "a\nStage 1: x -> PASS b\nc\nStage 2: y -> FAIL z\n";
        assert_eq!(render_log(body, 2, false), "c\nStage 2: y -> FAIL z");
        assert!(render_log(body, 10, true).lines().count() == 2);
    }

    #[test]
    fn test_format_models_filtered() {
        let v = serde_json::json!([
            {"id": "z-ai/glm-5", "name": "GLM 5", "prompt_price_per_m": 0.5,
             "completion_price_per_m": 1.0, "context_length": 200000, "created": 100,
             "supported_efforts": ["low", "high"], "default_effort": "high",
             "is_reasoning": true, "supports_max_tokens": false},
            {"id": "anthropic/claude-x", "name": "Claude X", "prompt_price_per_m": 0.2,
             "completion_price_per_m": 0.4, "context_length": 100000, "created": 200,
             "supported_efforts": [], "default_effort": null,
             "is_reasoning": false, "supports_max_tokens": false},
        ]);
        let s = format_models_filtered(&v, Some("glm"), None, "cheapest", 10);
        assert!(s.contains("z-ai/glm-5") && !s.contains("claude"));
        assert!(s.contains("low/high") && s.contains("[low/high]"));
        let s2 = format_models_filtered(&v, Some("claude"), None, "newest", 10);
        assert!(s2.contains("claude-x"));
        let s3 = format_models_filtered(&v, None, Some("GLM"), "name", 1);
        assert!(s3.contains("glm-5"));
        let s4 = format_models_filtered(&v, None, None, "bogus", 10);
        assert!(s4.find("claude-x") < s4.find("glm-5"));
    }

    #[test]
    fn test_format_compare() {
        let v = serde_json::json!({
            "run_a": {"run_id": "a", "pass_rate": 50.0},
            "run_b": {"run_id": "b", "pass_rate": 100.0},
            "metric_diff": {"pass_rate_delta": 50.0, "cost_delta": 0.01,
                "tokens_delta": 5, "duration_delta": 1.5},
            "file_diffs": [{"path": "main.py", "status": "modified"}],
        });
        let s = format_compare(&v);
        assert!(s.contains("a: a (50.0%)") && s.contains("[modified] main.py"));
    }
}
