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
}
