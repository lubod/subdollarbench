use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::StageResult;

/// Upload cap the campaign contract advertises (the hardening stage declares
/// 128MiB, far above it).
pub const UPLOAD_CAP_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct HttpVerifier {
    pub target_port: u16,
    pub client: reqwest::Client,
    pub run_seed: Option<String>,
}

impl HttpVerifier {
    /// A client that never reuses connections. The candidate's own idle
    /// reclaim may close a pooled keep-alive connection between two
    /// probes, and a rejected/closed idle socket is not a health signal
    /// (the stage-10 final GET flaked exactly so on 2026-10-16).
    fn fresh_client() -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            // The same bound as the shared client: these probes run while a
            // stalled slow-loris connection is held, which is exactly when a
            // broken candidate accepts and never answers. Without a timeout
            // the verification would hang forever.
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| format!("client: {e}"))
    }

    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        Self {
            target_port,
            client,
            run_seed: seed.map(|s| s.to_string()),
        }
    }

    pub fn get_seed_prefix<'a>(&'a self, default_val: &'a str) -> &'a str {
        super::get_seed_prefix(self.run_seed.as_deref(), default_val)
    }

    pub async fn run_all(&self) -> HttpTestSummary {
        let mut stages = Vec::new();
        stages.push(self.test_stage1_root().await);
        stages.push(self.test_stage2_not_found().await);
        stages.push(self.test_stage3_echo().await);
        stages.push(self.test_stage4_user_agent().await);
        stages.push(self.test_stage5_file_storage().await);

        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;

        HttpTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    /// Advisory robustness probes (NOT scored): malformed-request survival
    /// and slow-header isolation. The server may close the offending
    /// connection — that is acceptable behavior. What passes is *service*
    /// survival on fresh connections plus concurrency under a half-open
    /// sender. Never called from `run_all`; the pipeline logs these
    /// separately under [ROBUST].
    pub async fn run_robustness(&self) -> Vec<StageResult> {
        vec![
            self.robust_malformed().await,
            self.robust_slow_headers().await,
        ]
    }

    async fn robust_get_status(port: u16) -> Result<u16, String> {
        use tokio::io::AsyncWriteExt;
        let addr = format!("127.0.0.1:{}", port);
        let mut stream = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("Connection timed out to {}", addr))?
        .map_err(|e| format!("Failed to connect to {}: {}", addr, e))?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .map_err(|e| format!("Write to {} failed: {}", addr, e))?;
        let (status, complete) = Self::read_framed(&mut stream, &addr).await?;
        if !complete {
            return Err(format!("unframed response from {}", addr));
        }
        Ok(status)
    }

    async fn robust_malformed(&self) -> StageResult {
        let name = "Robust 1: malformed request survival (advisory)".to_string();
        let fail = |e: String| StageResult {
            stage: 101,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        use tokio::io::AsyncWriteExt;
        let addr = format!("127.0.0.1:{}", self.target_port);
        let mut dirty = match tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            _ => return fail(format!("connect for garbage probe: {}", addr)),
        };
        // Garbage request line + a header with no colon on one connection.
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            dirty.write_all(b"\x00\x01 GARBAGE\r\nNoColonHere\r\n\r\n"),
        )
        .await;
        drop(dirty);
        match tokio::time::timeout(
            Duration::from_secs(8),
            Self::robust_get_status(self.target_port),
        )
        .await
        {
            Ok(Ok(200)) => StageResult {
                stage: 101,
                name,
                passed: true,
                error: None,
            },
            Ok(Ok(s)) => fail(format!("GET / after garbage got {s}, want 200")),
            Ok(Err(e)) => fail(format!("GET / after garbage failed: {e}")),
            Err(_) => fail("GET / after garbage timed out (server wedged?)".to_string()),
        }
    }

    async fn robust_slow_headers(&self) -> StageResult {
        let name = "Robust 2: slow headers + half-open isolation (advisory)".to_string();
        let fail = |e: String| StageResult {
            stage: 102,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        use tokio::io::AsyncWriteExt;
        let addr = format!("127.0.0.1:{}", self.target_port);
        let mut slow = match tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            _ => return fail(format!("connect slow sender: {}", addr)),
        };
        if tokio::time::timeout(
            Duration::from_secs(3),
            slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n"),
        )
        .await
        .is_err()
        {
            return fail("partial header write failed".to_string());
        }
        // An independent client must succeed while the sender is half-open.
        match tokio::time::timeout(
            Duration::from_secs(8),
            Self::robust_get_status(self.target_port),
        )
        .await
        {
            Ok(Ok(200)) => {}
            Ok(Ok(s)) => return fail(format!("parallel GET / got {s} during half-open")),
            Ok(Err(e)) => return fail(format!("parallel GET / failed during half-open: {e}")),
            Err(_) => {
                return fail(
                    "parallel GET / timed out during half-open (single-threaded server?)"
                        .to_string(),
                )
            }
        }
        // Complete the slow request; expect a framed 200.
        if tokio::time::timeout(
            Duration::from_secs(3),
            slow.write_all(b"Connection: close\r\n\r\n"),
        )
        .await
        .is_err()
        {
            return fail("completion write failed".to_string());
        }
        match tokio::time::timeout(Duration::from_secs(8), Self::read_framed(&mut slow, &addr))
            .await
        {
            Ok(Ok((200, true))) => StageResult {
                stage: 102,
                name,
                passed: true,
                error: None,
            },
            Ok(Ok((s, c))) => fail(format!(
                "slow request got status {s} complete={c}, want 200+framed"
            )),
            Ok(Err(e)) => fail(format!("slow request read failed: {e}")),
            Err(_) => fail("slow request reply timed out".to_string()),
        }
    }
    pub async fn test_stage1_root(&self) -> StageResult {
        let name = "Stage 1: GET / (Root 200 OK + keep-alive)".to_string();
        // Raw socket, not reqwest: pooling only reuses a connection after the
        // body fully drains, and `Connection: close` silently reconnects —
        // a reqwest loop re-tests "200×3", never same-socket reuse. 3
        // sequential GETs on ONE `Connection: keep-alive` socket, 200s
        // counted by framed boundaries. A close-per-request stub fails.
        match Self::fetch_keepalive(self.target_port, 3).await {
            Ok(3) => StageResult {
                stage: 1,
                name,
                passed: true,
                error: None,
            },
            Ok(n) => StageResult {
                stage: 1,
                name,
                passed: false,
                error: Some(format!(
                    "keep-alive: {n}/3 sequential 200s on one connection"
                )),
            },
            Err(e) => StageResult {
                stage: 1,
                name,
                passed: false,
                error: Some(format!("GET / via keep-alive: {}", e)),
            },
        }
    }

    /// 3 sequential `GET /` on one keep-alive socket, 200s by framed
    /// Content-Length boundaries. Mirrors proxy.rs::fetch_keepalive.
    async fn fetch_keepalive(port: u16, n: usize) -> Result<usize, String> {
        use tokio::io::AsyncWriteExt;
        let addr = format!("127.0.0.1:{}", port);
        let conn = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("Connection timed out to {}", addr))?;
        let mut stream = conn.map_err(|e| format!("Failed to connect to {}: {}", addr, e))?;
        let mut ok = 0;
        for _ in 0..n {
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n")
                .await
                .map_err(|e| format!("Write to {} failed: {}", addr, e))?;
            let (status, complete) = Self::read_framed(&mut stream, &addr).await?;
            if !complete {
                return Err(format!("keep-alive response unframed from {}", addr));
            }
            if status != 200 {
                return Err(format!("keep-alive GET got {}", status));
            }
            ok += 1;
        }
        Ok(ok)
    }

    /// Read one framed response: status + whether Content-Length is
    /// satisfied (chunked accepted as framed). Minimal local framing —
    /// proxy.rs owns the full parser; this stays dependency-free.
    async fn read_framed(
        stream: &mut tokio::net::TcpStream,
        addr: &str,
    ) -> Result<(u16, bool), String> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(8) {
                return Err(format!("keep-alive read from {} timed out", addr));
            }
            match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut tmp)).await {
                Err(_) => return Err(format!("Read from {} timed out", addr)),
                Ok(Err(e)) => return Err(format!("Read from {} failed: {}", addr, e)),
                Ok(Ok(0)) => break,
                Ok(Ok(m)) => {
                    buf.extend_from_slice(&tmp[..m]);
                    if let Some((status, len)) = Self::head_status(&buf) {
                        if buf.len() >= len {
                            return Ok((status, true));
                        }
                    }
                }
            }
        }
        let status = Self::head_status(&buf).map(|(s, _)| s).unwrap_or(0);
        Ok((status, false))
    }

    /// Parse status + total framed length from a raw head. None while the
    /// head is incomplete.
    fn head_status(buf: &[u8]) -> Option<(u16, usize)> {
        let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = std::str::from_utf8(&buf[..head_end]).ok()?;
        let mut lines = head.lines();
        let status_line = lines.next()?;
        let mut parts = status_line.splitn(3, ' ');
        parts.next()?;
        let status: u16 = parts.next()?.parse().ok()?;
        let mut len = head_end + 4;
        let mut chunked = false;
        for line in lines {
            let (k, v) = line.split_once(':')?;
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = head_end + 4 + v.trim().parse::<usize>().ok()?,
                "transfer-encoding" if v.to_ascii_lowercase().contains("chunked") => chunked = true,
                _ => {}
            }
        }
        if chunked {
            // Framed when the terminal zero chunk is present.
            let body = &buf[head_end + 4..];
            return body
                .windows(5)
                .any(|w| w == b"0\r\n\r\n")
                .then_some((status, buf.len()));
        }
        Some((status, len))
    }

    pub async fn test_stage2_not_found(&self) -> StageResult {
        let name = "Stage 2: 404 Not Found Handling".to_string();
        let url = format!(
            "http://127.0.0.1:{}/nonexistent-route-1234",
            self.target_port
        );
        match self.client.get(&url).send().await {
            Ok(resp) => {
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    StageResult {
                        stage: 2,
                        name,
                        passed: true,
                        error: None,
                    }
                } else {
                    StageResult {
                        stage: 2,
                        name,
                        passed: false,
                        error: Some(format!("Expected 404 Not Found, got {}", resp.status())),
                    }
                }
            }
            Err(e) => StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub async fn test_stage3_echo(&self) -> StageResult {
        let name = "Stage 3: GET /echo/{str}".to_string();
        let prefix = self.get_seed_prefix("test");
        let word = if prefix == "test" {
            "subdollar_speed_test".to_string()
        } else {
            format!("echo_{}", prefix)
        };
        let url = format!("http://127.0.0.1:{}/echo/{}", self.target_port, word);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::OK {
                    return StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!("Expected 200 OK, got {}", resp.status())),
                    };
                }

                // Verify Content-Type header
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !content_type.to_lowercase().starts_with("text/plain") {
                    return StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Type 'text/plain', got '{}'",
                            content_type
                        )),
                    };
                }

                // Verify Content-Length header
                let content_length = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if content_length != word.len().to_string() {
                    return StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Length '{}', got '{}'",
                            word.len(),
                            content_length
                        )),
                    };
                }

                match resp.text().await {
                    Ok(text) if text.trim() == word => StageResult {
                        stage: 3,
                        name,
                        passed: true,
                        error: None,
                    },
                    Ok(text) => StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!("Body didn't match '{}', got: '{}'", word, text)),
                    },
                    Err(e) => StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(e.to_string()),
                    },
                }
            }
            Err(e) => StageResult {
                stage: 3,
                name,
                passed: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub async fn test_stage4_user_agent(&self) -> StageResult {
        let name = "Stage 4: GET /user-agent Header Echo".to_string();
        let ua = "SubDollarBench-Agent/1.0";
        let url = format!("http://127.0.0.1:{}/user-agent", self.target_port);
        match self.client.get(&url).header("User-Agent", ua).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::OK {
                    return StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!("Expected 200 OK, got {}", resp.status())),
                    };
                }

                // Verify Content-Type header
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !content_type.to_lowercase().starts_with("text/plain") {
                    return StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Type 'text/plain', got '{}'",
                            content_type
                        )),
                    };
                }

                // Verify Content-Length header
                let content_length = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if content_length != ua.len().to_string() {
                    return StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Length '{}', got '{}'",
                            ua.len(),
                            content_length
                        )),
                    };
                }

                match resp.text().await {
                    Ok(text) if text.trim() == ua => StageResult {
                        stage: 4,
                        name,
                        passed: true,
                        error: None,
                    },
                    Ok(text) => StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!("Expected UA '{}', got '{}'", ua, text)),
                    },
                    Err(e) => StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(e.to_string()),
                    },
                }
            }
            Err(e) => StageResult {
                stage: 4,
                name,
                passed: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub async fn test_stage5_file_storage(&self) -> StageResult {
        let name = "Stage 5: POST & GET /files/{filename}".to_string();
        let seed_prefix = self.get_seed_prefix("artifact");
        let filename = format!("bench_{}.txt", seed_prefix);
        let content = format!("SubDollarBench payload {}", seed_prefix);
        let post_url = format!("http://127.0.0.1:{}/files/{}", self.target_port, filename);
        let get_url = format!("http://127.0.0.1:{}/files/{}", self.target_port, filename);

        // POST request must return strictly 201 Created
        match self
            .client
            .post(&post_url)
            .body(content.clone())
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::CREATED {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!(
                            "POST /files expected 201 Created, got status: {}",
                            resp.status()
                        )),
                    };
                }
            }
            Err(e) => {
                return StageResult {
                    stage: 5,
                    name,
                    passed: false,
                    error: Some(format!("POST error: {}", e)),
                }
            }
        }

        // GET request to retrieve created file
        match self.client.get(&get_url).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::OK {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!("GET /files failed with status: {}", resp.status())),
                    };
                }

                // Verify Content-Length header
                let content_length = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if content_length != content.len().to_string() {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Length '{}', got '{}'",
                            content.len(),
                            content_length
                        )),
                    };
                }

                match resp.text().await {
                    Ok(text) if text.trim() == content => {}
                    Ok(text) => {
                        return StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(format!(
                                "File content mismatch! Expected '{}', got '{}'",
                                content, text
                            )),
                        }
                    }
                    Err(e) => {
                        return StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(e.to_string()),
                        }
                    }
                }
            }
            Err(e) => {
                return StageResult {
                    stage: 5,
                    name,
                    passed: false,
                    error: Some(e.to_string()),
                }
            }
        }

        // GET request for non-existent file must return 404
        let nonexistent_url = format!(
            "http://127.0.0.1:{}/files/missing_{}.txt",
            self.target_port, seed_prefix
        );
        match self.client.get(&nonexistent_url).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::NOT_FOUND {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected 404 Not Found for missing file, got {}",
                            resp.status()
                        )),
                    };
                }
            }
            Err(e) => {
                return StageResult {
                    stage: 5,
                    name,
                    passed: false,
                    error: Some(format!("GET missing file error: {}", e)),
                }
            }
        }

        // Path Traversal Security Check: request paths with directory traversal
        let traversal_payloads = [
            "../../../../etc/passwd",
            "../start.sh",
            "..%2F..%2Fetc%2Fpasswd",
        ];
        for payload in &traversal_payloads {
            let traversal_url = format!("http://127.0.0.1:{}/files/{}", self.target_port, payload);
            if let Ok(resp) = self.client.get(&traversal_url).send().await {
                if resp.status() == reqwest::StatusCode::OK {
                    let body = resp.text().await.unwrap_or_default();
                    if body.contains("root:") || body.contains("#!/bin") || body.contains("exec ") {
                        return StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(format!(
                                "Security failure: server allows directory traversal ({}) leaking sensitive content",
                                payload
                            )),
                        };
                    }
                }
            }
        }

        StageResult {
            stage: 5,
            name,
            passed: true,
            error: None,
        }
    }

    /// Stage 6 (campaign sdb-http): byte ranges on stored files.
    pub async fn test_stage6_range(&self) -> StageResult {
        let name = "Stage 6: Range requests (206 + Content-Range)".to_string();
        match self.stage6_inner().await {
            Ok(()) => StageResult {
                stage: 6,
                name,
                passed: true,
                error: None,
            },
            Err(e) => StageResult {
                stage: 6,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    async fn stage6_inner(&self) -> Result<(), String> {
        let seed = self.get_seed_prefix("range");
        let filename = format!("range_{seed}.txt");
        // Deterministic, position-distinguishable content.
        let content: String = (0..64).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let url = format!("http://127.0.0.1:{}/files/{}", self.target_port, filename);
        let post = self.client.post(&url).body(content.clone()).send().await;
        match post {
            Ok(r) if r.status() == reqwest::StatusCode::CREATED => {}
            Ok(r) => return Err(format!("upload for range test: got {}", r.status())),
            Err(e) => return Err(format!("upload for range test: {e}")),
        }
        let expect_range = |got: Option<&str>, want: &str| -> Result<(), String> {
            match got {
                Some(v) if v.trim().eq_ignore_ascii_case(want) => Ok(()),
                Some(v) => Err(format!("Content-Range {v:?} (want {want:?})")),
                None => Err(format!("missing Content-Range (want {want:?})")),
            }
        };
        // 1. bytes=2-5
        let resp = self
            .client
            .get(&url)
            .header("Range", "bytes=2-5")
            .send()
            .await
            .map_err(|e| format!("range 2-5: {e}"))?;
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(format!(
                "Range bytes=2-5 must be 206, got {}",
                resp.status()
            ));
        }
        expect_range(
            resp.headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok()),
            &format!("bytes 2-5/{len}", len = content.len()),
        )?;
        let body = resp.bytes().await.map_err(|e| format!("range body: {e}"))?;
        if body != content.as_bytes()[2..6] {
            return Err(format!(
                "Range 2-5 body {:?}",
                String::from_utf8_lossy(&body)
            ));
        }
        // 2. Suffix range bytes=-4
        let resp = self
            .client
            .get(&url)
            .header("Range", "bytes=-4")
            .send()
            .await
            .map_err(|e| format!("suffix range: {e}"))?;
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(format!("suffix Range must be 206, got {}", resp.status()));
        }
        expect_range(
            resp.headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok()),
            &format!(
                "bytes {}-{}/{len}",
                content.len() - 4,
                content.len() - 1,
                len = content.len()
            ),
        )?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("suffix body: {e}"))?;
        if body != content.as_bytes()[content.len() - 4..] {
            return Err("suffix range returned the wrong bytes".to_string());
        }
        // 3. Open-ended range bytes=60-
        let resp = self
            .client
            .get(&url)
            .header("Range", "bytes=60-")
            .send()
            .await
            .map_err(|e| format!("open range: {e}"))?;
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(format!(
                "open-ended Range must be 206, got {}",
                resp.status()
            ));
        }
        let body = resp.bytes().await.map_err(|e| format!("open body: {e}"))?;
        if body != content.as_bytes()[60..] {
            return Err("open-ended range returned the wrong bytes".to_string());
        }
        // 4. Unsatisfiable range -> 416
        let resp = self
            .client
            .get(&url)
            .header("Range", "bytes=500-600")
            .send()
            .await
            .map_err(|e| format!("unsatisfiable range: {e}"))?;
        if resp.status() != reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(format!(
                "unsatisfiable Range must be 416, got {}",
                resp.status()
            ));
        }
        // 5. A plain GET is still 200 + the full body.
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("plain get: {e}"))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!("plain GET after ranges: got {}", resp.status()));
        }
        let body = resp.bytes().await.map_err(|e| format!("plain body: {e}"))?;
        if body != content.as_bytes() {
            return Err("plain GET body differs from the uploaded content".to_string());
        }
        Ok(())
    }

    /// Stage 7 (campaign sdb-http): validators and conditional GET.
    pub async fn test_stage7_conditional(&self) -> StageResult {
        let name = "Stage 7: Conditional GET (ETag / Last-Modified -> 304)".to_string();
        match self.stage7_inner().await {
            Ok(()) => StageResult {
                stage: 7,
                name,
                passed: true,
                error: None,
            },
            Err(e) => StageResult {
                stage: 7,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    async fn stage7_inner(&self) -> Result<(), String> {
        let seed = self.get_seed_prefix("cond");
        let filename = format!("cond_{seed}.txt");
        let url = format!("http://127.0.0.1:{}/files/{}", self.target_port, filename);
        let first = format!("version-one {seed}");
        match self.client.post(&url).body(first.clone()).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::CREATED => {}
            Ok(r) => return Err(format!("upload v1: got {}", r.status())),
            Err(e) => return Err(format!("upload v1: {e}")),
        }
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("get v1: {e}"))?;
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string())
            .ok_or("GET /files must carry an ETag validator")?;
        let last_modified = resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string())
            .ok_or("GET /files must carry a Last-Modified validator")?;
        // Matching validator -> 304 with no body.
        let resp = self
            .client
            .get(&url)
            .header("If-None-Match", &etag)
            .send()
            .await
            .map_err(|e| format!("if-none-match: {e}"))?;
        if resp.status() != reqwest::StatusCode::NOT_MODIFIED {
            return Err(format!(
                "If-None-Match (matching) must be 304, got {}",
                resp.status()
            ));
        }
        let body = resp.bytes().await.map_err(|e| format!("304 body: {e}"))?;
        if !body.is_empty() {
            return Err("a 304 response must not carry a body".to_string());
        }
        // Stale validator -> 200 with the body.
        let resp = self
            .client
            .get(&url)
            .header("If-None-Match", "\"stale-validator\"")
            .send()
            .await
            .map_err(|e| format!("stale validator: {e}"))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!(
                "a stale If-None-Match must be 200, got {}",
                resp.status()
            ));
        }
        // If-Modified-Since in the future -> 304.
        let resp = self
            .client
            .get(&url)
            .header("If-Modified-Since", "Wed, 01 Jan 2031 00:00:00 GMT")
            .send()
            .await
            .map_err(|e| format!("if-modified-since: {e}"))?;
        if resp.status() != reqwest::StatusCode::NOT_MODIFIED {
            return Err(format!(
                "If-Modified-Since (future) must be 304, got {} (Last-Modified was {last_modified:?})",
                resp.status()
            ));
        }
        // New content -> new body and a *different* ETag; the old validator
        // must no longer 304.
        tokio::time::sleep(Duration::from_millis(1100)).await; // mtime tick
        let second = format!("version-two {seed} (changed)");
        match self.client.post(&url).body(second.clone()).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::CREATED => {}
            Ok(r) => return Err(format!("upload v2: got {}", r.status())),
            Err(e) => return Err(format!("upload v2: {e}")),
        }
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("get v2: {e}"))?;
        let etag2 = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string())
            .ok_or("GET after rewrite lost its ETag")?;
        let body = resp.bytes().await.map_err(|e| format!("v2 body: {e}"))?;
        if body != second.as_bytes() {
            return Err("rewritten file served old bytes".to_string());
        }
        if etag2 == etag {
            return Err("the ETag did not change when the content changed".to_string());
        }
        let resp = self
            .client
            .get(&url)
            .header("If-None-Match", &etag)
            .send()
            .await
            .map_err(|e| format!("stale after write: {e}"))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!(
                "the pre-rewrite validator must not 304 anymore, got {}",
                resp.status()
            ));
        }
        Ok(())
    }

    /// Stage 8 (campaign sdb-http): gzip content negotiation.
    pub async fn test_stage8_compression(&self) -> StageResult {
        let name = "Stage 8: gzip content negotiation".to_string();
        match self.stage8_inner().await {
            Ok(()) => StageResult {
                stage: 8,
                name,
                passed: true,
                error: None,
            },
            Err(e) => StageResult {
                stage: 8,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    async fn stage8_inner(&self) -> Result<(), String> {
        let seed = self.get_seed_prefix("gzip");
        let filename = format!("gzip_{seed}.txt");
        // Highly compressible and big enough that skipping gzip is a choice.
        let content = "SubDollarBench compressible payload. ".repeat(160);
        let url = format!("http://127.0.0.1:{}/files/{}", self.target_port, filename);
        match self.client.post(&url).body(content.clone()).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::CREATED => {}
            Ok(r) => return Err(format!("upload: got {}", r.status())),
            Err(e) => return Err(format!("upload: {e}")),
        }
        // Accept-Encoding: gzip -> Content-Encoding gzip + decodable bytes.
        let resp = self
            .client
            .get(&url)
            .header("Accept-Encoding", "gzip")
            .send()
            .await
            .map_err(|e| format!("gzip get: {e}"))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!(
                "GET with Accept-Encoding: gzip got {}",
                resp.status()
            ));
        }
        let encoding = resp
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("identity")
            .trim()
            .to_ascii_lowercase();
        if encoding != "gzip" {
            return Err(format!(
                "a compressible file with Accept-Encoding: gzip must answer Content-Encoding: gzip (got {encoding:?})"
            ));
        }
        let raw = resp.bytes().await.map_err(|e| format!("gzip body: {e}"))?;
        let mut decoded = Vec::new();
        {
            use std::io::Read;
            let mut gz = flate2::read::GzDecoder::new(raw.as_ref());
            gz.read_to_end(&mut decoded)
                .map_err(|e| format!("gzip body does not decode: {e}"))?;
        }
        if decoded != content.as_bytes() {
            return Err("the decompressed body differs from the stored file".to_string());
        }
        if raw.len() >= content.len() {
            return Err(format!(
                "gzip body ({} bytes) is not smaller than the payload ({} bytes)",
                raw.len(),
                content.len()
            ));
        }
        // identity -> raw bytes, no content-encoding.
        let resp = self
            .client
            .get(&url)
            .header("Accept-Encoding", "identity")
            .send()
            .await
            .map_err(|e| format!("identity get: {e}"))?;
        let encoding = resp
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("identity")
            .trim()
            .to_ascii_lowercase();
        if encoding != "identity" {
            return Err(format!(
                "Accept-Encoding: identity must not be answered with {encoding:?}"
            ));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("identity body: {e}"))?;
        if body != content.as_bytes() {
            return Err("identity body differs from the stored file".to_string());
        }
        Ok(())
    }

    /// Stage 9 (campaign sdb-http; the pipeline restarts the candidate under
    /// a 128MB cap): chunked uploads, Expect: 100-continue, and a declared
    /// body far above the cap.
    pub async fn test_stage9_uploads(&self) -> StageResult {
        let name = "Stage 9: Chunked uploads, 100-continue, size limits".to_string();
        match self.stage9_inner().await {
            Ok(()) => StageResult {
                stage: 9,
                name,
                passed: true,
                error: None,
            },
            Err(e) => StageResult {
                stage: 9,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    async fn stage9_inner(&self) -> Result<(), String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seed = self.get_seed_prefix("upload");
        // (a) Chunked upload arrives byte-exact.
        let filename = format!("chunked_{seed}.bin");
        let req = format!(
            "POST /files/{filename} HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nABCD\r\n5\r\nEFGHI\r\n0\r\n\r\n"
        );
        let (buf, _closed) = self.raw_exchange(&req, Duration::from_secs(6)).await?;
        let status = Self::head_status(&buf).map(|(s, _)| s).unwrap_or(0);
        if status != 201 {
            return Err(format!(
                "chunked POST must answer 201, got {status} ({} bytes)",
                buf.len()
            ));
        }
        let url = format!("http://127.0.0.1:{}/files/{filename}", self.target_port);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("get chunked: {e}"))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("chunked body: {e}"))?;
        if body.as_ref() != b"ABCDEFGHI" {
            return Err(format!(
                "chunked upload stored {:?} (want ABCDEFGHI)",
                String::from_utf8_lossy(&body)
            ));
        }
        // (b) Expect: 100-continue must not deadlock: within ~2s the server
        // either sends an interim 100 or the final status. Then the upload
        // completes byte-exact.
        let filename2 = format!("expect_{seed}.bin");
        let payload = "continue-payload";
        let head = format!(
            "POST /files/{filename2} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let addr = format!("127.0.0.1:{}", self.target_port);
        let mut stream = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("connect to {addr} timed out"))?
        .map_err(|e| format!("connect to {addr}: {e}"))?;
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| format!("100-continue head write: {e}"))?;
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 1024];
        let interim_deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut interim = false;
        while !interim && std::time::Instant::now() < interim_deadline {
            match tokio::time::timeout(Duration::from_millis(400), stream.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some((status, _)) = Self::head_status(&buf) {
                        if status == 100 {
                            interim = true;
                        } else if status >= 200 {
                            // Final status without an interim reply: allowed.
                            let text = String::from_utf8_lossy(&buf).to_string();
                            if status != 201 {
                                return Err(format!(
                                    "POST with Expect answered {status} without an interim 100: {text}"
                                ));
                            }
                            // Body was never sent; the server chose to answer
                            // early, which is legal. Verify storage is empty
                            // or skip to (c).
                            return self.stage9_limits_only().await;
                        }
                    }
                }
                Ok(Err(e)) => return Err(format!("100-continue read: {e}")),
                Err(_) => {}
            }
        }
        if !interim {
            return Err(
                "Expect: 100-continue produced neither an interim 100 nor a final status within 3s (deadlock)"
                    .to_string(),
            );
        }
        // Send the body; the server must finish the upload.
        stream
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("body write: {e}"))?;
        let mut final_buf = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut final_status = 0u16;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(400), stream.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    final_buf.extend_from_slice(&tmp[..n]);
                    if let Some((status, len)) = Self::head_status(&final_buf) {
                        if status >= 200 && final_buf.len() >= len {
                            final_status = status;
                            break;
                        }
                    }
                }
                Ok(Err(e)) => return Err(format!("final read: {e}")),
                Err(_) => {}
            }
        }
        if final_status != 201 {
            return Err(format!(
                "upload after 100-continue must end 201, got {final_status}"
            ));
        }
        let url2 = format!("http://127.0.0.1:{}/files/{filename2}", self.target_port);
        let resp = self
            .client
            .get(&url2)
            .send()
            .await
            .map_err(|e| format!("get expect: {e}"))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("expect body: {e}"))?;
        if body != payload.as_bytes() {
            return Err("the 100-continue upload was not stored byte-exact".to_string());
        }
        self.stage9_limits_only().await
    }

    async fn stage9_limits_only(&self) -> Result<(), String> {
        // (c) A declared 128MiB body must be refused without reading it.
        let req = "POST /files/huge.bin HTTP/1.1\r\nHost: x\r\nContent-Length: 134217728\r\nConnection: close\r\n\r\n";
        let (buf, closed) = self.raw_exchange(req, Duration::from_secs(5)).await?;
        let status = Self::head_status(&buf).map(|(s, _)| s).unwrap_or(0);
        let ok = matches!(status, 400 | 411 | 413) || (status == 0 && closed);
        if !ok {
            return Err(format!(
                "a declared 128MiB body (cap is {} bytes) must be refused before the bytes arrive, got status {status} (closed={closed})",
                UPLOAD_CAP_BYTES
            ));
        }
        Ok(())
    }

    /// Stage 10 (campaign sdb-http; runs under the same 128MB cap): path
    /// traversal, oversized heads, malformed lines and slow-loris, with
    /// normal traffic flowing.
    pub async fn test_stage10_hardening(&self) -> StageResult {
        let name = "Stage 10: Hardening (traversal, limits, slow-loris)".to_string();
        match self.stage10_inner().await {
            Ok(()) => StageResult {
                stage: 10,
                name,
                passed: true,
                error: None,
            },
            Err(e) => StageResult {
                stage: 10,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    async fn stage10_inner(&self) -> Result<(), String> {
        // (a) Path traversal must never serve outside the store.
        for target in [
            "/files/../../etc/passwd",
            "/files/..%2f..%2fetc%2fpasswd",
            "/files/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        ] {
            let req = format!("GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
            let (buf, _closed) = self.raw_exchange(&req, Duration::from_secs(5)).await?;
            let status = Self::head_status(&buf).map(|(s, _)| s).unwrap_or(0);
            if status == 200 {
                let text = String::from_utf8_lossy(&buf);
                if text.contains("root:") {
                    return Err(format!("path traversal {target:?} leaked /etc/passwd"));
                }
                return Err(format!(
                    "path traversal {target:?} answered 200; expect 400/404/403"
                ));
            }
            if !matches!(status, 400 | 403 | 404) {
                return Err(format!(
                    "path traversal {target:?} must answer 400/403/404 (or close), got {status}"
                ));
            }
        }
        // (b) Oversized header block: 4xx or a clean close, server survives.
        let big = format!(
            "GET / HTTP/1.1\r\nHost: x\r\nX-Big: {}\r\nConnection: close\r\n\r\n",
            "a".repeat(32 * 1024)
        );
        let (buf, closed) = self.raw_exchange(&big, Duration::from_secs(5)).await?;
        let status = Self::head_status(&buf).map(|(s, _)| s).unwrap_or(0);
        let ok = matches!(status, 400 | 413 | 431) || (status == 0 && closed);
        if !ok {
            return Err(format!(
                "32KiB of headers must be refused (431/400/close), got {status} (closed={closed})"
            ));
        }
        // (c) Garbage request line: any 4xx (an unknown method is a method
        // error, not necessarily a syntax error) or a clean close is
        // acceptable; a 2xx or a hang is not.
        let (buf, closed) = self
            .raw_exchange(
                "GARBAGE / HTTP/1.1\r\nHost: x\r\n\r\n",
                Duration::from_secs(4),
            )
            .await?;
        let status = Self::head_status(&buf).map(|(s, _)| s).unwrap_or(0);
        let rejected = (400..500).contains(&status) || (status == 0 && closed);
        if !rejected {
            return Err(format!(
                "a garbage request line must be rejected with a 4xx (or close), got {status}"
            ));
        }
        // (d) Slow-loris: reclaimed within ~12s while normal traffic flows.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = format!("127.0.0.1:{}", self.target_port);
        let mut stall = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("stall connect to {addr} timed out"))?
        .map_err(|e| format!("stall connect: {e}"))?;
        stall
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
            .await
            .map_err(|e| format!("stall write: {e}"))?;
        // Normal traffic must still work.
        let probe = Self::fresh_client()?
            .get(format!("http://127.0.0.1:{}/", self.target_port))
            .send()
            .await
            .map_err(|e| format!("GET / while a slow client idled: {e}"))?;
        if probe.status() != reqwest::StatusCode::OK {
            return Err(format!(
                "GET / while a slow client idled returned {}",
                probe.status()
            ));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        let mut reclaimed = false;
        let mut buf = [0u8; 512];
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), stall.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => {
                    reclaimed = true;
                    break;
                }
                Ok(Ok(_)) => {
                    // An error status (408/400) is also a reclaim.
                    reclaimed = true;
                    break;
                }
                Err(_) => {}
            }
        }
        if !reclaimed {
            return Err(
                "a stalled partial request stayed open for 12s: enforce an idle/header timeout"
                    .to_string(),
            );
        }
        // (e) Still healthy.
        let resp = Self::fresh_client()?
            .get(format!("http://127.0.0.1:{}/", self.target_port))
            .send()
            .await
            .map_err(|e| format!("final GET: {e}"))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!("final GET / returned {}", resp.status()));
        }
        Ok(())
    }

    /// Raw-socket exchange used by the upload/hardening stages; returns the
    /// bytes read and whether the peer closed.
    async fn raw_exchange(&self, req: &str, wait: Duration) -> Result<(Vec<u8>, bool), String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = format!("127.0.0.1:{}", self.target_port);
        let mut stream = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("connect to {addr} timed out"))?
        .map_err(|e| format!("connect to {addr}: {e}"))?;
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        let deadline = std::time::Instant::now() + wait;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut closed = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(400), stream.read(&mut tmp)).await {
                Ok(Ok(0)) => {
                    closed = true;
                    break;
                }
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() > 256 * 1024 {
                        break;
                    }
                }
                Ok(Err(_)) => {
                    closed = true;
                    break;
                }
                Err(_) => {}
            }
        }
        Ok((buf, closed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Tolerant mock: answers every complete request 200, stays open on
    /// garbage/partials. Models a server that survives malformed input.
    async fn spawn_tolerant_http_mock() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut pending = Vec::new();
                    loop {
                        let n = match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        pending.extend_from_slice(&buf[..n]);
                        // One reply per header terminator seen.
                        while let Some(pos) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            pending.drain(..pos + 4);
                            if socket
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok")
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                });
            }
        });
        port
    }

    /// `fresh_client` must bound its requests: the stage-10 probes run
    /// while a stalled slow-loris connection is held, so a candidate that
    /// accepts but never answers has to time out, not hang the run.
    #[tokio::test]
    async fn test_fresh_client_bounds_a_silent_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // Accept and never answer, keeping the socket open.
            let _sock = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        let started = std::time::Instant::now();
        let resp = HttpVerifier::fresh_client()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await;
        assert!(resp.is_err(), "a silent server must not answer");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the probe must time out in ~3s, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn test_robustness_passes_on_tolerant_server() {
        let port = spawn_tolerant_http_mock().await;
        let v = HttpVerifier::new(port, Some("robust"));
        let out = v.run_robustness().await;
        assert_eq!(out.len(), 2);
        assert!(out[0].passed, "malformed: {:?}", out[0].error);
        assert!(out[1].passed, "slow headers: {:?}", out[1].error);
        assert_eq!(out[0].stage, 101);
        assert_eq!(out[1].stage, 102);
    }

    #[tokio::test]
    async fn test_robustness_fails_when_server_dies() {
        // Fragile mock: accepts once, then vanishes — the follow-up GET
        // must fail and the probe must say so (not hang, not pass).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 64];
                let _ = socket.read(&mut buf).await;
            }
        });
        let v = HttpVerifier::new(port, Some("robust"));
        let out = v.run_robustness().await;
        assert!(!out[0].passed, "dead server must fail the malformed probe");
        assert!(
            out[0].error.as_deref().unwrap_or("").contains("GET /"),
            "error must name the failed step: {:?}",
            out[0].error
        );
    }
    #[tokio::test]
    async fn test_http_verifier_all_stages_pass() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let file_store: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

        let store_clone = file_store.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let store = store_clone.clone();

                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let first_line = req.lines().next().unwrap_or("");
                        let parts: Vec<&str> = first_line.split_whitespace().collect();
                        if parts.len() < 2 {
                            break;
                        }
                        let method = parts[0];
                        let path = parts[1];

                        let response = if method == "GET" && path == "/" {
                            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nOK"
                                .to_string()
                        } else if method == "GET" && path.starts_with("/echo/") {
                            let echo_str = &path["/echo/".len()..];
                            format!(
                                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                                echo_str.len(),
                                echo_str
                            )
                        } else if method == "GET" && path == "/user-agent" {
                            let mut ua = "unknown";
                            for line in req.lines() {
                                if line.to_lowercase().starts_with("user-agent:") {
                                    ua = line["user-agent:".len()..].trim();
                                }
                            }
                            format!(
                                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                                ua.len(),
                                ua
                            )
                        } else if method == "POST" && path.starts_with("/files/") {
                            let filename = path["/files/".len()..].to_string();
                            let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                            store.lock().unwrap().insert(filename, body.to_string());
                            "HTTP/1.1 201 Created\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                                .to_string()
                        } else if method == "GET" && path.starts_with("/files/") {
                            let filename = &path["/files/".len()..];
                            if let Some(content) = store.lock().unwrap().get(filename) {
                                format!(
                                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n{}",
                                    content.len(),
                                    content
                                )
                            } else {
                                "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string()
                            }
                        } else {
                            "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string()
                        };

                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                });
            }
        });

        // Test unseeded
        let verifier = HttpVerifier::new(port, None);
        let summary = verifier.run_all().await;
        assert_eq!(
            summary.passed_count, 5,
            "Unseeded summary failed: {:?}",
            summary.stages
        );
        assert_eq!(summary.total_stages, 5);
        assert_eq!(summary.pass_rate, 100.0);

        // Test seeded
        let verifier_seeded = HttpVerifier::new(port, Some("testseed"));
        let summary_seeded = verifier_seeded.run_all().await;
        assert_eq!(
            summary_seeded.passed_count, 5,
            "Seeded summary failed: {:?}",
            summary_seeded.stages
        );
        assert_eq!(summary_seeded.total_stages, 5);
        assert_eq!(summary_seeded.pass_rate, 100.0);
    }
}

#[cfg(test)]
mod campaign_stage_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Defect switches for the full mock server.
    #[derive(Clone, Copy, PartialEq)]
    enum HttpFail {
        None,
        /// Range headers are ignored (200 + whole file).
        NoRange,
        /// Validators are missing.
        NoValidators,
        /// ETag never changes when content changes.
        EtagStable,
        /// gzip is never applied.
        NoGzip,
        /// Advertises gzip but sends raw bytes.
        BadGzip,
        /// Never sends the interim 100.
        NoContinue,
        /// Declared-oversized uploads are accepted.
        OversizedOk,
        /// Path traversal is served.
        NoTraversalGuard,
        /// Oversized heads are accepted.
        BigHeadOk,
        /// Stalled partial requests are never reclaimed.
        NeverReclaim,
    }

    struct State {
        files: HashMap<String, Vec<u8>>,
        /// Monotonic content version (drives ETag + Last-Modified).
        versions: HashMap<String, u64>,
        version_seq: u64,
        fail: HttpFail,
    }

    fn last_modified_for(v: u64) -> String {
        // Fixed base date + v seconds; always in the past for our clock.
        let secs = 1_600_000_000 + v;
        let dt = chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default();
        dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
    }

    fn etag_for(name: &str, body: &[u8], v: u64, fail: HttpFail) -> String {
        if fail == HttpFail::EtagStable {
            return "\"constant\"".to_string();
        }
        let mut h: u64 = 1469598103934665603;
        for b in body {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        format!("\"{}-{h:x}-{v}\"", name.len())
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    async fn spawn_full_http_mock(fail: HttpFail) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            files: HashMap::new(),
            versions: HashMap::new(),
            version_seq: 0,
            fail,
        }));
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    serve_full_conn(sock, state).await;
                });
            }
        });
        port
    }

    async fn serve_full_conn(mut sock: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let fail = state.lock().unwrap().fail;
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        // Read the head (bounded read timeout models slow-loris handling).
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            let read = if fail == HttpFail::NeverReclaim {
                sock.read(&mut tmp).await
            } else {
                match tokio::time::timeout(Duration::from_secs(3), sock.read(&mut tmp)).await {
                    Ok(r) => r,
                    Err(_) => return,
                }
            };
            match read {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
            if buf.len() > 8 * 1024 && fail != HttpFail::BigHeadOk {
                let _ = sock
                    .write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
            if buf.len() > 256 * 1024 {
                break;
            }
        }
        let head_end = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(p) => p + 4,
            None => return,
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or("").trim().to_string();
        let mut parts = request_line.split(' ');
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("/").to_string();
        let mut headers: HashMap<String, String> = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        if method.is_empty() || method == "GARBAGE" || !target.starts_with('/') {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
        // Path traversal guard (decoded forms included).
        let decoded = target
            .replace("%2e", ".")
            .replace("%2E", ".")
            .replace("%2f", "/")
            .replace("%2F", "/");
        if decoded.contains("..") && fail != HttpFail::NoTraversalGuard {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
        if decoded.contains("..") {
            let body = b"root:x:0:0:root:/root:/bin/bash\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body).await;
            return;
        }
        if target == "/" && method == "GET" {
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n")
                .await;
            return;
        }
        if let Some(name) = target.strip_prefix("/files/") {
            if method == "GET" {
                let found = {
                    let st = state.lock().unwrap();
                    st.files
                        .get(name)
                        .map(|b| (b.clone(), *st.versions.get(name).unwrap_or(&0)))
                };
                let (body, version) = match found {
                    Some(v) => v,
                    None => {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                };
                let etag = etag_for(name, &body, version, fail);
                let lm = last_modified_for(version);
                // Conditional GET.
                let inm = headers.get("if-none-match").cloned().unwrap_or_default();
                let ims = headers
                    .get("if-modified-since")
                    .cloned()
                    .unwrap_or_default();
                // If-Modified-Since semantics: 304 when the resource has
                // not been modified since the given (later) date.
                let parse_http_date = |v: &str| -> Option<i64> {
                    if let Ok(d) = chrono::DateTime::parse_from_rfc2822(v) {
                        return Some(d.timestamp());
                    }
                    chrono::NaiveDateTime::parse_from_str(v, "%a, %d %b %Y %H:%M:%S GMT")
                        .ok()
                        .map(|d| d.and_utc().timestamp())
                };
                let ims_fresh = parse_http_date(&ims)
                    .zip(parse_http_date(&lm))
                    .map(|(i, l)| i >= l)
                    .unwrap_or(false);
                let not_modified =
                    (fail != HttpFail::NoValidators && !inm.is_empty() && inm == etag)
                        || (fail != HttpFail::NoValidators && !ims.is_empty() && ims_fresh);
                if not_modified {
                    let head = if fail == HttpFail::NoValidators {
                        "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        format!(
                            "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nLast-Modified: {lm}\r\nConnection: close\r\n\r\n"
                        )
                    };
                    let _ = sock.write_all(head.as_bytes()).await;
                    return;
                }
                // Range handling.
                let range = headers.get("range").cloned().unwrap_or_default();
                let mut status = 200u16;
                let mut extra = String::new();
                let mut slice = body.clone();
                if fail != HttpFail::NoRange && !range.is_empty() {
                    let spec = range.trim_start_matches("bytes=");
                    let (a, b) = spec.split_once('-').unwrap_or(("", ""));
                    let len = body.len();
                    let (start, end) = if a.is_empty() {
                        // suffix
                        let n = b.parse::<usize>().unwrap_or(0);
                        (len.saturating_sub(n), len - 1)
                    } else {
                        let s = a.parse::<usize>().unwrap_or(0);
                        let e = if b.is_empty() {
                            len - 1
                        } else {
                            b.parse::<usize>().unwrap_or(0)
                        };
                        (s, e.min(len - 1))
                    };
                    if start >= len || start > end {
                        let head = format!(
                            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        return;
                    }
                    status = 206;
                    slice = body[start..=end].to_vec();
                    extra.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
                }
                // gzip negotiation (only for compressible files).
                let ae = headers.get("accept-encoding").cloned().unwrap_or_default();
                let wants_gzip = ae.to_ascii_lowercase().contains("gzip");
                let mut encoded = slice.clone();
                let mut ce = String::new();
                if wants_gzip && status == 200 && slice.len() > 1024 && fail != HttpFail::NoGzip {
                    encoded = gzip(&slice);
                    ce = "Content-Encoding: gzip\r\n".to_string();
                }
                if fail == HttpFail::BadGzip && wants_gzip && slice.len() > 1024 {
                    encoded = slice.clone();
                    ce = "Content-Encoding: gzip\r\n".to_string();
                }
                let validators = if fail == HttpFail::NoValidators {
                    String::new()
                } else {
                    format!("ETag: {etag}\r\nLast-Modified: {lm}\r\n")
                };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\n{validators}{extra}{ce}Content-Length: {len}\r\nConnection: close\r\n\r\n",
                    reason = if status == 206 { "Partial Content" } else { "OK" },
                    len = encoded.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&encoded).await;
                return;
            }
            if method == "POST" {
                // Expect: 100-continue.
                if headers
                    .get("expect")
                    .map(|v| v.to_ascii_lowercase().contains("100-continue"))
                    .unwrap_or(false)
                    && fail != HttpFail::NoContinue
                {
                    let _ = sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
                }
                let cl = headers
                    .get("content-length")
                    .and_then(|v| v.parse::<usize>().ok());
                if let Some(n) = cl {
                    if n > UPLOAD_CAP_BYTES && fail != HttpFail::OversizedOk {
                        let _ = sock
                            .write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                }
                // Read the body: chunked or content-length.
                let mut body = Vec::new();
                let chunked = headers
                    .get("transfer-encoding")
                    .map(|v| v.to_ascii_lowercase().contains("chunked"))
                    .unwrap_or(false);
                if chunked {
                    // Parse chunks from buf + socket.
                    let mut data = buf[head_end..].to_vec();
                    loop {
                        // Need a size line.
                        if !data.windows(2).any(|w| w == b"\r\n") {
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => data.extend_from_slice(&tmp[..n]),
                            }
                            continue;
                        }
                        let p = data.windows(2).position(|w| w == b"\r\n").unwrap();
                        let size_line = String::from_utf8_lossy(&data[..p]).to_string();
                        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                        if size == 0 {
                            break;
                        }
                        let start = p + 2;
                        if data.len() < start + size + 2 {
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => data.extend_from_slice(&tmp[..n]),
                            }
                            continue;
                        }
                        body.extend_from_slice(&data[start..start + size]);
                        data.drain(..start + size + 2);
                    }
                } else if let Some(n) = cl {
                    let mut have = buf[head_end..].to_vec();
                    while have.len() < n {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(m) => have.extend_from_slice(&tmp[..m]),
                        }
                    }
                    have.truncate(n);
                    body = have;
                }
                {
                    let mut st = state.lock().unwrap();
                    st.version_seq += 1;
                    let version = st.version_seq;
                    st.files.insert(name.to_string(), body);
                    st.versions.insert(name.to_string(), version);
                }
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                return;
            }
        }
        let _ = sock
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    }

    #[tokio::test]
    async fn full_mock_passes_campaign_stages() {
        let port = spawn_full_http_mock(HttpFail::None).await;
        let v = HttpVerifier::new(port, Some("mockh"));
        for st in [
            v.test_stage6_range().await,
            v.test_stage7_conditional().await,
            v.test_stage8_compression().await,
            v.test_stage9_uploads().await,
            v.test_stage10_hardening().await,
        ] {
            assert!(st.passed, "stage {} failed: {:?}", st.stage, st.error);
        }
    }

    #[tokio::test]
    async fn campaign_stage_defects_are_caught() {
        for (fail, stage, needle) in [
            (HttpFail::NoRange, 6u32, "206"),
            (HttpFail::NoValidators, 7, "ETag"),
            (HttpFail::EtagStable, 7, "did not change"),
            (HttpFail::NoGzip, 8, "gzip"),
            (HttpFail::BadGzip, 8, "decode"),
            (HttpFail::NoContinue, 9, "deadlock"),
            (HttpFail::OversizedOk, 9, "refused"),
            (HttpFail::NoTraversalGuard, 10, "traversal"),
            (HttpFail::BigHeadOk, 10, "32KiB"),
            (HttpFail::NeverReclaim, 10, "stayed open"),
        ] {
            let port = spawn_full_http_mock(fail).await;
            let v = HttpVerifier::new(port, Some("mockh"));
            let st = match stage {
                6 => v.test_stage6_range().await,
                7 => v.test_stage7_conditional().await,
                8 => v.test_stage8_compression().await,
                9 => v.test_stage9_uploads().await,
                _ => v.test_stage10_hardening().await,
            };
            assert!(
                !st.passed,
                "stage {stage} must fail for the injected defect"
            );
            let err = st.error.unwrap_or_default();
            assert!(
                err.to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase()),
                "stage {stage} error {err:?} lacks {needle:?}"
            );
        }
    }
}
