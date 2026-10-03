//! Reverse-proxy verifier: routing (stages 1-3), failover (4), throughput (5).
//! SUT: candidate HTTP/1.1 proxy on target_port; three nginx upstreams as (name, port).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::StageResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct ProxyVerifier {
    pub target_port: u16,
    pub run_seed: Option<String>,
    pub upstreams: Vec<(String, u16)>,
}

#[derive(Debug, Clone)]
struct HttpResponse {
    version: String,
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    // Used by unit tests today; stage checks read headers via parse_header_block.
    #[allow(dead_code)]
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn is_chunked(&self) -> bool {
        self.headers
            .get("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false)
    }
}

/// `HTTP/1.1 200 OK` -> ("HTTP/1.1", 200).
fn parse_status_line(line: &str) -> Option<(String, u16)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let mut parts = line.splitn(3, ' ');
    let version = parts.next()?.to_string();
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return None;
    }
    Some((version, parts.next()?.parse::<u16>().ok()?))
}

/// Header block -> lowercased-key map; malformed lines are skipped.
fn parse_header_block(block: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for line in block.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(idx) = line.find(':') {
            let name = line[..idx].trim().to_ascii_lowercase();
            if !name.is_empty() {
                headers.insert(name, line[idx + 1..].trim().to_string());
            }
        }
    }
    headers
}

/// True when the bytes so far contain a complete framed message:
/// chunked with terminal zero chunk, or Content-Length satisfied, or no
/// framing (body runs to close — only EOF completes those).
fn content_length_satisfied(buf: &[u8], resp: &HttpResponse) -> bool {
    if resp.is_chunked() {
        // parse succeeded => terminal chunk present (decode errors -> None).
        return true;
    }
    if let Some(cl) = resp.headers.get("content-length") {
        if let Ok(len) = cl.trim().parse::<usize>() {
            let head_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or(buf.len());
            return buf.len() >= head_end + 4 + len;
        }
        return false;
    }
    false
}

/// Decode chunked transfer-coding (trailers ignored after terminal chunk).
fn decode_chunked_body(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let rel = data.get(pos..)?.windows(2).position(|w| w == b"\r\n")?;
        let size_line = std::str::from_utf8(&data[pos..pos + rel]).ok()?;
        let size = usize::from_str_radix(size_line.split(';').next()?.trim(), 16).ok()?;
        pos += rel + 2;
        if size == 0 {
            return Some(out);
        }
        if data.len() < pos + size + 2 || &data[pos + size..pos + size + 2] != b"\r\n" {
            return None;
        }
        out.extend_from_slice(&data[pos..pos + size]);
        pos += size + 2;
    }
}

/// Parse a complete raw response. Content-Length bodies must be fully present,
/// chunked bodies must carry the terminal zero chunk, otherwise the body runs
/// to end-of-data (server closed the connection).
fn parse_http_response(buf: &[u8]) -> Option<HttpResponse> {
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.lines();
    let (version, status) = parse_status_line(lines.next()?)?;
    let mut raw_headers = String::new();
    for line in lines {
        raw_headers.push_str(line);
        raw_headers.push('\n');
    }
    let headers = parse_header_block(&raw_headers);
    let raw_body = &buf[head_end + 4..];
    let chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    let body = if chunked {
        decode_chunked_body(raw_body)?
    } else if let Some(cl) = headers.get("content-length") {
        let len = cl.trim().parse::<usize>().ok()?;
        if raw_body.len() < len {
            return None;
        }
        raw_body[..len].to_vec()
    } else {
        raw_body.to_vec()
    };
    Some(HttpResponse {
        version,
        status,
        headers,
        body,
    })
}

fn stage_fail(stage: u32, name: &str, err: String) -> StageResult {
    StageResult {
        stage,
        name: name.to_string(),
        passed: false,
        error: Some(err),
    }
}

fn stage_ok(stage: u32, name: &str) -> StageResult {
    StageResult {
        stage,
        name: name.to_string(),
        passed: true,
        error: None,
    }
}

impl ProxyVerifier {
    pub fn new(target_port: u16, seed: Option<&str>, upstreams: Vec<(String, u16)>) -> Self {
        Self {
            target_port,
            run_seed: seed.map(|s| s.to_string()),
            upstreams,
        }
    }

    #[allow(dead_code)]
    fn get_seed_prefix<'a>(&'a self, default_val: &'a str) -> &'a str {
        super::get_seed_prefix(self.run_seed.as_deref(), default_val)
    }

    /// Raw `GET /` against a local port; reads to EOF (Connection: close).
    async fn fetch(port: u16) -> Result<(Vec<u8>, HttpResponse), String> {
        let addr = format!("127.0.0.1:{}", port);
        let conn = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("Connection timed out to {}", addr))?;
        let mut stream = conn.map_err(|e| format!("Failed to connect to {}: {}", addr, e))?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .map_err(|e| format!("Write to {} failed: {}", addr, e))?;
        // One absolute exchange deadline: per-read timeouts restart every
        // loop, so a slow-drip response (complete headers, open connection,
        // periodic bytes) hangs verification indefinitely. Finish on HTTP
        // framing (Content-Length satisfied) or the deadline, not on EOF.
        let start_ex = Instant::now();
        let max_exchange = Duration::from_secs(8);
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let elapsed = start_ex.elapsed();
            if elapsed >= max_exchange {
                return Err(format!("Exchange with {} exceeded 8s deadline", addr));
            }
            let left = max_exchange - elapsed;
            match tokio::time::timeout(left.min(Duration::from_secs(3)), stream.read(&mut tmp))
                .await
            {
                Err(_) => {
                    // Timeout with a complete framed response is success, not
                    // failure: the server answered, it just holds the socket.
                    if let Some(resp) = parse_http_response(&buf) {
                        if content_length_satisfied(&buf, &resp) {
                            break;
                        }
                    }
                    return Err(format!("Read from {} timed out", addr));
                }
                Ok(Err(e)) => return Err(format!("Read from {} failed: {}", addr, e)),
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(resp) = parse_http_response(&buf) {
                        if content_length_satisfied(&buf, &resp) {
                            break;
                        }
                    }
                }
            }
        }
        if buf.is_empty() {
            return Err(format!("Empty response from {}", addr));
        }
        let resp = parse_http_response(&buf)
            .ok_or_else(|| format!("Failed to parse HTTP response from {}", addr))?;
        Ok((buf, resp))
    }

    /// Direct-GET each upstream to learn its body marker.
    async fn learn_markers(&self) -> Result<Vec<String>, String> {
        if self.upstreams.len() < 3 {
            return Err(format!("Need 3 upstreams, have {}", self.upstreams.len()));
        }
        let mut markers = Vec::new();
        for (_, port) in &self.upstreams {
            let (_, resp) = Self::fetch(*port).await?;
            if resp.status != 200 {
                return Err(format!(
                    "Upstream 127.0.0.1:{} returned {}",
                    port, resp.status
                ));
            }
            markers.push(resp.body_text());
        }
        if markers.iter().any(|m| m.is_empty()) {
            return Err("Upstream returned empty body".to_string());
        }
        Ok(markers)
    }

    fn body_marker(body: &str, markers: &[String]) -> Option<usize> {
        markers.iter().position(|m| body.contains(m))
    }

    pub async fn run_routing(&self) -> ProxyTestSummary {
        let stages = vec![
            self.test_stage1().await,
            self.test_stage2().await,
            self.test_stage3().await,
        ];
        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;
        ProxyTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    async fn test_stage1(&self) -> StageResult {
        // NOTE: X-Forwarded-For goes *upstream* (client -> proxy -> upstream)
        // and stock nginx never reflects it downstream, so no black-box
        // response assertion can observe it. It stays a prompt requirement
        // enforced by code review; here we assert status + marker fidelity.
        let name = "Stage 1: Basic routing (200 + marker)";
        let markers = match self.learn_markers().await {
            Ok(m) => m,
            Err(e) => return stage_fail(1, name, format!("Upstream markers: {}", e)),
        };
        let (_, resp) = match Self::fetch(self.target_port).await {
            Ok(r) => r,
            Err(e) => return stage_fail(1, name, format!("GET / via proxy: {}", e)),
        };
        if resp.status != 200 {
            return stage_fail(1, name, format!("Expected 200, got {}", resp.status));
        }
        if Self::body_marker(&resp.body_text(), &markers).is_none() {
            return stage_fail(1, name, "Body matches no upstream marker".to_string());
        }
        stage_ok(1, name)
    }

    async fn test_stage2(&self) -> StageResult {
        let name = "Stage 2: Round-robin across all 3 upstreams";
        let markers = match self.learn_markers().await {
            Ok(m) => m,
            Err(e) => return stage_fail(2, name, format!("Upstream markers: {}", e)),
        };
        let mut hits = [0u32; 3];
        for i in 0..30 {
            let (_, resp) = match Self::fetch(self.target_port).await {
                Ok(r) => r,
                Err(e) => return stage_fail(2, name, format!("GET {}: {}", i, e)),
            };
            if resp.status != 200 {
                return stage_fail(2, name, format!("GET {}: got {}", i, resp.status));
            }
            match Self::body_marker(&resp.body_text(), &markers) {
                Some(idx) => hits[idx] += 1,
                None => return stage_fail(2, name, format!("GET {} matched no upstream", i)),
            }
        }
        if hits.iter().any(|&h| h < 5) {
            return stage_fail(2, name, format!("Uneven spread, hits: {:?}", hits));
        }
        stage_ok(2, name)
    }

    async fn test_stage3(&self) -> StageResult {
        let name = "Stage 3: Valid HTTP/1.1 framing";
        let (raw, resp) = match Self::fetch(self.target_port).await {
            Ok(r) => r,
            Err(e) => return stage_fail(3, name, format!("GET / via proxy: {}", e)),
        };
        if !raw.starts_with(b"HTTP/1.1 ") || resp.version != "HTTP/1.1" {
            return stage_fail(3, name, "Status line is not HTTP/1.1".to_string());
        }
        if !resp.headers.contains_key("content-length") && !resp.is_chunked() {
            return stage_fail(3, name, "No Content-Length or chunked framing".to_string());
        }
        stage_ok(3, name)
    }

    pub async fn run_failover(&self) -> StageResult {
        let name = "Stage 4: Failover (one upstream down)";
        if self.upstreams.len() < 3 {
            let n = self.upstreams.len();
            return stage_fail(4, name, format!("Need 3 upstreams, have {}", n));
        }
        let mid_name = self.upstreams[1].0.clone();
        let mid_port = self.upstreams[1].1;
        match std::process::Command::new("docker")
            .arg("stop")
            .arg(&mid_name)
            .output()
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let detail = String::from_utf8_lossy(&o.stderr);
                return stage_fail(4, name, format!("docker stop {}: {}", mid_name, detail));
            }
            Err(e) => return stage_fail(4, name, format!("docker stop {}: {}", mid_name, e)),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut burst_err: Option<String> = None;
        for i in 0..20 {
            match Self::fetch(self.target_port).await {
                Ok((_, resp)) if resp.status == 200 => {}
                Ok((_, resp)) => {
                    burst_err = Some(format!("GET {} during failover: got {}", i, resp.status));
                    break;
                }
                Err(e) => {
                    burst_err = Some(format!("GET {} during failover: {}", i, e));
                    break;
                }
            }
        }
        let _ = std::process::Command::new("docker")
            .arg("start")
            .arg(&mid_name)
            .output();
        if let Some(e) = burst_err {
            return stage_fail(4, name, format!("Proxy must fail over, {}", e));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let healthy = Self::fetch(mid_port)
                .await
                .map(|(_, r)| r.status == 200)
                .unwrap_or(false);
            if healthy {
                break;
            }
            if Instant::now() >= deadline {
                return stage_fail(4, name, format!("{} did not recover in 10s", mid_name));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        stage_ok(4, name)
    }

    pub async fn measure_throughput(&self) -> Option<f64> {
        use std::sync::Arc;
        use tokio::sync::Semaphore;
        let sem = Arc::new(Semaphore::new(20));
        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..200 {
            let sem = sem.clone();
            let port = self.target_port;
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.ok()?;
                let (_, resp) = Self::fetch(port).await.ok()?;
                (resp.status == 200).then_some(())
            }));
        }
        let mut ok = 0u32;
        for h in handles {
            if let Ok(Some(())) = h.await {
                ok += 1;
            }
        }
        if ok < 180 {
            return None;
        }
        let secs = start.elapsed().as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        Some(ok as f64 / secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_ok_and_rejected() {
        let ok = |s: &str| parse_status_line(s);
        assert_eq!(ok("HTTP/1.1 200 OK"), Some(("HTTP/1.1".into(), 200)));
        assert_eq!(
            ok("HTTP/1.1 502 Bad Gateway\r"),
            Some(("HTTP/1.1".into(), 502))
        );
        assert_eq!(ok("HTTP/1.0 200 OK"), Some(("HTTP/1.0".into(), 200)));
        assert_eq!(ok("NOTHTTP 200 OK"), None);
        assert_eq!(ok("HTTP/1.1 OK"), None);
        assert_eq!(ok("HTTP/2 200 OK"), None);
        assert_eq!(ok(""), None);
    }

    #[test]
    fn headers_case_insensitive_and_trimmed() {
        let map = parse_header_block("Content-Length:  5\r\nX-Forwarded-For: 1.2.3.4 \r\n");
        assert_eq!(map.get("content-length").map(String::as_str), Some("5"));
        let fwd = map.get("x-forwarded-for").map(String::as_str);
        assert_eq!(fwd, Some("1.2.3.4"));
        let map = parse_header_block("no-colon-here\r\n: empty-name\r\nA:b:c\r\n");
        assert!(!map.contains_key("no-colon-here") && !map.contains_key(""));
        assert_eq!(map.get("a").map(String::as_str), Some("b:c"));
    }

    #[test]
    fn content_length_body_and_truncation() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-FF: 9.9.9.9\r\n\r\nhelloEXTRA";
        let resp = parse_http_response(raw).expect("must parse");
        assert_eq!((resp.version.as_str(), resp.status), ("HTTP/1.1", 200));
        assert_eq!(resp.body, b"hello");
        assert_eq!(resp.header("X-FF"), Some("9.9.9.9"));
        let short = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        assert!(parse_http_response(short).is_none());
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n").is_none());
    }

    #[test]
    fn chunked_decode_with_extensions_and_failures() {
        let two = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked_body(two).unwrap(), b"hello world");
        let ext = b"5;ext=x\r\nhello\r\n0\r\n\r\n";
        assert_eq!(decode_chunked_body(ext).unwrap(), b"hello");
        assert!(decode_chunked_body(b"5\r\nhello\r\n").is_none());
        assert!(decode_chunked_body(b"zz\r\nhello\r\n0\r\n\r\n").is_none());
        assert!(decode_chunked_body(b"5\r\nhelloXX0\r\n\r\n").is_none());
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(raw).expect("must parse").body, b"hello");
    }

    #[test]
    fn close_delimited_body_runs_to_eof() {
        let raw = b"HTTP/1.1 200 OK\r\nX-Marker: a\r\n\r\nclose-delimited-body";
        let resp = parse_http_response(raw).expect("must parse");
        assert_eq!(resp.body, b"close-delimited-body");
    }

    /// Stub-must-not-pass: a "proxy" that answers 200 with a static body
    /// matching no upstream marker scores 0 on stage 1. `body_marker`
    /// returns None for exactly this shape; stage 1 fails on it.
    #[test]
    fn test_fake_proxy_static_body_matches_no_marker() {
        let markers = vec!["upstream-alpha".to_string(), "upstream-beta".to_string()];
        assert_eq!(ProxyVerifier::body_marker("static hello", &markers), None);
        assert_eq!(
            ProxyVerifier::body_marker("served by upstream-alpha", &markers),
            Some(0)
        );
    }

    /// Stage-level: a static-body stub fails routing against live markers.
    #[tokio::test]
    async fn test_fake_proxy_fails_stage1() {
        use tokio::net::TcpListener;
        async fn serve_static(listener: TcpListener) {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let body = b"static stub";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(&head.into_bytes()).await;
                    let _ = s.write_all(body).await;
                });
            }
        }
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = up.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = up.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let body = b"real-upstream-marker";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(&head.into_bytes()).await;
                    let _ = s.write_all(body).await;
                });
            }
        });
        let stub = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stub_port = stub.local_addr().unwrap().port();
        tokio::spawn(serve_static(stub));
        let v = ProxyVerifier::new(
            stub_port,
            None,
            vec![
                ("up".to_string(), up_port),
                ("up2".to_string(), up_port),
                ("up3".to_string(), up_port),
            ],
        );
        let r = v.test_stage1().await;
        assert!(
            !r.passed,
            "static-body stub must fail routing: {:?}",
            r.error
        );
    }
}
