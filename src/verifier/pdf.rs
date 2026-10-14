use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;
use std::time::{Duration, Instant};

use super::StageResult;

#[derive(Debug, Clone)]
pub struct PdfTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct PdfVerifier {
    pub target_port: u16,
    pub client: reqwest::Client,
    pub run_seed: Option<String>,
}

impl PdfVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
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

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.target_port)
    }

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn stream_obj(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut o = if dict.is_empty() {
            format!("<< /Length {} >>\nstream\n", data.len()).into_bytes()
        } else {
            format!("<< /Length {} {} >>\nstream\n", data.len(), dict).into_bytes()
        };
        o.extend_from_slice(data);
        o.extend_from_slice(b"\nendstream");
        o
    }

    /// Assembles numbered objects (1-based) with a correct xref table + trailer.
    fn build_pdf(objects: &[Vec<u8>], root: u32) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_pos = out.len();
        let count = objects.len() + 1;
        out.extend_from_slice(format!("xref\n0 {}\n", count).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root {} 0 R >>\nstartxref\n{}\n%%EOF\n",
                count, root, xref_pos
            )
            .as_bytes(),
        );
        out
    }

    fn single_page_pdf(stream_data: &[u8], dict: &str) -> Vec<u8> {
        let objs = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>".to_vec(),
            Self::stream_obj(dict, stream_data),
        ];
        Self::build_pdf(&objs, 1)
    }

    /// Single arbitrary-text page (dynamic-content pin): the caller supplies
    /// the literal text; the Tj escaping burden stays on the caller (nonces
    /// are `[a-z0-9-]` by construction).
    fn build_text_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({}) Tj ET", text);
        Self::single_page_pdf(stream.as_bytes(), "")
    }

    pub fn pdf_stage1(&self) -> (Vec<u8>, String) {
        let pfx = self.get_seed_prefix("live");
        let want = format!("Hello, SubDollar {}!", pfx);
        let stream = format!("BT /F1 12 Tf 72 720 Td (Hello, SubDollar {}!) Tj ET", pfx);
        let pdf = Self::single_page_pdf(stream.as_bytes(), "");
        (pdf, want)
    }

    pub fn pdf_stage2(&self) -> (Vec<u8>, String) {
        let pfx = self.get_seed_prefix("live");
        let want = format!("Hello, {}!\nHiA(B)", pfx);
        let stream = format!(
            "BT /F1 12 Tf 72 720 Td (Hello, ) Tj [({}) 120 (!)] TJ 0 -14 Td <4869> Tj (A\\(B\\)) Tj ET",
            pfx
        );
        let pdf = Self::single_page_pdf(stream.as_bytes(), "");
        (pdf, want)
    }

    pub fn pdf_stage3(&self) -> (Vec<u8>, String) {
        let pfx = self.get_seed_prefix("live");
        let want = format!("Compressed stream {} works!", pfx);
        let raw_stream = format!(
            "BT /F1 12 Tf 72 720 Td (Compressed stream {} works!) Tj ET",
            pfx
        );
        let compressed = Self::zlib_compress(raw_stream.as_bytes());
        let pdf = Self::single_page_pdf(&compressed, "/Filter /FlateDecode");
        (pdf, want)
    }

    pub fn pdf_stage4(&self) -> (Vec<u8>, String) {
        let pfx = self.get_seed_prefix("live");
        let p1 = format!("Page one {}", pfx);
        let p2 = format!("Page two {}", pfx);
        let p3 = format!("Page three {}", pfx);
        let want = format!("{}\n{}\n{}", p1, p2, p3);

        let s1 = format!("BT /F1 12 Tf 72 720 Td ({}) Tj ET", p1);
        let s2 = format!("BT /F1 12 Tf 72 700 Td ({}) Tj ET", p2);
        let s3 = format!("BT /F1 12 Tf 72 680 Td ({}) Tj ET", p3);
        let decoy_id = rand::random::<u16>();

        let objs = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 6 0 R >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 7 0 R >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 8 0 R >>".to_vec(),
            Self::stream_obj("", s1.as_bytes()),
            Self::stream_obj("", s2.as_bytes()),
            Self::stream_obj("", s3.as_bytes()),
            // Decoy: valid object, never referenced from the page tree.
            format!(
                "<< /Note (internal) >>\n(DECOY-{} must not appear) Tj",
                decoy_id
            )
            .into_bytes(),
        ];
        (Self::build_pdf(&objs, 1), want)
    }

    async fn post_extract(&self, pdf: &[u8]) -> Result<reqwest::Response, String> {
        self.client
            .post(format!("{}/extract", self.base()))
            .header("Content-Type", "application/pdf")
            .body(pdf.to_vec())
            .send()
            .await
            .map_err(|e| format!("POST /extract failed: {}", e))
    }

    fn short(s: &str) -> String {
        const MAX_CHARS: usize = 160;
        let mut chars = s.chars();
        let prefix: String = chars.by_ref().take(MAX_CHARS).collect();
        if chars.next().is_some() {
            format!("{}...", prefix)
        } else {
            prefix
        }
    }

    async fn check_extract(
        &self,
        stage: u32,
        name: &str,
        pdf: Vec<u8>,
        expected: &str,
    ) -> StageResult {
        let done = |passed: bool, error: Option<String>| StageResult {
            stage,
            name: name.to_string(),
            passed,
            error,
        };
        match self.post_extract(&pdf).await {
            Err(e) => done(false, Some(e)),
            Ok(resp) => {
                if resp.status().as_u16() != 200 {
                    return done(false, Some(format!("status {}", resp.status())));
                }
                match resp.bytes().await {
                    Err(e) => done(false, Some(format!("read body: {}", e))),
                    Ok(body) => {
                        let got = String::from_utf8_lossy(&body).to_string();
                        if got == expected {
                            done(true, None)
                        } else {
                            done(
                                false,
                                Some(format!(
                                    "mismatch: got {:?}, want {:?}",
                                    Self::short(&got),
                                    expected
                                )),
                            )
                        }
                    }
                }
            }
        }
    }

    async fn test_stage1_single_tj(&self) -> StageResult {
        let (pdf, want) = self.pdf_stage1();
        self.check_extract(1, "Stage 1: Single-page Tj extraction", pdf, &want)
            .await
    }

    async fn test_stage2_positioning(&self) -> StageResult {
        let (pdf, want) = self.pdf_stage2();
        self.check_extract(
            2,
            "Stage 2: TJ arrays, positioning, hex, escapes",
            pdf,
            &want,
        )
        .await
    }

    async fn test_stage3_flate(&self) -> StageResult {
        let (pdf, want) = self.pdf_stage3();
        self.check_extract(3, "Stage 3: FlateDecode stream", pdf, &want)
            .await
    }

    async fn test_stage4_multipage(&self) -> StageResult {
        let (pdf, want) = self.pdf_stage4();
        self.check_extract(4, "Stage 4: Multi-page with decoy object", pdf, &want)
            .await
    }

    async fn test_stage5_bad_input(&self) -> StageResult {
        let name = "Stage 5: Bad input and routing".to_string();
        let done = |passed: bool, error: Option<String>| StageResult {
            stage: 5,
            name: name.clone(),
            passed,
            error,
        };
        match self
            .client
            .post(format!("{}/extract", self.base()))
            .header("Content-Type", "application/pdf")
            .body(b"this is not a pdf".to_vec())
            .send()
            .await
        {
            Err(e) => return done(false, Some(format!("garbage POST failed: {}", e))),
            Ok(resp) => {
                if resp.status().as_u16() != 400 {
                    return done(
                        false,
                        Some(format!("garbage POST status {}", resp.status())),
                    );
                }
            }
        }
        match self.client.get(format!("{}/", self.base())).send().await {
            Err(e) => return done(false, Some(format!("GET / failed: {}", e))),
            Ok(resp) => {
                if resp.status().as_u16() != 200 {
                    return done(false, Some(format!("GET / status {}", resp.status())));
                }
            }
        }
        match self
            .client
            .get(format!("{}/nope", self.base()))
            .send()
            .await
        {
            Err(e) => return done(false, Some(format!("GET /nope failed: {}", e))),
            Ok(resp) => {
                if resp.status().as_u16() != 404 {
                    return done(false, Some(format!("GET /nope status {}", resp.status())));
                }
            }
        }
        // Dynamic-content pin: two extracts with distinct nonce text must
        // return DISTINCT bodies containing their nonce. A stub returning a
        // canned extraction passes every stage above and fails here — the
        // PDF analogue of the pubsub hard-coded-Accept stub.
        let nonce_a = format!("nonce-a-{}", self.get_seed_prefix("pdfdyn"));
        let nonce_b = format!("nonce-b-{}", self.get_seed_prefix("pdfdyn"));
        let pdf_a = Self::build_text_pdf(&nonce_a);
        let pdf_b = Self::build_text_pdf(&nonce_b);
        let got_a = match self.post_extract(&pdf_a).await {
            Err(e) => return done(false, Some(format!("dynamic POST A failed: {}", e))),
            Ok(resp) => {
                if resp.status().as_u16() != 200 {
                    return done(
                        false,
                        Some(format!("dynamic POST A status {}", resp.status())),
                    );
                }
                match resp.bytes().await {
                    Err(e) => return done(false, Some(format!("dynamic read A: {}", e))),
                    Ok(b) => String::from_utf8_lossy(&b).to_string(),
                }
            }
        };
        let got_b = match self.post_extract(&pdf_b).await {
            Err(e) => return done(false, Some(format!("dynamic POST B failed: {}", e))),
            Ok(resp) => {
                if resp.status().as_u16() != 200 {
                    return done(
                        false,
                        Some(format!("dynamic POST B status {}", resp.status())),
                    );
                }
                match resp.bytes().await {
                    Err(e) => return done(false, Some(format!("dynamic read B: {}", e))),
                    Ok(b) => String::from_utf8_lossy(&b).to_string(),
                }
            }
        };
        if !got_a.contains(&nonce_a) || !got_b.contains(&nonce_b) {
            return done(
                false,
                Some(format!(
                    "dynamic extraction mismatch: A→{:?} B→{:?}",
                    Self::short(&got_a),
                    Self::short(&got_b)
                )),
            );
        }
        if got_a == got_b {
            return done(
                false,
                Some("canned extraction: distinct inputs, identical output".to_string()),
            );
        }
        done(true, None)
    }

    pub async fn test_stage6_concurrent_burst(&self) -> StageResult {
        // 16 concurrent multi-page extractions, all byte-exact.
        let name = "Stage 6: Concurrent Burst (16-way)".to_string();
        let (pdf, want) = self.pdf_stage4();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let port = self.target_port;
            let pdf_c = pdf.clone();
            let want_c = want.clone();
            tasks.push(tokio::spawn(async move {
                let v = PdfVerifier::new(port, None);
                let resp = v.post_extract(&pdf_c).await.map_err(|e| e.to_string())?;
                if resp.status().as_u16() != 200 {
                    return Err(format!("status {}", resp.status()));
                }
                let body = resp.bytes().await.map_err(|e| e.to_string())?;
                if body.as_ref() != want_c.as_bytes() {
                    return Err("body mismatch".to_string());
                }
                Ok::<(), String>(())
            }));
        }
        // Outer stage timeout so one wedged extraction cannot stall the
        // stage past the client timeouts. Spawned tasks are bounded by the
        // 10s reqwest client timeout each and drain on their own; the join
        // only bounds how long the stage waits for them.
        let fails = match tokio::time::timeout(Duration::from_secs(60), async {
            let mut fails = Vec::new();
            for t in tasks {
                if let Err(e) = t.await.unwrap_or(Err("join failed".to_string())) {
                    fails.push(e);
                }
            }
            fails
        })
        .await
        {
            Ok(fails) => fails,
            Err(_) => vec!["burst join timed out after 60s".to_string()],
        };
        if fails.is_empty() {
            StageResult {
                stage: 6,
                name,
                passed: true,
                error: None,
            }
        } else {
            StageResult {
                stage: 6,
                name,
                passed: false,
                error: Some(format!(
                    "{} of 16 burst extractions failed: {}",
                    fails.len(),
                    fails[0]
                )),
            }
        }
    }

    pub async fn run_all(&self) -> PdfTestSummary {
        let mut stages = Vec::new();
        stages.push(self.test_stage1_single_tj().await);
        stages.push(self.test_stage2_positioning().await);
        stages.push(self.test_stage3_flate().await);
        stages.push(self.test_stage4_multipage().await);
        stages.push(self.test_stage5_bad_input().await);
        stages.push(self.test_stage6_concurrent_burst().await);

        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;

        PdfTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    /// Repeated multi-page extractions, scored in pages/sec. None on any mismatch.
    pub async fn measure_throughput(&self) -> Option<f64> {
        let (pdf, want) = self.pdf_stage4();
        let iters = 20u32;
        let pages = 3.0 * iters as f64;
        let start = Instant::now();
        for _ in 0..iters {
            let resp = self.post_extract(&pdf).await.ok()?;
            if resp.status().as_u16() != 200 {
                return None;
            }
            let body = resp.bytes().await.ok()?;
            if body.as_ref() != want.as_bytes() {
                return None;
            }
        }
        let secs = start.elapsed().as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        Some(pages / secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_obj_offset(pdf: &[u8], num: u32) -> Option<usize> {
        let text = String::from_utf8_lossy(pdf).to_string();
        let xref_at: usize = text
            .rfind("startxref")
            .and_then(|p| text[p..].split_whitespace().nth(1))
            .and_then(|n| n.parse().ok())?;
        let entries: Vec<&str> = text[xref_at..].lines().skip(2).collect();
        // entries[0] is object 0 (free); entries[num] is object `num`.
        let line = entries.get(num as usize)?;
        if !line.ends_with(" n ") && !line.ends_with(" n") {
            return None;
        }
        line[0..10].parse().ok()
    }

    #[test]
    fn test_pdf_builder_xref_offsets_resolve() {
        let v = PdfVerifier::new(8090, Some("testseed"));
        let (pdf, _want) = v.pdf_stage1();
        let off3 = find_obj_offset(&pdf, 3).expect("offset 3");
        let off4 = find_obj_offset(&pdf, 4).expect("offset 4");
        assert!(
            pdf[off3..].starts_with(b"3 0 obj"),
            "object 3 offset does not point to header"
        );
        assert!(
            pdf[off4..].starts_with(b"4 0 obj"),
            "object 4 offset does not point to header"
        );
    }

    #[test]
    fn test_pdf_stage3_fixture_is_valid_zlib() {
        let v = PdfVerifier::new(8090, Some("testseed"));
        let (pdf, _want) = v.pdf_stage3();
        assert!(pdf
            .windows(b"/FlateDecode".len())
            .any(|w| w == b"/FlateDecode"));
    }

    #[test]
    fn test_short_utf8_boundary_safety() {
        assert_eq!(PdfVerifier::short("abc"), "abc");
    }
    #[test]
    fn test_pdf_verifier_seeded_uniqueness() {
        let v1 = PdfVerifier::new(8090, Some("alpha"));
        let v2 = PdfVerifier::new(8090, Some("beta"));

        let (pdf1, want1) = v1.pdf_stage1();
        let (pdf2, want2) = v2.pdf_stage1();

        assert_ne!(want1, want2);
        assert_ne!(pdf1, pdf2);
        assert!(want1.contains("alpha"));
        assert!(want2.contains("beta"));

        let (flate1, fwant1) = v1.pdf_stage3();
        let (flate2, fwant2) = v2.pdf_stage3();
        assert_ne!(fwant1, fwant2);
        assert_ne!(flate1, flate2);
    }

    // ---- Fixture layout, wire contract and stage plumbing ----

    use flate2::read::ZlibDecoder;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::io::Read as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One request as seen by the in-process HTTP mock.
    struct MockRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    /// Minimal HTTP/1.1 server for one request per connection. The handler
    /// maps a request to (status, body); every request seen is recorded.
    async fn spawn_http_mock<F>(handler: F) -> (u16, Arc<Mutex<Vec<MockRequest>>>)
    where
        F: Fn(&MockRequest) -> (u16, Vec<u8>) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let cap = captured.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let cap = cap.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let head_end = loop {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut lines = head.lines();
                    let mut parts = lines.next().unwrap_or("").split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let mut headers = HashMap::new();
                    for line in lines {
                        if let Some((name, value)) = line.split_once(':') {
                            headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }
                    let content_length = headers
                        .get("content-length")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let body = buf[head_end..(head_end + content_length).min(buf.len())].to_vec();
                    let req = MockRequest {
                        method,
                        path,
                        headers,
                        body,
                    };
                    let (status, resp_body) = handler(&req);
                    cap.lock().unwrap().push(req);
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        404 => "Not Found",
                        500 => "Internal Server Error",
                        503 => "Service Unavailable",
                        _ => "Unknown",
                    };
                    let mut resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        resp_body.len()
                    )
                    .into_bytes();
                    resp.extend_from_slice(&resp_body);
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (port, captured)
    }

    /// Oldest-first canned response bodies, shared with a mock handler.
    fn response_queue(bodies: Vec<Vec<u8>>) -> Arc<Mutex<VecDeque<Vec<u8>>>> {
        Arc::new(Mutex::new(bodies.into()))
    }

    /// Answers the first `answers.len()` requests with the given status/body
    /// pairs, then drops further connections without a response.
    async fn spawn_dropping_after(answers: Vec<(u16, Vec<u8>)>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let idx = Arc::new(AtomicUsize::new(0));
        let answers = Arc::new(answers);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let idx = idx.clone();
                let answers = answers.clone();
                tokio::spawn(async move {
                    let mut tmp = [0u8; 1024];
                    let _ = sock.read(&mut tmp).await;
                    let n = idx.fetch_add(1, Ordering::SeqCst);
                    if let Some((status, body)) = answers.get(n) {
                        let mut resp = format!(
                            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        resp.extend_from_slice(body);
                        let _ = sock.write_all(&resp).await;
                    }
                    // No answer after the scripted prefix: the connection
                    // closes and the client must report the failure.
                    let _ = sock.shutdown().await;
                });
            }
        });
        port
    }

    /// Body of numbered object `num`. Scans sequentially instead of walking
    /// the xref text: lossy UTF-8 conversion (used by `find_obj_offset`)
    /// shifts byte offsets on PDFs with binary streams like stage 3.
    fn obj_at(pdf: &[u8], num: u32) -> Vec<u8> {
        let header = format!("\n{num} 0 obj\n");
        let start = pdf
            .windows(header.len())
            .position(|w| w == header.as_bytes())
            .expect("object header")
            + header.len();
        let rest = &pdf[start..];
        let end = rest
            .windows(7)
            .position(|w| w == b"endobj\n")
            .expect("object must terminate");
        rest[..end].to_vec()
    }

    /// Bytes between `stream\n` and the final `\nendstream` of an object.
    fn stream_payload(obj: &[u8]) -> Vec<u8> {
        let start = obj
            .windows(7)
            .position(|w| w == b"stream\n")
            .expect("stream keyword")
            + 7;
        let end = obj
            .windows(10)
            .rposition(|w| w == b"\nendstream")
            .expect("endstream keyword");
        obj[start..end].to_vec()
    }

    /// Object bytes up to (excluding) the `stream` keyword.
    fn dict_bytes(obj: &[u8]) -> Vec<u8> {
        let start = obj
            .windows(7)
            .position(|w| w == b"stream\n")
            .expect("stream keyword");
        obj[..start].to_vec()
    }

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ZlibDecoder::new(data).read_to_end(&mut out).unwrap();
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn test_base_and_seed_prefix_contract() {
        let v = PdfVerifier::new(4321, None);
        assert_eq!(v.base(), "http://127.0.0.1:4321");
        assert_eq!(v.get_seed_prefix("live"), "live");
        assert_eq!(v.get_seed_prefix(""), "");
        assert_eq!(v.run_seed, None);

        // Empty seeds fall back to the default; longer seeds use the last 8.
        let v = PdfVerifier::new(1, Some(""));
        assert_eq!(v.get_seed_prefix("live"), "live");
        let v = PdfVerifier::new(1, Some("short"));
        assert_eq!(v.get_seed_prefix("live"), "short");
        let v = PdfVerifier::new(1, Some("0123456789"));
        assert_eq!(v.get_seed_prefix("live"), "23456789");
        assert_eq!(v.run_seed.as_deref(), Some("0123456789"));
    }

    #[test]
    fn test_zlib_compress_roundtrip() {
        let raw = b"BT /F1 12 Tf (compress me) Tj ET";
        let compressed = PdfVerifier::zlib_compress(raw);
        assert_ne!(
            compressed,
            raw.to_vec(),
            "compression must produce zlib bytes"
        );
        assert_eq!(deflate(&compressed), raw.to_vec());

        let empty = PdfVerifier::zlib_compress(b"");
        assert_eq!(deflate(&empty), Vec::<u8>::new());
    }

    #[test]
    fn test_stream_obj_layout_with_and_without_dict() {
        assert_eq!(
            PdfVerifier::stream_obj("", b"hello"),
            b"<< /Length 5 >>\nstream\nhello\nendstream".to_vec()
        );
        assert_eq!(
            PdfVerifier::stream_obj("/Filter /FlateDecode", b"\x00\x01\x02"),
            b"<< /Length 3 /Filter /FlateDecode >>\nstream\n\x00\x01\x02\nendstream".to_vec()
        );
    }

    #[test]
    fn test_build_pdf_xref_trailer_and_offsets() {
        let objs = vec![
            b"<< /Type /Catalog >>".to_vec(),
            b"<< /Type /Pages /Count 0 >>".to_vec(),
        ];
        let pdf = PdfVerifier::build_pdf(&objs, 2);
        assert!(pdf.starts_with(b"%PDF-1.4\n"));

        // Every numbered object resolves through the xref to its own header.
        for n in 1..=objs.len() {
            let off = find_obj_offset(&pdf, n as u32)
                .unwrap_or_else(|| panic!("object {n} must resolve"));
            assert!(
                pdf[off..].starts_with(format!("{n} 0 obj\n").as_bytes()),
                "object {n} offset points at the wrong bytes"
            );
        }

        let text = String::from_utf8_lossy(&pdf).to_string();
        assert!(text.contains("xref\n0 3\n"));
        assert!(text.contains("0000000000 65535 f \n"), "free entry missing");
        assert!(
            text.contains("trailer\n<< /Size 3 /Root 2 0 R >>\n"),
            "trailer must carry size and requested root"
        );
        let xref_pos: usize = text
            .rfind("startxref")
            .and_then(|p| text[p..].split_whitespace().nth(1))
            .and_then(|n| n.parse().ok())
            .expect("startxref value");
        assert!(
            pdf[xref_pos..].starts_with(b"xref\n0 3\n"),
            "startxref must point at the xref keyword"
        );
        assert!(pdf.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn test_single_page_pdf_object_wiring() {
        let data = b"page-content";
        let pdf = PdfVerifier::single_page_pdf(data, "/Filter /FlateDecode");

        let catalog = obj_at(&pdf, 1);
        assert!(String::from_utf8_lossy(&catalog).contains("/Type /Catalog /Pages 2 0 R"));
        let pages = obj_at(&pdf, 2);
        assert!(String::from_utf8_lossy(&pages).contains("/Kids [3 0 R] /Count 1"));
        let page = obj_at(&pdf, 3);
        assert!(String::from_utf8_lossy(&page)
            .contains("/Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R"));
        let content = obj_at(&pdf, 4);
        assert!(String::from_utf8_lossy(&dict_bytes(&content)).contains("/Filter /FlateDecode"));
        assert_eq!(stream_payload(&content), data.to_vec());
    }

    #[test]
    fn test_build_text_pdf_embeds_literal_text() {
        let pdf = PdfVerifier::build_text_pdf("nonce-abc-123");
        let content = obj_at(&pdf, 4);
        assert_eq!(
            stream_payload(&content),
            b"BT /F1 12 Tf 72 720 Td (nonce-abc-123) Tj ET".to_vec()
        );
    }

    #[test]
    fn test_pdf_stage1_fixture_contract() {
        let v = PdfVerifier::new(1, Some("abc"));
        let (pdf, want) = v.pdf_stage1();
        assert_eq!(want, "Hello, SubDollar abc!");
        assert_eq!(
            stream_payload(&obj_at(&pdf, 4)),
            b"BT /F1 12 Tf 72 720 Td (Hello, SubDollar abc!) Tj ET".to_vec()
        );

        // Unseeded runs use the "live" default prefix.
        let (_, want) = PdfVerifier::new(1, None).pdf_stage1();
        assert_eq!(want, "Hello, SubDollar live!");
    }

    #[test]
    fn test_pdf_stage2_fixture_contract() {
        let v = PdfVerifier::new(1, Some("abc"));
        let (pdf, want) = v.pdf_stage2();
        assert_eq!(want, "Hello, abc!\nHiA(B)");
        assert_eq!(
            stream_payload(&obj_at(&pdf, 4)),
            b"BT /F1 12 Tf 72 720 Td (Hello, ) Tj [(abc) 120 (!)] TJ 0 -14 Td <4869> Tj (A\\(B\\)) Tj ET"
                .to_vec()
        );
    }

    #[test]
    fn test_pdf_stage3_fixture_decompresses_to_exact_text() {
        let v = PdfVerifier::new(1, Some("abc"));
        let (pdf, want) = v.pdf_stage3();
        assert_eq!(want, "Compressed stream abc works!");

        let content = obj_at(&pdf, 4);
        assert!(String::from_utf8_lossy(&dict_bytes(&content)).contains("/Filter /FlateDecode"));
        assert_eq!(
            deflate(&stream_payload(&content)),
            b"BT /F1 12 Tf 72 720 Td (Compressed stream abc works!) Tj ET".to_vec()
        );
    }

    #[test]
    fn test_pdf_stage4_multipage_layout_and_decoy() {
        let v = PdfVerifier::new(1, Some("abc"));
        let (pdf, want) = v.pdf_stage4();
        assert_eq!(want, "Page one abc\nPage two abc\nPage three abc");

        let pages = obj_at(&pdf, 2);
        assert!(String::from_utf8_lossy(&pages).contains("/Kids [3 0 R 4 0 R 5 0 R] /Count 3"));
        for (n, expected, y) in [
            (6u32, "Page one abc", 720),
            (7, "Page two abc", 700),
            (8, "Page three abc", 680),
        ] {
            let content = obj_at(&pdf, n);
            assert_eq!(
                stream_payload(&content),
                format!("BT /F1 12 Tf 72 {y} Td ({expected}) Tj ET").into_bytes(),
                "content stream {n}"
            );
        }

        // The decoy object exists and carries its text but is never linked
        // from the page tree and never leaks into the expected output.
        let decoy = obj_at(&pdf, 9);
        let decoy_text = String::from_utf8_lossy(&decoy).to_string();
        assert!(decoy_text.contains("DECOY-") && decoy_text.contains("must not appear"));
        assert!(!want.contains("DECOY"));
        assert!(!String::from_utf8_lossy(&pages).contains("9 0 R"));
    }

    #[test]
    fn test_short_truncates_at_char_boundaries() {
        assert_eq!(PdfVerifier::short(""), "");
        assert_eq!(PdfVerifier::short("abc"), "abc");

        let exactly = "a".repeat(160);
        assert_eq!(
            PdfVerifier::short(&exactly),
            exactly,
            "160 chars need no ellipsis"
        );

        let over = format!("{exactly}b");
        let s = PdfVerifier::short(&over);
        assert_eq!(s, format!("{exactly}..."));
        assert_eq!(s.chars().count(), 163);

        // A multibyte char at the 160-char boundary must stay intact.
        let mb = format!("{}é{}", "a".repeat(159), "b");
        assert_eq!(PdfVerifier::short(&mb), format!("{}é...", "a".repeat(159)));

        let snow = "☃".repeat(200);
        assert_eq!(PdfVerifier::short(&snow), format!("{}...", "☃".repeat(160)));
    }

    #[tokio::test]
    async fn test_post_extract_wire_contract() {
        let (port, captured) = spawn_http_mock(|_| (200, b"extracted".to_vec())).await;
        let v = PdfVerifier::new(port, Some("wireseed"));
        let (pdf, _) = v.pdf_stage1();

        let resp = v.post_extract(&pdf).await.expect("mock must answer");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"extracted");

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/extract");
        assert_eq!(
            reqs[0].headers.get("content-type").map(String::as_str),
            Some("application/pdf")
        );
        assert_eq!(reqs[0].body, pdf);
    }

    #[tokio::test]
    async fn test_post_extract_unreachable_reports_context() {
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);

        let v = PdfVerifier::new(port, None);
        let err = v.post_extract(b"%PDF-1.4").await.unwrap_err();
        assert!(err.contains("POST /extract failed"), "{err}");
    }

    #[tokio::test]
    async fn test_check_extract_pass_mismatch_status_and_body_error() {
        // Byte-exact match: pass with no error.
        let (port, _) = spawn_http_mock(|_| (200, b"right".to_vec())).await;
        let v = PdfVerifier::new(port, None);
        let r = v.check_extract(7, "check", vec![], "right").await;
        assert_eq!(
            r,
            StageResult {
                stage: 7,
                name: "check".to_string(),
                passed: true,
                error: None,
            }
        );

        // Wrong body: both sides are reported.
        let (port, _) = spawn_http_mock(|_| (200, b"WRONG".to_vec())).await;
        let v = PdfVerifier::new(port, None);
        let r = v.check_extract(8, "check", vec![], "right").await;
        assert!(!r.passed);
        assert_eq!(
            r.error.as_deref(),
            Some("mismatch: got \"WRONG\", want \"right\"")
        );

        // Non-200: status is the error, body is not read.
        let (port, _) = spawn_http_mock(|_| (500, b"boom".to_vec())).await;
        let v = PdfVerifier::new(port, None);
        let r = v.check_extract(9, "check", vec![], "right").await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("status 500 Internal Server Error"));

        // Truncated Content-Length: the body read itself must fail.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut tmp = [0u8; 1024];
                let _ = sock.read(&mut tmp).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nConnection: close\r\n\r\nshort",
                    )
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        let v = PdfVerifier::new(port, None);
        let r = v.check_extract(10, "check", vec![], "anything").await;
        assert!(!r.passed);
        assert!(
            r.error.as_deref().unwrap_or("").contains("read body"),
            "truncated body must be reported: {:?}",
            r.error
        );
    }

    #[tokio::test]
    async fn test_stage_wrappers_pass_with_expected_bodies() {
        let v = PdfVerifier::new(0, Some("stagewrap"));
        let queue = response_queue(vec![
            v.pdf_stage1().1.into_bytes(),
            v.pdf_stage2().1.into_bytes(),
            v.pdf_stage3().1.into_bytes(),
            v.pdf_stage4().1.into_bytes(),
        ]);
        let (port, captured) = spawn_http_mock(move |_| {
            let body = queue.lock().unwrap().pop_front().unwrap_or_default();
            (200, body)
        })
        .await;
        let v = PdfVerifier::new(port, Some("stagewrap"));

        let checks = [
            (
                v.test_stage1_single_tj().await,
                1,
                "Stage 1: Single-page Tj extraction",
            ),
            (
                v.test_stage2_positioning().await,
                2,
                "Stage 2: TJ arrays, positioning, hex, escapes",
            ),
            (
                v.test_stage3_flate().await,
                3,
                "Stage 3: FlateDecode stream",
            ),
            (
                v.test_stage4_multipage().await,
                4,
                "Stage 4: Multi-page with decoy object",
            ),
        ];
        for (result, stage, name) in checks {
            assert!(result.passed, "stage {stage}: {:?}", result.error);
            assert_eq!(result.stage, stage);
            assert_eq!(result.name, name);
            assert_eq!(result.error, None);
        }
        assert_eq!(captured.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn test_stage5_bad_input_passes_on_faithful_mock() {
        let nonce_a = b"nonce-a-pdfdyn".to_vec();
        let nonce_b = b"nonce-b-pdfdyn".to_vec();
        let (ha, hb) = (nonce_a.clone(), nonce_b.clone());
        let (port, captured) =
            spawn_http_mock(move |req| match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/") => (200, b"<html>extract</html>".to_vec()),
                ("GET", _) => (404, b"nope".to_vec()),
                ("POST", "/extract") => {
                    if !req.body.starts_with(b"%PDF") {
                        (400, b"not a pdf".to_vec())
                    } else if contains(&req.body, &ha) {
                        (200, ha.clone())
                    } else if contains(&req.body, &hb) {
                        (200, hb.clone())
                    } else {
                        (200, b"nothing".to_vec())
                    }
                }
                _ => (404, b"nope".to_vec()),
            })
            .await;

        let v = PdfVerifier::new(port, None);
        let r = v.test_stage5_bad_input().await;
        assert!(
            r.passed,
            "faithful routing+nonce mock must pass: {:?}",
            r.error
        );
        assert_eq!(r.stage, 5);
        assert_eq!(r.name, "Stage 5: Bad input and routing");

        let reqs = captured.lock().unwrap();
        assert_eq!(
            reqs.len(),
            5,
            "garbage, GET /, GET /nope, two dynamic POSTs"
        );
        assert_eq!(reqs[0].path, "/extract");
        assert_eq!(reqs[1].path, "/");
        assert_eq!(reqs[2].path, "/nope");
        assert!(contains(&reqs[3].body, &nonce_a));
        assert!(contains(&reqs[4].body, &nonce_b));
        assert_ne!(reqs[3].body, reqs[4].body);
    }

    #[tokio::test]
    async fn test_stage5_bad_input_failure_branches() {
        // Garbage accepted with 200 instead of rejected with 400.
        let (port, _) = spawn_http_mock(|req| {
            if req.method == "POST" {
                (200, b"accepted".to_vec())
            } else {
                (200, b"ok".to_vec())
            }
        })
        .await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("garbage POST status 200 OK"));

        // Unknown routes must 404.
        let (port, _) = spawn_http_mock(|req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", _) if !req.body.starts_with(b"%PDF") => (400, b"bad".to_vec()),
            ("POST", _) => (200, b"fake".to_vec()),
            ("GET", "/") => (200, b"ok".to_vec()),
            _ => (200, b"swallowed".to_vec()),
        })
        .await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert_eq!(r.error.as_deref(), Some("GET /nope status 200 OK"));

        // Distinct PDFs answered with an identical body that contains both
        // nonces: the canned-extraction branch must still reject it.
        let (port, _) = spawn_http_mock(|req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", _) if !req.body.starts_with(b"%PDF") => (400, b"bad".to_vec()),
            ("POST", _) => (200, b"nonce-a-pdfdyn and nonce-b-pdfdyn".to_vec()),
            ("GET", "/") => (200, b"ok".to_vec()),
            _ => (404, b"nope".to_vec()),
        })
        .await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert_eq!(
            r.error.as_deref(),
            Some("canned extraction: distinct inputs, identical output")
        );

        // Dynamic bodies that carry the wrong text fail before the equality check.
        let (port, _) = spawn_http_mock(|req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", _) if !req.body.starts_with(b"%PDF") => (400, b"bad".to_vec()),
            ("POST", _) => (200, b"nothing to see".to_vec()),
            ("GET", "/") => (200, b"ok".to_vec()),
            _ => (404, b"nope".to_vec()),
        })
        .await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .contains("dynamic extraction mismatch"),
            "unexpected error: {:?}",
            r.error
        );

        // The root route must answer 200 after the garbage POST.
        let (port, _) = spawn_http_mock(|req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", _) if !req.body.starts_with(b"%PDF") => (400, b"bad".to_vec()),
            ("GET", "/") => (503, b"down".to_vec()),
            _ => (404, b"nope".to_vec()),
        })
        .await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert_eq!(
            r.error.as_deref(),
            Some("GET / status 503 Service Unavailable")
        );

        // Dynamic extracts must answer 200 too.
        let (port, _) = spawn_http_mock(|req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", _) if !req.body.starts_with(b"%PDF") => (400, b"bad".to_vec()),
            ("POST", _) => (500, b"explode".to_vec()),
            ("GET", "/") => (200, b"ok".to_vec()),
            _ => (404, b"nope".to_vec()),
        })
        .await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert_eq!(
            r.error.as_deref(),
            Some("dynamic POST A status 500 Internal Server Error")
        );

        // Dynamic POST B must answer 200 as well.
        let (port, _) = spawn_http_mock(|req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", _) if !req.body.starts_with(b"%PDF") => (400, b"bad".to_vec()),
            ("POST", _) if contains(&req.body, b"nonce-a-") => (200, b"nonce-a-pdfdyn".to_vec()),
            ("POST", _) => (500, b"explode".to_vec()),
            ("GET", "/") => (200, b"ok".to_vec()),
            _ => (404, b"nope".to_vec()),
        })
        .await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert_eq!(
            r.error.as_deref(),
            Some("dynamic POST B status 500 Internal Server Error")
        );
    }

    #[tokio::test]
    async fn test_stage5_routing_connect_errors_are_reported() {
        // Garbage POST answered, then the server vanishes before GET /.
        let port = spawn_dropping_after(vec![(400, b"bad".to_vec())]).await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert!(
            r.error.as_deref().unwrap_or("").starts_with("GET / failed"),
            "{:?}",
            r.error
        );

        // POST and GET / answered, then the server vanishes before GET /nope.
        let port = spawn_dropping_after(vec![(400, b"bad".to_vec()), (200, b"ok".to_vec())]).await;
        let r = PdfVerifier::new(port, None).test_stage5_bad_input().await;
        assert!(!r.passed);
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .starts_with("GET /nope failed"),
            "{:?}",
            r.error
        );
    }

    #[tokio::test]
    async fn test_stage6_concurrent_burst_pass_and_mismatch() {
        let want = PdfVerifier::new(0, Some("burst")).pdf_stage4().1;
        let body = want.clone().into_bytes();
        let (port, captured) = spawn_http_mock(move |_| (200, body.clone())).await;
        let r = PdfVerifier::new(port, Some("burst"))
            .test_stage6_concurrent_burst()
            .await;
        assert!(r.passed, "16 exact extractions must pass: {:?}", r.error);
        assert_eq!(r.stage, 6);
        assert_eq!(r.name, "Stage 6: Concurrent Burst (16-way)");
        assert_eq!(captured.lock().unwrap().len(), 16);

        let (port, _) = spawn_http_mock(|_| (200, b"stale body".to_vec())).await;
        let r = PdfVerifier::new(port, Some("burst"))
            .test_stage6_concurrent_burst()
            .await;
        assert!(!r.passed);
        assert_eq!(
            r.error.as_deref(),
            Some("16 of 16 burst extractions failed: body mismatch")
        );
    }

    #[tokio::test]
    async fn test_measure_throughput_pass_and_failure() {
        let want = PdfVerifier::new(0, Some("perf")).pdf_stage4().1;
        let body = want.into_bytes();
        let (port, captured) = spawn_http_mock(move |_| (200, body.clone())).await;
        let fps = PdfVerifier::new(port, Some("perf"))
            .measure_throughput()
            .await
            .expect("20 exact extractions must measure");
        assert!(fps.is_finite() && fps > 0.0, "fps={fps}");
        assert_eq!(captured.lock().unwrap().len(), 20);

        // A wrong body aborts on the first iteration.
        let (port, _) = spawn_http_mock(|_| (200, b"stale".to_vec())).await;
        assert!(PdfVerifier::new(port, Some("perf"))
            .measure_throughput()
            .await
            .is_none());

        // A non-200 likewise yields no score.
        let (port, _) = spawn_http_mock(|_| (503, Vec::new())).await;
        assert!(PdfVerifier::new(port, Some("perf"))
            .measure_throughput()
            .await
            .is_none());

        // Unreachable target yields no score (and no panic).
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        assert!(PdfVerifier::new(port, Some("perf"))
            .measure_throughput()
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_run_all_passes_against_scripted_server() {
        let v0 = PdfVerifier::new(0, Some("allpass"));
        let queue = response_queue(vec![
            v0.pdf_stage1().1.into_bytes(),
            v0.pdf_stage2().1.into_bytes(),
            v0.pdf_stage3().1.into_bytes(),
            v0.pdf_stage4().1.into_bytes(),
            b"nonce-a-allpass".to_vec(),
            b"nonce-b-allpass".to_vec(),
        ]);
        let fallback = v0.pdf_stage4().1.into_bytes();
        let (port, captured) =
            spawn_http_mock(move |req| match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/") => (200, b"<html>extract</html>".to_vec()),
                ("GET", _) => (404, b"nope".to_vec()),
                ("POST", "/extract") => {
                    if !req.body.starts_with(b"%PDF") {
                        (400, b"not a pdf".to_vec())
                    } else {
                        let next = queue.lock().unwrap().pop_front();
                        (200, next.unwrap_or_else(|| fallback.clone()))
                    }
                }
                _ => (404, b"nope".to_vec()),
            })
            .await;

        let summary = PdfVerifier::new(port, Some("allpass")).run_all().await;
        assert_eq!(summary.total_stages, 6);
        assert_eq!(
            summary.passed_count, 6,
            "every stage must pass: {:?}",
            summary.stages
        );
        assert_eq!(summary.pass_rate, 100.0);
        for (i, stage) in summary.stages.iter().enumerate() {
            assert_eq!(stage.stage, (i + 1) as u32);
            assert!(stage.passed && stage.error.is_none());
        }
        assert_eq!(summary.stages[4].name, "Stage 5: Bad input and routing");
        assert_eq!(captured.lock().unwrap().len(), 4 + 5 + 16);
    }

    #[tokio::test]
    async fn test_run_all_reports_failures_against_dead_port() {
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);

        let summary = PdfVerifier::new(port, None).run_all().await;
        assert_eq!(summary.total_stages, 6);
        assert_eq!(summary.passed_count, 0);
        assert_eq!(summary.pass_rate, 0.0);
        for (i, stage) in summary.stages.iter().enumerate() {
            assert_eq!(stage.stage, (i + 1) as u32);
            assert!(!stage.passed);
            assert!(stage.error.is_some(), "stage {} must explain itself", i + 1);
        }
        assert!(summary.stages[0]
            .error
            .as_deref()
            .unwrap()
            .contains("POST /extract failed"));
        assert!(summary.stages[4]
            .error
            .as_deref()
            .unwrap()
            .contains("garbage POST failed"));
        assert!(summary.stages[5]
            .error
            .as_deref()
            .unwrap()
            .contains("16 of 16 burst extractions failed"));
    }
}
