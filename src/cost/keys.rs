//! Per-run capped OpenRouter keys: blast-radius control for the model container.
//!
//! `omp.rs` passes the key into the agent container by design, and the egress
//! filter bounds the route, not the secret. So the key that enters the
//! container must not be the account key: mint a capped key per batch
//! (`limit` + 24h `expires_at`), pass *that* in-container, revoke at
//! `finish()`. A leak buys hours and one batch budget, not the account.
//!
//! Minting needs a **management** key (`OPENROUTER_MGMT_KEY`, 0600 like the
//! account key). Absent/empty mgmt key = fail-open to today's behavior with
//! a loud log line, never a silent downgrade and never a failed batch.

use anyhow::{Context, Result};

/// Management key source: `OPENROUTER_MGMT_KEY` env. Empty when unset, so
/// callers fail open loudly instead of failing the batch.
pub fn mgmt_key() -> Option<String> {
    std::env::var("OPENROUTER_MGMT_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// A minted per-run key: plaintext (write-once, never logged) + hash for revoke.
pub struct RunKey {
    pub key: String,
    pub hash: String,
}

/// Mint a capped key for one batch. `limit_usd` = batch budget + headroom.
/// Name carries the run id for dashboard attribution.
pub async fn mint_run_key(run_id: &str, limit_usd: f64) -> Result<RunKey> {
    let mgmt = mgmt_key().context("no OPENROUTER_MGMT_KEY configured")?;
    mint_run_key_at(
        &mgmt,
        run_id,
        limit_usd,
        "https://openrouter.ai/api/v1/keys",
    )
    .await
}

pub async fn mint_run_key_at(
    mgmt_key: &str,
    run_id: &str,
    limit_usd: f64,
    url: &str,
) -> Result<RunKey> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    // 24h expiry: a leaked key dies on its own even if revoke is missed.
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(24);
    let body = serde_json::json!({
        "name": format!("sdb-{run_id}"),
        "limit": limit_usd,
        "expires_at": expires_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    });
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", mgmt_key.trim()))
        .json(&body)
        .send()
        .await
        .context("mint run key")?;
    if !resp.status().is_success() {
        anyhow::bail!("mint run key -> {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await.context("mint key body")?;
    // Never log `key`: plaintext is returned once, write-only by design.
    let key = v
        .get("key")
        .and_then(|k| k.as_str())
        .context("mint response has no key")?
        .to_string();
    let hash = v
        .get("data")
        .and_then(|d| d.get("hash"))
        .and_then(|h| h.as_str())
        .context("mint response has no hash")?
        .to_string();
    Ok(RunKey { key, hash })
}

/// Revoke a run key by hash. Best-effort at batch end: log, never fail.
pub async fn revoke_run_key(hash: &str) -> Result<()> {
    let mgmt = mgmt_key().context("no OPENROUTER_MGMT_KEY configured")?;
    revoke_run_key_at(&mgmt, hash, "https://openrouter.ai/api/v1/keys").await
}

pub async fn revoke_run_key_at(mgmt_key: &str, hash: &str, base_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let url = format!("{}/{hash}", base_url.trim_end_matches('/'));
    let resp = client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", mgmt_key.trim()))
        .send()
        .await
        .context("revoke run key")?;
    if !resp.status().is_success() {
        anyhow::bail!("revoke run key -> {}", resp.status());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
