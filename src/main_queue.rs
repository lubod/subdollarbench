//! Queue CLI handlers: `status` `queue` `ps` `enqueue` `queue-extend` `dequeue` `queue-clear` `stop`.
//!
//! Pure move from `main.rs` (2026-10-05 split): zero behavior change.

use anyhow::Result;
use subdollarbench::config::TaskType;
use subdollarbench::remote;

use super::{resolve_parent_id, resolve_server_flag};

/// `status`: live engine status.
pub async fn handle_status(server: Option<String>, json: bool) -> Result<()> {
    let server = resolve_server_flag(&server);
    let v = remote::get_json(&server, "/api/status").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", remote::format_status(&v));
    }
    Ok(())
}

/// `queue`: live queue contents.
pub async fn handle_queue(server: Option<String>, json: bool) -> Result<()> {
    let server = resolve_server_flag(&server);
    let v = remote::get_json(&server, "/api/queue").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", remote::format_queue(&v));
    }
    Ok(())
}

/// `ps`: merged engine view (queue head + archived manifests).
pub async fn handle_ps(
    server: Option<String>,
    model: Option<String>,
    task: Option<String>,
    effort: Option<String>,
    status: Option<String>,
    limit: usize,
    runs_dir: String,
) -> Result<()> {
    let server = resolve_server_flag(&server);
    // Both calls are read-only; the engine keeps running untouched.
    let queue = remote::get_json(&server, "/api/queue").await?;
    let runs = remote::get_json(&server, "/api/runs").await?;
    let _ = runs_dir;
    print!(
        "{}",
        remote::format_ps(
            &queue,
            &runs,
            model.as_deref(),
            task.as_deref(),
            effort.as_deref(),
            status.as_deref(),
            limit,
        )
    );
    Ok(())
}

/// `enqueue`: one run behind the live job.
#[allow(clippy::too_many_arguments)]
pub async fn handle_enqueue(
    model: String,
    task: TaskType,
    effort: String,
    harness: subdollarbench::config::Harness,
    max_turns: u32,
    budget_usd: f64,
    timeout_min: u64,
    api_key: Option<String>,
    eval_only: bool,
    trials: u32,
    extend_if_unstable: bool,
    max_trials: u32,
    no_save: bool,
    review_model: Option<String>,
    server: Option<String>,
    stop_on_pass: bool,
    feedback_rounds: u32,
) -> Result<()> {
    let server = resolve_server_flag(&server);
    let body = remote::run_request(
        &model,
        &task.to_string(),
        &effort,
        harness.as_str(),
        max_turns,
        budget_usd,
        timeout_min,
        api_key.as_deref(),
        eval_only,
        trials,
        extend_if_unstable,
        max_trials,
        no_save,
        review_model.as_deref(),
        stop_on_pass,
        feedback_rounds,
    );
    let resp = remote::post_json(&server, "/api/queue", &body).await?;
    let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    if resp
        .get("started")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        println!("enqueued {id}: engine idle, started immediately");
    } else {
        let pos = resp
            .get("position")
            .and_then(|v| v.as_u64())
            .map(|p| p.to_string())
            .unwrap_or("-".into());
        println!("enqueued {id} at position {pos}");
    }
    Ok(())
}

/// `queue-extend`: extension behind the live job.
#[allow(clippy::too_many_arguments)]
pub async fn handle_queue_extend(
    parent: String,
    trials: u32,
    max_turns: Option<u32>,
    budget_usd: Option<f64>,
    timeout_min: Option<u64>,
    eval_only: bool,
    results_dir: String,
    server: Option<String>,
) -> Result<()> {
    let server = resolve_server_flag(&server);
    let pid = resolve_parent_id(&results_dir, &parent)?;
    let body = remote::extend_body(
        trials,
        max_turns,
        budget_usd,
        timeout_min,
        eval_only.then_some(true),
    );
    // Server path-encodes cleanly (clean_run_id rejects slashes).
    let resp = remote::post_json(&server, &format!("/api/runs/{pid}/extend"), &body).await?;
    if let Some(qid) = resp.get("queued").and_then(|v| v.as_str()) {
        let pos = resp
            .get("position")
            .and_then(|v| v.as_u64())
            .map(|p| p.to_string())
            .unwrap_or("-".into());
        println!("queued {qid} (extend {pid} +{trials}) at position {pos}");
    } else if let Some(started) = resp.get("started").and_then(|v| v.as_str()) {
        println!("extend {started} started immediately");
    } else {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    }
    Ok(())
}

/// `dequeue`: remove one pending queue item.
pub async fn handle_dequeue(id: String, server: Option<String>) -> Result<()> {
    let server = resolve_server_flag(&server);
    println!(
        "{}",
        remote::delete_text(&server, &format!("/api/queue/{id}")).await?
    );
    Ok(())
}

/// `queue-clear`: drop finished queue history.
pub async fn handle_queue_clear(server: Option<String>) -> Result<()> {
    let server = resolve_server_flag(&server);
    // Server only drops finished history; pending work is untouched.
    println!("{}", remote::delete_text(&server, "/api/queue").await?);
    Ok(())
}

/// `stop`: cancel live run(s).
pub async fn handle_stop(run: String, server: Option<String>) -> Result<()> {
    let server = resolve_server_flag(&server);
    let path = if run.trim().is_empty() {
        "/api/run/stop".to_string()
    } else {
        format!("/api/run/stop?run={}", run.trim())
    };
    println!("{}", remote::post_empty(&server, &path).await?);
    Ok(())
}
