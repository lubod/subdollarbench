//! Reverse-proxy verifier: routing (stages 1-3), failover (4), throughput (5)
//! plus the sdb-edge campaign stages (6 fast failover under a hung upstream,
//! 7 active health checks + /status, 8 caching, 9 limits/hardening), which
//! run only when a ticket's acceptance includes them.
//! SUT: candidate HTTP/1.1 proxy on target_port; three nginx upstreams as (name, port).

/// Cache window used by the caching stage and the env the pipeline sets
/// (`SDB_PROXY_CACHE_TTL`); the candidate and the verifier must agree.
pub const CACHE_TTL_SECS: u64 = 3;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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

/// `upstreamN: up|down` lines from a /status body -> 1-based index -> up.
fn parse_status_lines(body: &str) -> HashMap<usize, bool> {
    let mut out = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().to_ascii_lowercase();
            if let Some(n) = key.trim().strip_prefix("upstream") {
                if let Ok(idx) = n.parse::<usize>() {
                    if value == "up" {
                        out.insert(idx, true);
                    } else if value == "down" {
                        out.insert(idx, false);
                    }
                }
            }
        }
    }
    out
}

/// `X-Cache` value, upper-cased (`HIT`/`MISS`).
fn x_cache_of(resp: &HttpResponse) -> Option<String> {
    resp.headers
        .get("x-cache")
        .map(|v| v.trim().to_ascii_uppercase())
}

/// First `HTTP/x.y NNN` status code in a raw buffer.
fn status_code_of(buf: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(buf);
    let line = text.lines().next()?;
    line.split_whitespace().nth(1)?.parse::<u16>().ok()
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
        Self::fetch_path(port, "/").await
    }

    /// Same contract as `fetch`, for an arbitrary request target (the
    /// `/status` health surface, hardening probes).
    async fn fetch_path(port: u16, path: &str) -> Result<(Vec<u8>, HttpResponse), String> {
        let addr = format!("127.0.0.1:{}", port);
        let conn = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("Connection timed out to {}", addr))?;
        let mut stream = conn.map_err(|e| format!("Failed to connect to {}: {}", addr, e))?;
        let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
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
        // Primary: the docker pool (unchanged healthy path — same strings,
        // same verdicts as always).
        let mut docker_err = None;
        let mut markers = Vec::new();
        let mut docker_ok = true;
        for (_, port) in &self.upstreams {
            match Self::fetch(*port).await {
                Ok((_, resp)) => {
                    if resp.status != 200 {
                        docker_ok = false;
                        docker_err = Some(format!(
                            "Upstream 127.0.0.1:{} returned {}",
                            port, resp.status
                        ));
                        break;
                    }
                    markers.push(resp.body_text());
                }
                Err(e) => {
                    docker_ok = false;
                    docker_err = Some(e);
                    break;
                }
            }
        }
        if docker_ok && !markers.iter().any(|m| m.is_empty()) {
            return Ok(markers);
        }
        // Fallback: host-published docker fixtures decay under daemon churn
        // (measured: Ports:null + refused at verify despite clean starts),
        // while the containers keep serving in-network aliases. Distinguish
        // the two before degrading: a dead container is a harness fault with
        // the culprit named; a live-but-unpublished one degrades to
        // identical in-process stubs (byte-identical bodies via the shared
        // const — attribution unaffected).
        let mut dead = Vec::new();
        for (name, _) in &self.upstreams {
            if !Self::upstream_running(name) {
                dead.push(name.clone());
            }
        }
        if !dead.is_empty() {
            return Err(format!(
                "harness fixture fault: upstream container(s) not running: {}",
                dead.join(", ")
            ));
        }
        eprintln!(
            "[FIXTURE] host publish decayed ({}); markers from identical in-process stubs",
            docker_err.unwrap_or_default()
        );
        Self::learn_markers_local().await
    }

    /// True when the named container exists and is running (daemon-local
    /// inspect — immune to the publish decay that breaks host dials).
    fn upstream_running(name: &str) -> bool {
        std::process::Command::new("docker")
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false)
    }

    /// Marker bodies without docker: one ephemeral stub per upstream,
    /// serving the byte-identical bodies the nginx pool serves (shared
    /// const — divergence is a compile-time-visible choice, not drift).
    async fn learn_markers_local() -> Result<Vec<String>, String> {
        let mut markers = Vec::new();
        let mut guards = Vec::new();
        for idx in 1u8..=3u8 {
            let body =
                crate::sandbox::docker::SandboxManager::upstream_marker_body(idx).into_bytes();
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("stub bind failed: {}", e))?;
            let port = listener
                .local_addr()
                .map_err(|e| format!("stub addr failed: {}", e))?
                .port();
            guards.push(tokio::spawn(async move {
                loop {
                    let Ok((mut s, _)) = listener.accept().await else {
                        return;
                    };
                    let body = body.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 1024];
                        let _ = s.read(&mut buf).await;
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = s.write_all(&head.into_bytes()).await;
                        let _ = s.write_all(&body).await;
                    });
                }
            }));
            let (_, resp) = Self::fetch(port).await?;
            if resp.status != 200 {
                for g in guards {
                    g.abort();
                }
                return Err(format!("local stub {} returned {}", port, resp.status));
            }
            markers.push(resp.body_text());
        }
        for g in guards {
            g.abort();
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
        let name = "Stage 3: Valid HTTP/1.1 framing + keep-alive";
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
        // Keep-alive: 3 sequential GETs on ONE connection must all 200.
        // A stub that closes every connection (or answers once then hangs)
        // fails here; Connection: close per-request still passes stage 1-2.
        match Self::fetch_keepalive(self.target_port, 3).await {
            Ok(3) => stage_ok(3, name),
            Ok(n) => stage_fail(
                3,
                name,
                format!("keep-alive: {n}/3 sequential 200s on one connection"),
            ),
            Err(e) => stage_fail(3, name, format!("keep-alive: {}", e)),
        }
    }

    /// 3 sequential `GET /` on one `Connection: keep-alive` socket, counting
    /// 200s by framed Content-Length boundaries (no EOF reliance).
    async fn fetch_keepalive(port: u16, n: usize) -> Result<usize, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let start = Instant::now();
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
                        if let Some(resp) = parse_http_response(&buf) {
                            if content_length_satisfied(&buf, &resp) {
                                break;
                            }
                        }
                    }
                }
            }
            match parse_http_response(&buf) {
                Some(r) if r.status == 200 => ok += 1,
                Some(r) => return Err(format!("keep-alive GET got {}", r.status)),
                None => return Err("keep-alive response unparseable".to_string()),
            }
        }
        Ok(ok)
    }

    pub async fn run_failover(&self) -> StageResult {
        let name = "Stage 4: Failover (one upstream down)";
        if self.upstreams.len() < 3 {
            let n = self.upstreams.len();
            return stage_fail(4, name, format!("Need 3 upstreams, have {}", n));
        }
        // Markers for the rejoin check: learned in-process (deterministic),
        // never host-dialed.
        let markers = match Self::learn_markers_local().await {
            Ok(m) => m,
            Err(e) => return stage_fail(4, name, format!("fixture markers: {}", e)),
        };
        let mid_name = self.upstreams[1].0.clone();
        // Fixture pre-check: killing a dead container proves nothing and the
        // failure that follows would misattribute. Name the culprit.
        if !Self::upstream_running(&mid_name) {
            return stage_fail(
                4,
                name,
                format!("harness fixture fault: {} not running pre-kill", mid_name),
            );
        }
        // Pause (not stop): freezing hung-connections tests slow-failover
        // too, dodges stop/start re-publish races, and unpauses instantly —
        // no 10s nginx boot wait, no removal races with concurrent sweeps.
        match std::process::Command::new("docker")
            .arg("pause")
            .arg(&mid_name)
            .output()
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let detail = String::from_utf8_lossy(&o.stderr);
                return stage_fail(4, name, format!("docker pause {}: {}", mid_name, detail));
            }
            Err(e) => return stage_fail(4, name, format!("docker pause {}: {}", mid_name, e)),
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
            .arg("unpause")
            .arg(&mid_name)
            .output();
        if let Some(e) = burst_err {
            return stage_fail(4, name, format!("Proxy must fail over, {}", e));
        }
        // Rejoin proven end-to-end (via proxy), not by direct dial: the
        // direct path is the decay-prone one, and rejoining rotation is the
        // scored behavior. Same 10s backstop.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rejoined = match Self::fetch(self.target_port).await {
                Ok((_, resp)) => {
                    resp.status == 200 && Self::body_marker(&resp.body_text(), &markers) == Some(1)
                }
                Err(_) => false,
            };
            if rejoined {
                break;
            }
            if Instant::now() >= deadline {
                return stage_fail(
                    4,
                    name,
                    format!("{} did not rejoin rotation in 10s", mid_name),
                );
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

    /// docker pause/unpause a fixture upstream (stage 4 pattern; frozen
    /// process, TCP still accepting = the hung-upstream case).
    fn upstream_pause(name: &str, pause: bool) -> Result<(), String> {
        let verb = if pause { "pause" } else { "unpause" };
        match std::process::Command::new("docker")
            .arg(verb)
            .arg(name)
            .output()
        {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!(
                "docker {} {}: {}",
                verb,
                name,
                String::from_utf8_lossy(&o.stderr)
            )),
            Err(e) => Err(format!("docker {} {}: {}", verb, name, e)),
        }
    }

    /// GET /status and parse the per-upstream health lines.
    async fn status_snapshot(port: u16) -> Result<HashMap<usize, bool>, String> {
        let (_, resp) = Self::fetch_path(port, "/status").await?;
        if resp.status != 200 {
            return Err(format!("/status returned {}", resp.status));
        }
        let map = parse_status_lines(&resp.body_text());
        if !map.contains_key(&1) {
            let preview: String = resp.body_text().chars().take(120).collect();
            return Err(format!(
                "/status has no `upstreamN: up|down` lines (body: {:?})",
                preview
            ));
        }
        Ok(map)
    }

    /// Write `req`, then read until close or `wait` elapses; returns the
    /// bytes received and whether the peer closed.
    async fn raw_exchange(
        port: u16,
        req: &[u8],
        wait: Duration,
    ) -> Result<(Vec<u8>, bool), String> {
        let addr = format!("127.0.0.1:{}", port);
        let mut stream = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("connect to {} timed out", addr))?
            .map_err(|e| format!("connect to {}: {}", addr, e))?;
        stream
            .write_all(req)
            .await
            .map_err(|e| format!("write to {}: {}", addr, e))?;
        let deadline = Instant::now() + wait;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut closed = false;
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut tmp)).await {
                Ok(Ok(0)) => {
                    closed = true;
                    break;
                }
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() > 512 * 1024 {
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

    /// Stage 6 (campaign sdb-edge): a hung upstream (process frozen, TCP
    /// still accepting) must not hold clients — every request during the
    /// pause returns 200 inside the per-request budget (timeout + retry),
    /// and the upstream rejoins after it thaws.
    pub async fn test_stage6_quick_failover(&self) -> StageResult {
        let name = "Stage 6: Fast failover under a hung upstream";
        if self.upstreams.len() < 3 {
            let n = self.upstreams.len();
            return stage_fail(6, name, format!("Need 3 upstreams, have {}", n));
        }
        const REQUESTS: usize = 10;
        const BUDGET_MS: u128 = 2500;
        let mid_name = self.upstreams[1].0.clone();
        if !Self::upstream_running(&mid_name) {
            return stage_fail(
                6,
                name,
                format!("harness fixture fault: {} not running pre-pause", mid_name),
            );
        }
        let markers = match Self::learn_markers_local().await {
            Ok(m) => m,
            Err(e) => return stage_fail(6, name, format!("fixture markers: {}", e)),
        };
        if let Err(e) = Self::upstream_pause(&mid_name, true) {
            return stage_fail(6, name, e);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut worst_ms: u128 = 0;
        let mut failure: Option<String> = None;
        for i in 0..REQUESTS {
            let started = Instant::now();
            match Self::fetch(self.target_port).await {
                Ok((_, resp)) if resp.status == 200 => {
                    worst_ms = worst_ms.max(started.elapsed().as_millis());
                }
                Ok((_, resp)) => {
                    failure = Some(format!("GET {} during the pause: got {}", i, resp.status));
                    break;
                }
                Err(e) => {
                    failure = Some(format!("GET {} during the pause: {}", i, e));
                    break;
                }
            }
        }
        let _ = Self::upstream_pause(&mid_name, false);
        if let Some(e) = failure {
            return stage_fail(6, name, format!("Proxy must fail over, {}", e));
        }
        if worst_ms > BUDGET_MS {
            return stage_fail(
                6,
                name,
                format!(
                    "worst request took {}ms during the hang (budget {}ms): time out the hung upstream and retry, do not wait for the client deadline",
                    worst_ms, BUDGET_MS
                ),
            );
        }
        // Rejoin proven through the proxy after the thaw.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rejoined = match Self::fetch(self.target_port).await {
                Ok((_, resp)) => {
                    resp.status == 200 && Self::body_marker(&resp.body_text(), &markers) == Some(1)
                }
                Err(_) => false,
            };
            if rejoined {
                return stage_ok(6, name);
            }
            if Instant::now() >= deadline {
                return stage_fail(
                    6,
                    name,
                    format!("{} did not rejoin rotation in 10s", mid_name),
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Stage 7 (campaign sdb-edge): the proxy actively probes its upstreams
    /// and exposes per-upstream health at `GET /status` (`upstreamN:
    /// up|down`). Health must flip DOWN while the proxy is idle (probes,
    /// not passive client-traffic inference) and back UP after recovery.
    pub async fn test_stage7_health_status(&self) -> StageResult {
        let name = "Stage 7: Active health checks + /status";
        if self.upstreams.len() < 3 {
            let n = self.upstreams.len();
            return stage_fail(7, name, format!("Need 3 upstreams, have {}", n));
        }
        let mid_name = self.upstreams[1].0.clone();
        if !Self::upstream_running(&mid_name) {
            return stage_fail(
                7,
                name,
                format!("harness fixture fault: {} not running pre-pause", mid_name),
            );
        }
        let markers = match Self::learn_markers_local().await {
            Ok(m) => m,
            Err(e) => return stage_fail(7, name, format!("fixture markers: {}", e)),
        };
        // 1. All three healthy within 6s (initial probe cycle).
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            match Self::status_snapshot(self.target_port).await {
                Ok(map) if (1..=3).all(|i| map.get(&i) == Some(&true)) => break,
                Ok(map) => {
                    if Instant::now() >= deadline {
                        return stage_fail(
                            7,
                            name,
                            format!("/status never reported all upstreams up: {:?}", map),
                        );
                    }
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return stage_fail(7, name, format!("GET /status: {}", e));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // 2. Pause the middle upstream; stay idle (health must come from
        //    active probes, not from client requests failing).
        if let Err(e) = Self::upstream_pause(&mid_name, true) {
            return stage_fail(7, name, e);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match Self::status_snapshot(self.target_port).await {
                Ok(map) if map.get(&2) == Some(&false) => break,
                Ok(map) => {
                    if Instant::now() >= deadline {
                        return stage_fail(
                            7,
                            name,
                            format!(
                                "{} was down for 5s of idle but /status still says {:?} — probe upstreams actively",
                                mid_name, map
                            ),
                        );
                    }
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return stage_fail(7, name, format!("GET /status: {}", e));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // 3. Recovery: unpause, /status flips back up, traffic rejoins.
        if let Err(e) = Self::upstream_pause(&mid_name, false) {
            return stage_fail(7, name, e);
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            match Self::status_snapshot(self.target_port).await {
                Ok(map) if map.get(&2) == Some(&true) => break,
                Ok(map) => {
                    if Instant::now() >= deadline {
                        return stage_fail(
                            7,
                            name,
                            format!(
                                "/status still reports {} down after recovery: {:?}",
                                mid_name, map
                            ),
                        );
                    }
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return stage_fail(7, name, format!("GET /status: {}", e));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rejoined = match Self::fetch(self.target_port).await {
                Ok((_, resp)) => {
                    resp.status == 200 && Self::body_marker(&resp.body_text(), &markers) == Some(1)
                }
                Err(_) => false,
            };
            if rejoined {
                return stage_ok(7, name);
            }
            if Instant::now() >= deadline {
                return stage_fail(
                    7,
                    name,
                    format!("{} did not rejoin rotation in 10s after recovery", mid_name),
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Stage 8 (campaign sdb-edge; the pipeline restarts the candidate with
    /// `SDB_PROXY_CACHE_TTL` set): repeated GETs inside the TTL are served
    /// from cache — `X-Cache: MISS` on fetch, `HIT` + `Age` on hit, bodies
    /// byte-identical to their MISS twin — and entries expire after ~TTL.
    pub async fn test_stage8_caching(&self) -> StageResult {
        self.test_stage8_caching_with(CACHE_TTL_SECS).await
    }

    /// Same stage with an explicit TTL (tests scale the window down).
    pub(crate) async fn test_stage8_caching_with(&self, ttl_secs: u64) -> StageResult {
        let name = "Stage 8: Response caching (TTL + X-Cache)";
        let markers = match Self::learn_markers_local().await {
            Ok(m) => m,
            Err(e) => return stage_fail(8, name, format!("fixture markers: {}", e)),
        };
        let mut miss_bodies: HashMap<usize, Vec<u8>> = HashMap::new();
        let mut hit_with_age = false;
        for _ in 0..24 {
            let (_, resp) = match Self::fetch(self.target_port).await {
                Ok(r) => r,
                Err(e) => return stage_fail(8, name, format!("GET via proxy: {}", e)),
            };
            if resp.status != 200 {
                return stage_fail(8, name, format!("GET via proxy returned {}", resp.status));
            }
            let marker = match Self::body_marker(&resp.body_text(), &markers) {
                Some(m) => m,
                None => return stage_fail(8, name, "body matched no upstream marker".to_string()),
            };
            let xc = match x_cache_of(&resp) {
                Some(v) => v,
                None => {
                    return stage_fail(
                        8,
                        name,
                        "response carries no X-Cache header (MISS on fetch, HIT on cache hits)"
                            .to_string(),
                    )
                }
            };
            match xc.as_str() {
                "MISS" => {
                    miss_bodies
                        .entry(marker)
                        .or_insert_with(|| resp.body.clone());
                }
                "HIT" => {
                    if let Some(miss_body) = miss_bodies.get(&marker) {
                        if miss_body != &resp.body {
                            return stage_fail(
                                8,
                                name,
                                format!(
                                    "cache HIT body for upstream {} differs from its MISS body",
                                    marker + 1
                                ),
                            );
                        }
                        match resp
                            .headers
                            .get("age")
                            .and_then(|v| v.trim().parse::<u64>().ok())
                        {
                            Some(_) => hit_with_age = true,
                            None => {
                                return stage_fail(
                                    8,
                                    name,
                                    "cache HIT carries no numeric Age header".to_string(),
                                )
                            }
                        }
                    }
                }
                other => {
                    return stage_fail(
                        8,
                        name,
                        format!("X-Cache must be MISS or HIT, got {:?}", other),
                    )
                }
            }
            if hit_with_age {
                break;
            }
        }
        if !hit_with_age {
            return stage_fail(
                8,
                name,
                format!(
                    "no repeat request inside the {}s TTL was served from cache (need X-Cache: HIT + Age)",
                    ttl_secs
                ),
            );
        }
        // Entries must expire: after the window a request refetches (MISS).
        tokio::time::sleep(Duration::from_millis(ttl_secs * 1000 + 1500)).await;
        for _ in 0..4 {
            match Self::fetch(self.target_port).await {
                Ok((_, resp)) if resp.status == 200 => {
                    if x_cache_of(&resp).as_deref() == Some("MISS") {
                        return stage_ok(8, name);
                    }
                }
                Ok((_, resp)) => {
                    return stage_fail(8, name, format!("post-TTL GET returned {}", resp.status))
                }
                Err(e) => return stage_fail(8, name, format!("post-TTL GET: {}", e)),
            }
        }
        stage_fail(
            8,
            name,
            format!(
                "entries never expired: no MISS within {}s+ of the {}s TTL",
                ttl_secs, ttl_secs
            ),
        )
    }

    /// Stage 9 (campaign sdb-edge; the pipeline restarts the candidate
    /// under a 128MB cap): hostile input must be rejected cheaply — an
    /// oversized header block answers 4xx (or closes), an oversized
    /// declared body answers 413/close before the bytes arrive, a stalled
    /// half-request is reclaimed (slow-loris), and normal traffic keeps
    /// working throughout.
    pub async fn test_stage9_limits(&self) -> StageResult {
        let name = "Stage 9: Limits and hostile input";
        // (a) Oversized header block: 4xx or a clean close, within 5s.
        let big_header = format!(
            "GET / HTTP/1.1\r\nHost: x\r\nX-Big: {}\r\n\r\n",
            "a".repeat(32 * 1024)
        );
        match Self::raw_exchange(
            self.target_port,
            big_header.as_bytes(),
            Duration::from_secs(5),
        )
        .await
        {
            Ok((buf, closed)) => {
                let code = status_code_of(&buf);
                let ok =
                    matches!(code, Some(400) | Some(413) | Some(431)) || (closed && buf.is_empty());
                if !ok {
                    return stage_fail(
                        9,
                        name,
                        format!(
                            "32KiB of headers got {:?} (closed={}): answer 4xx (431) or close without buffering",
                            code, closed
                        ),
                    );
                }
            }
            Err(e) => return stage_fail(9, name, format!("oversized-header probe: {}", e)),
        }
        // (b) Oversized declared body: reject before the bytes arrive.
        let big_body = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 100000000\r\n\r\n";
        match Self::raw_exchange(
            self.target_port,
            big_body.as_bytes(),
            Duration::from_secs(5),
        )
        .await
        {
            Ok((buf, closed)) => {
                let code = status_code_of(&buf);
                let ok = matches!(code, Some(400) | Some(411) | Some(413) | Some(417))
                    || (closed && buf.is_empty());
                if !ok {
                    return stage_fail(
                        9,
                        name,
                        format!(
                            "a declared 100MB body got {:?} (closed={}): answer 413 before reading the body",
                            code, closed
                        ),
                    );
                }
            }
            Err(e) => return stage_fail(9, name, format!("oversized-body probe: {}", e)),
        }
        // (c) Slow-loris: hold a half-request open; the proxy must keep
        // serving normal clients and reclaim the stalled socket.
        let addr = format!("127.0.0.1:{}", self.target_port);
        let mut stall =
            match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await {
                Err(_) => return stage_fail(9, name, format!("connect to {} timed out", addr)),
                Ok(Err(e)) => return stage_fail(9, name, format!("connect to {}: {}", addr, e)),
                Ok(Ok(s)) => s,
            };
        if let Err(e) = stall.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await {
            return stage_fail(9, name, format!("stall write: {}", e));
        }
        // Normal traffic must keep flowing while the stalled socket idles.
        match Self::fetch(self.target_port).await {
            Ok((_, resp)) if resp.status == 200 => {}
            Ok((_, resp)) => {
                return stage_fail(
                    9,
                    name,
                    format!(
                        "normal GET while a slow client idled returned {}",
                        resp.status
                    ),
                )
            }
            Err(e) => {
                return stage_fail(
                    9,
                    name,
                    format!("normal GET while a slow client idled: {}", e),
                )
            }
        }
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut reclaimed = false;
        let mut tmp = [0u8; 1024];
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), stall.read(&mut tmp)).await {
                Ok(Ok(0)) | Ok(Err(_)) => {
                    reclaimed = true;
                    break;
                }
                Ok(Ok(n)) => {
                    if matches!(status_code_of(&tmp[..n]), Some(400) | Some(408)) {
                        reclaimed = true;
                        break;
                    }
                }
                Err(_) => {}
            }
        }
        if !reclaimed {
            return stage_fail(
                9,
                name,
                "a half-sent request stayed open for 12s: enforce a header/read timeout (slow-loris)"
                    .to_string(),
            );
        }
        // (d) Still healthy after all of it.
        match Self::fetch(self.target_port).await {
            Ok((_, resp)) if resp.status == 200 => stage_ok(9, name),
            Ok((_, resp)) => stage_fail(
                9,
                name,
                format!("GET after the hostile probes returned {}", resp.status),
            ),
            Err(e) => stage_fail(9, name, format!("GET after the hostile probes: {}", e)),
        }
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
    fn test_marker_bodies_match_docker_pool() {
        // Byte-identical contract between the nginx pool (docker.rs) and
        // the in-process fallback stubs: attribution depends on it.
        for i in 1u8..=3u8 {
            assert_eq!(
                crate::sandbox::docker::SandboxManager::upstream_marker_body(i),
                format!("upstream-{} marker\n", i)
            );
        }
    }

    #[tokio::test]
    async fn test_learn_markers_local_serves_canonical_bodies_in_order() {
        let markers = ProxyVerifier::learn_markers_local()
            .await
            .expect("in-process stubs must serve");
        assert_eq!(
            markers,
            vec![
                "upstream-1 marker\n".to_string(),
                "upstream-2 marker\n".to_string(),
                "upstream-3 marker\n".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_learn_markers_names_dead_fixtures() {
        // Closed ports + absent containers: must fail LOUD naming the
        // culprit (harness fault), never silently misattribute. Deterministic
        // without docker: inspect fails closed either way.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let v = ProxyVerifier::new(
            9,
            None,
            vec![
                ("no-such-upstream-a".to_string(), dead_port),
                ("no-such-upstream-b".to_string(), dead_port),
                ("no-such-upstream-c".to_string(), dead_port),
            ],
        );
        let err = v.learn_markers().await.expect_err("must fail");
        assert!(
            err.contains("harness fixture fault") && err.contains("no-such-upstream-a"),
            "must name the dead fixture: {err}"
        );
    }

    /// Integration: the production decay shape — containers RUNNING but host
    /// ports refused (publish decay). learn_markers must degrade to identical
    /// in-process markers instead of failing. Requires docker.
    #[tokio::test]
    #[ignore = "requires docker; decay simulation with unpublished nginx"]
    async fn test_learn_markers_falls_back_on_unpublished_containers() {
        for (i, name) in ["sdb-ut-up1", "sdb-ut-up2", "sdb-ut-up3"]
            .iter()
            .enumerate()
        {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", name])
                .output();
            // NOTE: deliberately no -p publish (the decay shape).
            let out = std::process::Command::new("docker")
                .args(["run", "-d", "--name", name, "nginx:alpine"])
                .output()
                .expect("docker run");
            assert!(out.status.success(), "start {name}");
            let _ = i;
        }
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let v = ProxyVerifier::new(
            9,
            None,
            vec![
                ("sdb-ut-up1".to_string(), dead_port),
                ("sdb-ut-up2".to_string(), dead_port),
                ("sdb-ut-up3".to_string(), dead_port),
            ],
        );
        let markers = v.learn_markers().await.expect("fallback must serve");
        assert_eq!(
            markers,
            vec![
                "upstream-1 marker\n".to_string(),
                "upstream-2 marker\n".to_string(),
                "upstream-3 marker\n".to_string(),
            ]
        );
        for name in ["sdb-ut-up1", "sdb-ut-up2", "sdb-ut-up3"] {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", name])
                .output();
        }
    }

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

    // ---- Parsing edge cases and mock-server driven stage logic ----

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Canonical `upstream-N marker` body for a request index (round-robin).
    fn marker_for(i: usize) -> Vec<u8> {
        format!("upstream-{} marker\n", i % 3 + 1).into_bytes()
    }

    /// Three in-process upstreams serving the canonical marker bodies.
    async fn spawn_marker_upstreams() -> Vec<(String, u16)> {
        let mut ups = Vec::new();
        for idx in 1u8..=3u8 {
            let body = format!("upstream-{idx} marker\n").into_bytes();
            let (port, _) = spawn_http_mock(move |_| (200, body.clone(), true)).await;
            ups.push((format!("sdb-ut-marker-{idx}"), port));
        }
        ups
    }

    /// Minimal HTTP/1.1 server: one handler decision per request, persistent
    /// connections until the handler asks for a close. Records request heads.
    async fn spawn_http_mock<F>(handler: F) -> (u16, Arc<Mutex<Vec<String>>>)
    where
        F: Fn(usize) -> (u16, Vec<u8>, bool) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_out = seen.clone();
        let idx = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                let idx = idx.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let head_end = loop {
                            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break p + 4;
                            }
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        seen.lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&buf[..head_end]).to_string());
                        buf.drain(..head_end);
                        let i = idx.fetch_add(1, Ordering::SeqCst);
                        let (status, body, close) = handler(i);
                        let reason = match status {
                            200 => "OK",
                            404 => "Not Found",
                            500 => "Internal Server Error",
                            502 => "Bad Gateway",
                            503 => "Service Unavailable",
                            _ => "Unknown",
                        };
                        let mut resp = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                            body.len(),
                            if close { "close" } else { "keep-alive" }
                        )
                        .into_bytes();
                        resp.extend_from_slice(&body);
                        if sock.write_all(&resp).await.is_err() {
                            return;
                        }
                        if close {
                            let _ = sock.shutdown().await;
                            return;
                        }
                    }
                });
            }
        });
        (port, seen_out)
    }

    /// Raw-byte server: reads one request, writes the returned bytes, closes.
    async fn spawn_raw_oneshot<F>(make: F) -> u16
    where
        F: Fn(usize) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let idx = Arc::new(AtomicUsize::new(0));
        let make = Arc::new(make);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let make = make.clone();
                let idx = idx.clone();
                tokio::spawn(async move {
                    let mut tmp = [0u8; 1024];
                    let _ = sock.read(&mut tmp).await;
                    let i = idx.fetch_add(1, Ordering::SeqCst);
                    let _ = sock.write_all(&make(i)).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        port
    }

    /// Raw-byte server that answers but keeps the socket open (no EOF).
    async fn spawn_raw_hold<F>(make: F) -> u16
    where
        F: Fn(usize) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let idx = Arc::new(AtomicUsize::new(0));
        let make = Arc::new(make);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let make = make.clone();
                let idx = idx.clone();
                tokio::spawn(async move {
                    let mut tmp = [0u8; 1024];
                    let _ = sock.read(&mut tmp).await;
                    let i = idx.fetch_add(1, Ordering::SeqCst);
                    let _ = sock.write_all(&make(i)).await;
                    // Never close: framing (or the exchange deadline) must
                    // decide when the response is complete.
                    tokio::time::sleep(Duration::from_secs(10)).await;
                });
            }
        });
        port
    }

    /// Reserved-then-dropped port nothing listens on.
    /// Raw-socket mock proxy: the handler builds the full response bytes
    /// from the request head; `stall_close_ms > 0` closes sockets whose
    /// head never completes (0 = never close, the slow-loris defect).
    async fn spawn_raw_proxy<F>(handler: F, stall_close_ms: u64) -> u16
    where
        F: Fn(String) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = std::sync::Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        let read = if stall_close_ms > 0 {
                            match tokio::time::timeout(
                                Duration::from_millis(stall_close_ms),
                                sock.read(&mut tmp),
                            )
                            .await
                            {
                                Ok(r) => r,
                                Err(_) => return,
                            }
                        } else {
                            sock.read(&mut tmp).await
                        };
                        match read {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                        if buf.len() > 256 * 1024 {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let reply = handler(head);
                    let _ = sock.write_all(&reply).await;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                });
            }
        });
        port
    }

    fn raw_http(status: u16, reason: &str, body: &str, extra: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
            body.len(),
            extra,
            body
        )
        .into_bytes()
    }

    fn fixture_marker_body(idx: u8) -> String {
        crate::sandbox::docker::SandboxManager::upstream_marker_body(idx)
    }

    #[test]
    fn test_status_and_cache_helpers() {
        let map = parse_status_lines("upstream1: up\nupstream2: down\nupstream3: up\nextra: 1\n");
        assert_eq!(map.get(&1), Some(&true));
        assert_eq!(map.get(&2), Some(&false));
        assert_eq!(map.get(&3), Some(&true));
        assert_eq!(map.get(&4), None);
        assert!(parse_status_lines("garbage").is_empty());

        let mut headers = HashMap::new();
        headers.insert("x-cache".to_string(), " hit ".to_string());
        let resp = HttpResponse {
            version: "HTTP/1.1".into(),
            status: 200,
            headers,
            body: Vec::new(),
        };
        assert_eq!(x_cache_of(&resp).as_deref(), Some("HIT"));
        assert_eq!(status_code_of(b"HTTP/1.1 431 Too Long\r\n\r\n"), Some(431));
        assert_eq!(status_code_of(b"nope"), None);
    }

    #[tokio::test]
    async fn test_status_snapshot_mock_and_failures() {
        let port = spawn_raw_proxy(
            |_| {
                raw_http(
                    200,
                    "OK",
                    "upstream1: up\nupstream2: down\nupstream3: up\n",
                    "",
                )
            },
            300,
        )
        .await;
        let map = ProxyVerifier::status_snapshot(port).await.unwrap();
        assert_eq!(map.get(&2), Some(&false));

        let port = spawn_raw_proxy(|_| raw_http(200, "OK", "no health here", ""), 300).await;
        let err = ProxyVerifier::status_snapshot(port).await.unwrap_err();
        assert!(err.contains("no `upstreamN: up|down` lines"), "{err}");

        let port = spawn_raw_proxy(|_| raw_http(500, "Server Error", "boom", ""), 300).await;
        let err = ProxyVerifier::status_snapshot(port).await.unwrap_err();
        assert!(err.contains("returned 500"), "{err}");
    }

    #[derive(Clone, Copy, PartialEq)]
    enum CacheMode {
        Honest,
        NeverHits,
        HitWithoutAge,
        HitBodyMismatch,
        NeverExpires,
    }

    /// Mock proxy with a one-entry TTL cache over marker 1's body.
    async fn spawn_cache_mock(mode: CacheMode, ttl_secs: u64) -> u16 {
        use std::sync::{Arc, Mutex};
        let cache: Arc<Mutex<Option<(String, Instant)>>> = Arc::new(Mutex::new(None));
        spawn_raw_proxy(
            move |_head: String| {
                let body = fixture_marker_body(1);
                let mut guard = cache.lock().unwrap();
                let now = Instant::now();
                let fresh = match &*guard {
                    None => true,
                    Some((_, at)) => {
                        mode != CacheMode::NeverExpires
                            && now.duration_since(*at).as_secs() >= ttl_secs
                    }
                };
                if fresh {
                    *guard = Some((body.clone(), now));
                    return raw_http(200, "OK", &body, "X-Cache: MISS\r\n");
                }
                let age = now.duration_since(guard.as_ref().unwrap().1).as_secs();
                match mode {
                    CacheMode::NeverHits => {
                        *guard = Some((body.clone(), now));
                        raw_http(200, "OK", &body, "X-Cache: MISS\r\n")
                    }
                    CacheMode::HitWithoutAge => raw_http(200, "OK", &body, "X-Cache: HIT\r\n"),
                    CacheMode::HitBodyMismatch => {
                        raw_http(200, "OK", &format!("{body}X"), "X-Cache: HIT\r\nAge: 0\r\n")
                    }
                    _ => raw_http(200, "OK", &body, &format!("X-Cache: HIT\r\nAge: {age}\r\n")),
                }
            },
            300,
        )
        .await
    }

    #[tokio::test]
    async fn test_stage8_caching_mock_pass_and_failures() {
        let port = spawn_cache_mock(CacheMode::Honest, 1).await;
        let v = ProxyVerifier::new(port, Some("cm"), Vec::new());
        let st = v.test_stage8_caching_with(1).await;
        assert!(st.passed, "honest TTL cache must pass: {:?}", st.error);

        let port = spawn_cache_mock(CacheMode::NeverHits, 1).await;
        let v = ProxyVerifier::new(port, Some("cm"), Vec::new());
        let st = v.test_stage8_caching_with(1).await;
        assert!(!st.passed, "a cache that never hits must fail");
        assert!(
            st.error
                .as_deref()
                .unwrap_or("")
                .contains("no repeat request"),
            "{:?}",
            st.error
        );

        let port = spawn_cache_mock(CacheMode::HitWithoutAge, 1).await;
        let v = ProxyVerifier::new(port, Some("cm"), Vec::new());
        let st = v.test_stage8_caching_with(1).await;
        assert!(!st.passed, "a HIT without Age must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("Age"),
            "{:?}",
            st.error
        );

        let port = spawn_cache_mock(CacheMode::HitBodyMismatch, 1).await;
        let v = ProxyVerifier::new(port, Some("cm"), Vec::new());
        let st = v.test_stage8_caching_with(1).await;
        assert!(
            !st.passed,
            "a HIT whose body is not the MISS body must fail"
        );
        assert!(
            st.error.as_deref().unwrap_or("").contains("differs"),
            "{:?}",
            st.error
        );

        let port = spawn_cache_mock(CacheMode::NeverExpires, 1).await;
        let v = ProxyVerifier::new(port, Some("cm"), Vec::new());
        let st = v.test_stage8_caching_with(1).await;
        assert!(!st.passed, "entries that never expire must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("never expired"),
            "{:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_stage9_limits_mock_pass_and_oversized_head_fail() {
        // A proxy with sane limits: 431 on an oversized head, 413 on an
        // oversized declared body, stalled sockets reclaimed.
        let strict = |head: String| {
            let lower = head.to_ascii_lowercase();
            if head.len() > 8 * 1024 {
                return raw_http(431, "Request Header Fields Too Large", "", "");
            }
            if let Some(pos) = lower.find("content-length:") {
                let digits: String = lower[pos + "content-length:".len()..]
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if digits
                    .parse::<u64>()
                    .map(|v| v > 1_048_576)
                    .unwrap_or(false)
                {
                    return raw_http(413, "Payload Too Large", "", "");
                }
            }
            raw_http(200, "OK", &fixture_marker_body(1), "")
        };
        let port = spawn_raw_proxy(strict, 700).await;
        let v = ProxyVerifier::new(port, Some("lm"), Vec::new());
        let st = v.test_stage9_limits().await;
        assert!(st.passed, "well-behaved limits must pass: {:?}", st.error);

        // A proxy that answers 200 to an oversized head and never reclaims
        // stalled sockets fails loudly.
        let careless = |_head: String| raw_http(200, "OK", &fixture_marker_body(1), "");
        let port = spawn_raw_proxy(careless, 0).await;
        let v = ProxyVerifier::new(port, Some("lm"), Vec::new());
        let st = v.test_stage9_limits().await;
        assert!(!st.passed, "an oversized head answered 200 must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("32KiB"),
            "{:?}",
            st.error
        );
    }

    async fn dead_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[test]
    fn test_parse_status_line_edge_cases() {
        assert_eq!(
            parse_status_line("HTTP/1.1 204 No Content"),
            Some(("HTTP/1.1".to_string(), 204))
        );
        assert_eq!(
            parse_status_line("HTTP/1.1 200"),
            Some(("HTTP/1.1".to_string(), 200))
        );
        assert_eq!(parse_status_line("HTTP/1.1 abc"), None);
        assert_eq!(parse_status_line("HTTP/1.1 -1"), None);
        assert_eq!(parse_status_line("HTTP/1.1 65536"), None);
        assert_eq!(parse_status_line("HTTP/1.1\r"), None);
    }

    #[test]
    fn test_parse_header_block_malformed_and_last_value_wins() {
        let map =
            parse_header_block("X-FF: 1\r\nX-FF: 2\r\nNoColon\r\n: empty\r\nEmpty-Value:\r\n");
        assert_eq!(map.get("x-ff").map(String::as_str), Some("2"));
        assert_eq!(map.get("empty-value").map(String::as_str), Some(""));
        assert_eq!(map.len(), 2, "malformed lines must be skipped: {map:?}");
    }

    fn stub_response(headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            version: "HTTP/1.1".to_string(),
            status: 200,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn test_content_length_satisfied_framing_rules() {
        // Chunked is complete by definition: decoding already required the
        // terminal zero chunk.
        let chunked = stub_response(&[("transfer-encoding", "chunked")]);
        assert!(content_length_satisfied(b"", &chunked));
        let chunked_upper = stub_response(&[("transfer-encoding", "Chunked")]);
        assert!(content_length_satisfied(b"", &chunked_upper));

        let cl = stub_response(&[("content-length", "5")]);
        assert!(content_length_satisfied(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            &cl
        ));
        assert!(!content_length_satisfied(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel",
            &cl
        ));
        assert!(
            !content_length_satisfied(b"hello", &cl),
            "no header terminator yet"
        );

        let bad = stub_response(&[("content-length", "many")]);
        assert!(!content_length_satisfied(
            b"HTTP/1.1 200 OK\r\nContent-Length: many\r\n\r\nbody",
            &bad
        ));

        let none = stub_response(&[]);
        assert!(!content_length_satisfied(
            b"HTTP/1.1 200 OK\r\n\r\nbody",
            &none
        ));
    }

    #[test]
    fn test_parse_http_response_http10_headers_and_invalid_lengths() {
        let raw = b"HTTP/1.0 404 Not Found\r\nX-A: 1\r\nX-A: 2\r\nNoColon\r\n\r\nbody";
        let resp = parse_http_response(raw).expect("must parse");
        assert_eq!(resp.version, "HTTP/1.0");
        assert_eq!(resp.status, 404);
        assert_eq!(resp.header("x-a"), Some("2"));
        assert_eq!(resp.body_text(), "body");
        assert!(!resp.is_chunked());

        assert!(
            parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\nbody").is_none()
        );
        assert!(
            parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\nbody").is_none()
        );
    }

    #[test]
    fn test_chunked_trailer_is_ignored_after_terminal_chunk() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let resp = parse_http_response(raw).expect("must parse");
        assert_eq!(resp.body, b"hello".to_vec());
        assert!(resp.is_chunked());
    }

    #[test]
    fn test_body_text_is_lossy_for_non_utf8() {
        let resp = HttpResponse {
            version: "HTTP/1.1".to_string(),
            status: 200,
            headers: HashMap::new(),
            body: vec![0xff, b'o', b'k'],
        };
        assert_eq!(resp.body_text(), "\u{fffd}ok");
    }

    #[test]
    fn test_body_marker_uses_list_order_and_empty_list() {
        let markers = vec!["alpha".to_string(), "beta".to_string()];
        // The first list entry that matches wins, regardless of text order.
        assert_eq!(
            ProxyVerifier::body_marker("x beta alpha", &markers),
            Some(0)
        );
        assert_eq!(ProxyVerifier::body_marker("x beta", &markers), Some(1));
        assert_eq!(ProxyVerifier::body_marker("anything", &[]), None);
    }

    #[test]
    fn test_get_seed_prefix_contract() {
        let v = ProxyVerifier::new(1, None, vec![]);
        assert_eq!(v.get_seed_prefix("proxy"), "proxy");
        let v = ProxyVerifier::new(1, Some("0123456789"), vec![]);
        assert_eq!(v.get_seed_prefix("proxy"), "23456789");
    }

    #[test]
    fn test_upstream_running_false_for_absent_container() {
        // Docker absent or container absent: inspect must fail closed.
        assert!(!ProxyVerifier::upstream_running("sdb-ut-absent-probe-91"));
    }

    #[tokio::test]
    async fn test_fetch_request_and_response_contract() {
        let (port, seen) = spawn_http_mock(|_| (200, b"hello-body".to_vec(), true)).await;
        let (raw, resp) = ProxyVerifier::fetch(port).await.expect("mock must answer");

        assert!(raw.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(resp.version, "HTTP/1.1");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello-body".to_vec());
        assert_eq!(resp.header("Content-Length"), Some("10"));
        assert_eq!(resp.body_text(), "hello-body");
        assert!(!resp.is_chunked());

        let heads = seen.lock().unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(
            heads[0], "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            "fetch must dial exactly this request"
        );
    }

    #[tokio::test]
    async fn test_fetch_empty_and_unparseable_are_errors() {
        let port = spawn_raw_oneshot(|_| Vec::new()).await;
        let err = ProxyVerifier::fetch(port).await.unwrap_err();
        assert!(err.starts_with("Empty response from 127.0.0.1:"), "{err}");

        let port = spawn_raw_oneshot(|_| b"GARBAGE\r\n\r\n".to_vec()).await;
        let err = ProxyVerifier::fetch(port).await.unwrap_err();
        assert!(
            err.starts_with("Failed to parse HTTP response from 127.0.0.1:"),
            "{err}"
        );

        let err = ProxyVerifier::fetch(dead_port().await).await.unwrap_err();
        assert!(err.starts_with("Failed to connect to 127.0.0.1:"), "{err}");
    }

    #[tokio::test]
    async fn test_fetch_chunked_framing_finishes_without_eof() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
                .to_vec();
        let port = spawn_raw_hold(move |_| raw.clone()).await;
        let start = Instant::now();
        let (_, resp) = ProxyVerifier::fetch(port).await.expect("framed response");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must finish on the terminal chunk, not EOF or the deadline"
        );
        assert!(resp.is_chunked());
        assert_eq!(resp.body, b"hello".to_vec());
    }

    #[tokio::test]
    async fn test_fetch_times_out_when_framing_incomplete() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: keep-alive\r\n\r\nshort"
            .to_vec();
        let port = spawn_raw_hold(move |_| raw.clone()).await;
        let start = Instant::now();
        let err = ProxyVerifier::fetch(port).await.unwrap_err();
        assert!(err.contains("Read from 127.0.0.1:"), "{err}");
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() >= Duration::from_secs(2));
        assert!(start.elapsed() < Duration::from_secs(8));
    }

    #[tokio::test]
    async fn test_fetch_keepalive_counts_and_errors() {
        // One socket, three framed 200s.
        let (port, seen) = spawn_http_mock(|_| (200, b"live".to_vec(), false)).await;
        assert_eq!(ProxyVerifier::fetch_keepalive(port, 3).await.unwrap(), 3);
        assert_eq!(seen.lock().unwrap().len(), 3, "one connection, three GETs");

        // A second response that closes the socket breaks the sequence.
        let (port, _) = spawn_http_mock(|_| (200, b"live".to_vec(), true)).await;
        assert_eq!(ProxyVerifier::fetch_keepalive(port, 1).await.unwrap(), 1);
        assert!(ProxyVerifier::fetch_keepalive(port, 3).await.is_err());

        // A non-200 inside the sequence must be named.
        let (port, _) = spawn_http_mock(|i| {
            if i == 1 {
                (500, b"boom".to_vec(), false)
            } else {
                (200, b"ok".to_vec(), false)
            }
        })
        .await;
        let err = ProxyVerifier::fetch_keepalive(port, 3).await.unwrap_err();
        assert_eq!(err, "keep-alive GET got 500");

        // A server that answers once then hangs trips the per-read timeout
        // instead of stalling verification forever.
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok".to_vec();
        let port = spawn_raw_hold(move |_| raw.clone()).await;
        let start = Instant::now();
        let err = ProxyVerifier::fetch_keepalive(port, 3).await.unwrap_err();
        assert!(err.contains("Read from 127.0.0.1:"), "{err}");
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn test_learn_markers_happy_path_and_preconditions() {
        // Fewer than three upstreams is rejected before any dial.
        let v = ProxyVerifier::new(1, None, vec![("solo".to_string(), 1)]);
        assert_eq!(
            v.learn_markers().await.unwrap_err(),
            "Need 3 upstreams, have 1"
        );

        // Three reachable stubs: canonical markers in upstream order.
        let ups = spawn_marker_upstreams().await;
        let markers = ProxyVerifier::new(1, None, ups)
            .learn_markers()
            .await
            .unwrap();
        assert_eq!(
            markers,
            vec![
                "upstream-1 marker\n".to_string(),
                "upstream-2 marker\n".to_string(),
                "upstream-3 marker\n".to_string(),
            ]
        );

        // A 500 upstream plus absent containers: loud harness fault naming the culprit.
        let (bad, _) = spawn_http_mock(|_| (500, b"boom".to_vec(), true)).await;
        let v = ProxyVerifier::new(
            1,
            None,
            vec![
                ("sdb-ut-absent-a".to_string(), bad),
                ("sdb-ut-absent-b".to_string(), bad),
                ("sdb-ut-absent-c".to_string(), bad),
            ],
        );
        let err = v.learn_markers().await.unwrap_err();
        assert!(err.contains("harness fixture fault"), "{err}");
        assert!(err.contains("sdb-ut-absent-a"), "{err}");

        // Reachable upstreams with empty bodies reach the same fault path.
        let (empty, _) = spawn_http_mock(|_| (200, Vec::new(), true)).await;
        let v = ProxyVerifier::new(
            1,
            None,
            vec![
                ("sdb-ut-absent-a".to_string(), empty),
                ("sdb-ut-absent-b".to_string(), empty),
                ("sdb-ut-absent-c".to_string(), empty),
            ],
        );
        let err = v.learn_markers().await.unwrap_err();
        assert!(err.contains("harness fixture fault"), "{err}");
    }

    #[tokio::test]
    async fn test_stage1_routing_pass_and_failures() {
        let ups = spawn_marker_upstreams().await;

        let (proxy, _) = spawn_http_mock(|i| (200, marker_for(i), false)).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage1()
            .await;
        assert!(r.passed, "round-robin proxy must pass: {:?}", r.error);
        assert_eq!(r.stage, 1);
        assert_eq!(r.name, "Stage 1: Basic routing (200 + marker)");

        let (proxy, _) = spawn_http_mock(|_| (503, b"down".to_vec(), true)).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage1()
            .await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("Expected 200, got 503"));

        let (proxy, _) = spawn_http_mock(|_| (200, b"static stub".to_vec(), true)).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage1()
            .await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("Body matches no upstream marker"));

        let r = ProxyVerifier::new(dead_port().await, None, ups.clone())
            .test_stage1()
            .await;
        assert!(!r.passed);
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .starts_with("GET / via proxy: Failed to connect"),
            "{:?}",
            r.error
        );

        // Marker-learning failure is reported with stage context.
        let r = ProxyVerifier::new(1, None, vec![("solo".to_string(), 1)])
            .test_stage1()
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("Upstream markers: Need 3 upstreams, have 1")
        );
    }

    #[tokio::test]
    async fn test_stage2_round_robin_pass_and_failure_modes() {
        let ups = spawn_marker_upstreams().await;

        let (proxy, _) = spawn_http_mock(|i| (200, marker_for(i), false)).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage2()
            .await;
        assert!(r.passed, "even spread must pass: {:?}", r.error);
        assert_eq!(r.name, "Stage 2: Round-robin across all 3 upstreams");

        // A sticky proxy starves two upstreams.
        let (proxy, _) = spawn_http_mock(|_| (200, marker_for(0), false)).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage2()
            .await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("Uneven spread, hits: [30, 0, 0]"));

        // A mid-burst non-200 is reported with its index.
        let (proxy, _) = spawn_http_mock(|i| {
            if i == 3 {
                (503, Vec::new(), false)
            } else {
                (200, marker_for(i), false)
            }
        })
        .await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage2()
            .await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("GET 3: got 503"));

        // A body matching no marker fails immediately.
        let (proxy, _) = spawn_http_mock(|_| (200, b"mystery body".to_vec(), false)).await;
        let r = ProxyVerifier::new(proxy, None, ups).test_stage2().await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("GET 0 matched no upstream"));

        // Marker-learning failure is reported with stage context.
        let r = ProxyVerifier::new(1, None, vec![("solo".to_string(), 1)])
            .test_stage2()
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("Upstream markers: Need 3 upstreams, have 1")
        );

        // An unreachable proxy is reported on the first GET.
        let r = ProxyVerifier::new(dead_port().await, None, spawn_marker_upstreams().await)
            .test_stage2()
            .await;
        assert!(!r.passed);
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .starts_with("GET 0: Failed to connect"),
            "{:?}",
            r.error
        );
    }

    #[tokio::test]
    async fn test_stage3_framing_and_keepalive_failure_modes() {
        let ups = spawn_marker_upstreams().await;

        // Faithful: HTTP/1.1 + Content-Length + three keep-alive GETs.
        let (proxy, _) = spawn_http_mock(|_| (200, b"ok".to_vec(), false)).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage3()
            .await;
        assert!(r.passed, "framed keep-alive proxy must pass: {:?}", r.error);
        assert_eq!(r.name, "Stage 3: Valid HTTP/1.1 framing + keep-alive");

        // HTTP/1.0 status line is not HTTP/1.1.
        let raw10 = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec();
        let proxy = spawn_raw_oneshot(move |_| raw10.clone()).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage3()
            .await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("Status line is not HTTP/1.1"));

        // Close-delimited body: neither Content-Length nor chunked coding.
        let raw_none =
            b"HTTP/1.1 200 OK\r\nX-Note: no framing\r\nConnection: close\r\n\r\nok".to_vec();
        let proxy = spawn_raw_oneshot(move |_| raw_none.clone()).await;
        let r = ProxyVerifier::new(proxy, None, ups.clone())
            .test_stage3()
            .await;
        assert!(!r.passed);
        assert_eq!(
            r.error.as_deref(),
            Some("No Content-Length or chunked framing")
        );

        // A proxy that closes after every response fails keep-alive.
        let (proxy, _) = spawn_http_mock(|_| (200, b"ok".to_vec(), true)).await;
        let r = ProxyVerifier::new(proxy, None, ups).test_stage3().await;
        assert!(!r.passed);
        assert!(
            r.error.as_deref().unwrap_or("").starts_with("keep-alive:"),
            "{:?}",
            r.error
        );

        // An unreachable proxy fails before any framing check.
        let r = ProxyVerifier::new(dead_port().await, None, vec![])
            .test_stage3()
            .await;
        assert!(!r.passed);
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .starts_with("GET / via proxy: Failed to connect"),
            "{:?}",
            r.error
        );
    }

    #[tokio::test]
    async fn test_run_routing_aggregates_three_stages() {
        let ups = spawn_marker_upstreams().await;
        let (proxy, seen) = spawn_http_mock(|i| (200, marker_for(i), false)).await;
        let summary = ProxyVerifier::new(proxy, None, ups).run_routing().await;

        assert_eq!(summary.total_stages, 3);
        assert_eq!(summary.passed_count, 3, "{:?}", summary.stages);
        assert_eq!(summary.pass_rate, 100.0);
        assert_eq!(summary.stages[0].stage, 1);
        assert_eq!(summary.stages[1].stage, 2);
        assert_eq!(summary.stages[2].stage, 3);
        assert!(summary.stages.iter().all(|s| s.passed && s.error.is_none()));
        // 1 (stage 1) + 30 (stage 2) + 1 + 3 (stage 3 fetch + keep-alive).
        assert_eq!(seen.lock().unwrap().len(), 35);
    }

    #[tokio::test]
    async fn test_run_failover_short_circuits_without_three_upstreams() {
        let v = ProxyVerifier::new(1, None, vec![("only".to_string(), 1)]);
        let r = v.run_failover().await;
        assert!(!r.passed);
        assert_eq!(r.stage, 4);
        assert_eq!(r.name, "Stage 4: Failover (one upstream down)");
        assert_eq!(r.error.as_deref(), Some("Need 3 upstreams, have 1"));
    }

    #[tokio::test]
    async fn test_measure_throughput_pass_and_failures() {
        let (proxy, _) = spawn_http_mock(|_| (200, b"ok".to_vec(), true)).await;
        let fps = ProxyVerifier::new(proxy, None, vec![])
            .measure_throughput()
            .await
            .expect("200 local 200s must measure");
        assert!(fps > 0.0 && fps.is_finite(), "fps={fps}");

        let (proxy, _) = spawn_http_mock(|_| (503, b"down".to_vec(), true)).await;
        assert!(ProxyVerifier::new(proxy, None, vec![])
            .measure_throughput()
            .await
            .is_none());

        let dead = dead_port().await;
        assert!(ProxyVerifier::new(dead, None, vec![])
            .measure_throughput()
            .await
            .is_none());
    }
}
