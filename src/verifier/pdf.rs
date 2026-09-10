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

// zlib stream of: BT /F1 12 Tf 72 720 Td (Compressed stream works!) Tj ET
const STAGE3_STREAM: &[u8] = &[
    120, 156, 115, 10, 81, 208, 119, 51, 84, 48, 52, 82, 8, 73, 83, 48, 55, 2, 34, 3, 133, 144,
    20, 5, 13, 231, 252, 220, 130, 162, 212, 226, 226, 212, 20, 133, 226, 146, 162, 212, 196, 92,
    133, 242, 252, 162, 236, 98, 69, 77, 133, 144, 44, 5, 215, 16, 0, 155, 144, 16, 20,
];

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

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.target_port)
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

    fn pdf_stage1() -> (Vec<u8>, &'static str) {
        let pdf = Self::single_page_pdf(
            b"BT /F1 12 Tf 72 720 Td (Hello, SubDollar!) Tj ET",
            "",
        );
        (pdf, "Hello, SubDollar!")
    }

    fn pdf_stage2() -> (Vec<u8>, &'static str) {
        let pdf = Self::single_page_pdf(
            b"BT /F1 12 Tf 72 720 Td (Hello, ) Tj [(Wor) 120 (ld!)] TJ 0 -14 Td <4869> Tj (A\\(B\\)) Tj ET",
            "",
        );
        (pdf, "Hello, World!\nHiA(B)")
    }

    fn pdf_stage3() -> (Vec<u8>, &'static str) {
        let pdf = Self::single_page_pdf(STAGE3_STREAM, "/Filter /FlateDecode");
        (pdf, "Compressed stream works!")
    }

    fn pdf_stage4() -> (Vec<u8>, &'static str) {
        let objs = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 6 0 R >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 7 0 R >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 8 0 R >>".to_vec(),
            Self::stream_obj("", b"BT /F1 12 Tf 72 720 Td (Page one) Tj ET"),
            Self::stream_obj("", b"BT /F1 12 Tf 72 700 Td (Page two) Tj ET"),
            Self::stream_obj("", b"BT /F1 12 Tf 72 680 Td (Page three) Tj ET"),
            // Decoy: valid object, never referenced from the page tree.
            b"<< /Note (internal) >>\n(DECOY-7741 must not appear) Tj".to_vec(),
        ];
        (Self::build_pdf(&objs, 1), "Page one\nPage two\nPage three")
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
        const MAX: usize = 160;
        if s.len() <= MAX {
            s.to_string()
        } else {
            format!("{}...", &s[..MAX])
        }
    }

    async fn check_extract(&self, stage: u32, name: &str, pdf: Vec<u8>, expected: &str) -> StageResult {
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
        let (pdf, want) = Self::pdf_stage1();
        self.check_extract(1, "Stage 1: Single-page Tj extraction", pdf, want)
            .await
    }

    async fn test_stage2_positioning(&self) -> StageResult {
        let (pdf, want) = Self::pdf_stage2();
        self.check_extract(2, "Stage 2: TJ arrays, positioning, hex, escapes", pdf, want)
            .await
    }

    async fn test_stage3_flate(&self) -> StageResult {
        let (pdf, want) = Self::pdf_stage3();
        self.check_extract(3, "Stage 3: FlateDecode stream", pdf, want).await
    }

    async fn test_stage4_multipage(&self) -> StageResult {
        let (pdf, want) = Self::pdf_stage4();
        self.check_extract(4, "Stage 4: Multi-page with decoy object", pdf, want)
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
                    return done(false, Some(format!("garbage POST status {}", resp.status())));
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
        done(true, None)
    }

    pub async fn run_all(&self) -> PdfTestSummary {
        let mut stages = Vec::new();
        stages.push(self.test_stage1_single_tj().await);
        stages.push(self.test_stage2_positioning().await);
        stages.push(self.test_stage3_flate().await);
        stages.push(self.test_stage4_multipage().await);
        stages.push(self.test_stage5_bad_input().await);

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
        let (pdf, want) = Self::pdf_stage4();
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
        // Every live xref entry must point at its own "<n> 0 obj" header.
        let (pdf, want) = PdfVerifier::pdf_stage4();
        assert_eq!(want, "Page one\nPage two\nPage three");
        for num in 1..=9u32 {
            let off = find_obj_offset(&pdf, num).expect("xref entry");
            let head = format!("{} 0 obj", num);
            assert!(
                pdf[off..].starts_with(head.as_bytes()),
                "object {} mispointed",
                num
            );
        }
    }

    fn adler32(data: &[u8]) -> u32 {
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for byte in data {
            a = (a + *byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    #[test]
    fn test_pdf_stage3_fixture_is_valid_zlib() {
        // Without a flate dependency: check zlib header + Adler-32 trailer
        // over the expected raw content stream.
        let raw = b"BT /F1 12 Tf 72 720 Td (Compressed stream works!) Tj ET";
        assert_eq!(STAGE3_STREAM[0], 120);
        assert_eq!(STAGE3_STREAM[1], 156);
        let n = STAGE3_STREAM.len();
        let want = adler32(raw);
        let got = ((STAGE3_STREAM[n - 4] as u32) << 24)
            | ((STAGE3_STREAM[n - 3] as u32) << 16)
            | ((STAGE3_STREAM[n - 2] as u32) << 8)
            | (STAGE3_STREAM[n - 1] as u32);
        assert_eq!(got, want);
    }
}
