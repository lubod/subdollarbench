//! Queue CLI handlers: `status` `queue` `ps` `enqueue` `queue-extend` `dequeue` `queue-clear` `stop`.
//!
//! Pure move from `main.rs` (2026-10-05 split): zero behavior change.

use anyhow::Result;
use subdollarbench::remote;

use super::resolve_server_flag;

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
