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
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal mock: serves one canned JSON body per connection, records
    /// method + path + whether an Authorization header was present.
    async fn mock_once(
        body: &'static str,
        status: u16,
    ) -> (u16, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let first = head.lines().next().unwrap_or("").to_string();
                let authed = head.to_lowercase().contains("authorization:");
                let _ = tx.send(format!("{first} authed={authed}"));
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        (port, rx)
    }

    #[tokio::test]
    async fn test_mint_posts_limit_and_returns_key_once() {
        let body = r#"{"key":"sk-or-v1-runkey123","data":{"hash":"abc123hash"}}"#;
        let (port, seen) = mock_once(body, 200).await;
        let rk = mint_run_key_at(
            "mgmt-secret",
            "w0",
            2.0,
            &format!("http://127.0.0.1:{port}/keys"),
        )
        .await
        .unwrap();
        assert_eq!(rk.key, "sk-or-v1-runkey123");
        assert_eq!(rk.hash, "abc123hash");
        let hit = seen.await.unwrap();
        assert!(hit.contains("POST"), "mint must POST: {hit}");
        assert!(hit.contains("authed=true"), "mgmt bearer required: {hit}");
    }

    #[tokio::test]
    async fn test_revoke_deletes_by_hash() {
        let (port, seen) = mock_once("{}", 200).await;
        revoke_run_key_at(
            "mgmt-secret",
            "abc123hash",
            &format!("http://127.0.0.1:{port}/keys"),
        )
        .await
        .unwrap();
        let hit = seen.await.unwrap();
        assert!(hit.contains("DELETE"), "revoke must DELETE: {hit}");
        assert!(hit.contains("abc123hash"), "hash in path: {hit}");
        assert!(hit.contains("authed=true"), "mgmt bearer required: {hit}");
    }

    #[tokio::test]
    async fn test_mint_failure_is_loud() {
        let (port, _seen) = mock_once("{}", 401).await;
        // No unwrap_err: RunKey deliberately has no Debug (it would print
        // the plaintext key into test output on failure paths).
        let err = match mint_run_key_at(
            "bad-mgmt",
            "w0",
            2.0,
            &format!("http://127.0.0.1:{port}/keys"),
        )
        .await
        {
            Ok(_) => panic!("mint with 401 must fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("401"), "must surface status: {err}");
    }

    #[test]
    fn test_mgmt_key_absent_is_none() {
        // Never mutate the real env: scoped save/restore.
        let saved = std::env::var("OPENROUTER_MGMT_KEY").ok();
        std::env::remove_var("OPENROUTER_MGMT_KEY");
        assert_eq!(mgmt_key(), None);
        if let Some(v) = saved {
            std::env::set_var("OPENROUTER_MGMT_KEY", v);
        }
    }
}
