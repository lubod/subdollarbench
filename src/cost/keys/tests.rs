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
