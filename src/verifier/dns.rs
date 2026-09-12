use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::info;

use crate::verifier::StageResult;

#[derive(Debug, Clone)]
pub struct DnsTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

#[derive(Debug, Clone)]
pub struct DnsResourceRecord {
    pub rtype: u16,
    pub rclass: u16,
    pub ttl: u32,
    pub rdata: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ParsedDnsResponse {
    pub id: u16,
    pub qr: bool,
    pub rcode: u8,
    pub ancount: u16,
    pub answers: Vec<DnsResourceRecord>,
}

pub struct DnsVerifier {
    pub target_port: u16,
    pub run_seed: Option<String>,
}

impl DnsVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        Self {
            target_port,
            run_seed: seed.map(|s| s.to_string()),
        }
    }

    pub fn get_seed_prefix<'a>(&'a self, default_val: &'a str) -> &'a str {
        super::get_seed_prefix(self.run_seed.as_deref(), default_val)
    }

    /// Parses an incoming RFC 1035 UDP response packet, validating the header and answer records.
    pub fn parse_response(buf: &[u8]) -> Result<ParsedDnsResponse, String> {
        if buf.len() < 12 {
            return Err(format!("Response too short: {} bytes (min 12)", buf.len()));
        }
        let id = u16::from_be_bytes([buf[0], buf[1]]);
        let flags = u16::from_be_bytes([buf[2], buf[3]]);
        let qr = (flags & 0x8000) != 0;
        let rcode = (flags & 0x000F) as u8;
        let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
        let ancount = u16::from_be_bytes([buf[6], buf[7]]);

        let mut offset = 12;
        // Skip Question Section
        for _ in 0..qdcount {
            while offset < buf.len() {
                let len = buf[offset] as usize;
                if len == 0 {
                    offset += 1;
                    break;
                }
                if (len & 0xC0) == 0xC0 {
                    // Compressed name pointer (2 bytes)
                    offset += 2;
                    break;
                }
                offset += 1 + len;
            }
            // Skip QTYPE (2 bytes) + QCLASS (2 bytes)
            offset += 4;
            if offset > buf.len() {
                return Err("Malformed question section in DNS response".to_string());
            }
        }

        let mut answers = Vec::new();
        // Parse Answer Section
        for _ in 0..ancount {
            if offset >= buf.len() {
                break;
            }
            // Parse Name (pointer or label sequence)
            while offset < buf.len() {
                let len = buf[offset] as usize;
                if len == 0 {
                    offset += 1;
                    break;
                }
                if (len & 0xC0) == 0xC0 {
                    offset += 2;
                    break;
                }
                offset += 1 + len;
            }
            if offset + 10 > buf.len() {
                break;
            }
            let rtype = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
            let rclass = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]);
            let ttl = u32::from_be_bytes([buf[offset + 4], buf[offset + 5], buf[offset + 6], buf[offset + 7]]);
            let rdlen = u16::from_be_bytes([buf[offset + 8], buf[offset + 9]]) as usize;
            offset += 10;
            if offset + rdlen > buf.len() {
                break;
            }
            let rdata = buf[offset..offset + rdlen].to_vec();
            offset += rdlen;
            answers.push(DnsResourceRecord {
                rtype,
                rclass,
                ttl,
                rdata,
            });
        }

        Ok(ParsedDnsResponse {
            id,
            qr,
            rcode,
            ancount,
            answers,
        })
    }

    /// Dynamically probes the candidate DNS UDP server port until it responds or times out.
    pub async fn wait_for_ready(&self, timeout_secs: u64) -> bool {
        let start = std::time::Instant::now();
        let timeout_dur = Duration::from_secs(timeout_secs);
        while start.elapsed() < timeout_dur {
            if let Ok(socket) = UdpSocket::bind("127.0.0.1:0").await {
                let target: Result<SocketAddr, _> =
                    format!("127.0.0.1:{}", self.target_port).parse();
                if let Ok(target_addr) = target {
                    let test_id = rand::random::<u16>();
                    let query = Self::build_query(test_id, "localhost", 1);
                    if socket.send_to(&query, target_addr).await.is_ok() {
                        let mut buf = [0u8; 512];
                        if let Ok(Ok((len, _))) =
                            timeout(Duration::from_millis(300), socket.recv_from(&mut buf)).await
                        {
                            if len >= 12 {
                                return true;
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }

    pub async fn run_all(&self) -> DnsTestSummary {
        info!(
            "Starting hardened DNS RFC 1035 verification suite on port {}",
            self.target_port
        );
        let mut stages = Vec::new();

        stages.push(self.test_stage1_handshake().await);
        stages.push(self.test_stage2_example_com().await);
        stages.push(self.test_stage3_localhost().await);
        stages.push(self.test_stage4_txt_record().await);
        stages.push(self.test_stage5_nxdomain().await);
        stages.push(self.test_stage6_concurrency().await);

        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;

        DnsTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    /// Timed burst of sequential queries, scored in queries/sec.
    /// Strictly verifies dynamic query transaction IDs and valid Answer records.
    pub async fn measure_throughput(&self) -> Option<f64> {
        let socket = UdpSocket::bind("127.0.0.1:0").await.ok()?;
        let target: SocketAddr = format!("127.0.0.1:{}", self.target_port).parse().ok()?;
        let iters = 200u32;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let id = rand::random::<u16>();
            let query = Self::build_query(id, "example.com", 1);
            socket.send_to(&query, target).await.ok()?;
            let mut buf = [0u8; 512];
            let resp = timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await;
            match resp {
                Ok(Ok((len, _))) if len >= 12 => {
                    let parsed = Self::parse_response(&buf[..len]).ok()?;
                    if parsed.id != id || !parsed.qr {
                        return None;
                    }
                    let has_ip = parsed.answers.iter().any(|a| a.rtype == 1 && a.rdata == [93, 184, 216, 34])
                        || buf[..len].windows(4).any(|w| w == [93, 184, 216, 34]);
                    if !has_ip {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        let secs = start.elapsed().as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        Some(iters as f64 / secs)
    }

    async fn send_query(&self, id: u16, domain: &str, qtype: u16) -> Result<Vec<u8>, String> {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind client socket: {}", e))?;
        let target: SocketAddr = format!("127.0.0.1:{}", self.target_port)
            .parse()
            .map_err(|e| format!("Invalid target address: {}", e))?;

        let query = Self::build_query(id, domain, qtype);
        socket
            .send_to(&query, target)
            .await
            .map_err(|e| format!("Failed to send query: {}", e))?;

        let mut buf = vec![0u8; 1024];
        let (len, _) = timeout(Duration::from_millis(1500), socket.recv_from(&mut buf))
            .await
            .map_err(|_| "Timeout waiting for DNS response".to_string())?
            .map_err(|e| format!("Error receiving DNS response: {}", e))?;

        buf.truncate(len);
        Ok(buf)
    }

    pub fn build_query(id: u16, domain: &str, qtype: u16) -> Vec<u8> {
        let mut packet = Vec::with_capacity(64);
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(&[0x01, 0x00]); // Standard query, RD=1
        packet.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
        packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        for label in domain.split('.') {
            if !label.is_empty() {
                packet.push(label.len() as u8);
                packet.extend_from_slice(label.as_bytes());
            }
        }
        packet.push(0x00);
        packet.extend_from_slice(&qtype.to_be_bytes());
        packet.extend_from_slice(&[0x00, 0x01]); // QCLASS=IN
        packet
    }

    pub async fn test_stage1_handshake(&self) -> StageResult {
        let name = "Stage 1: Header Handshake & Valid Query Response".to_string();
        let tx_id = rand::random::<u16>();
        match self.send_query(tx_id, "example.com", 1).await {
            Ok(resp) => match Self::parse_response(&resp) {
                Ok(parsed) => {
                    if parsed.id != tx_id {
                        return StageResult {
                            stage: 1,
                            name,
                            passed: false,
                            error: Some(format!(
                                "Transaction ID mismatch: sent 0x{:04x}, got 0x{:04x}",
                                tx_id, parsed.id
                            )),
                        };
                    }
                    if !parsed.qr {
                        return StageResult {
                            stage: 1,
                            name,
                            passed: false,
                            error: Some("Response QR bit not set (flags indicate query)".to_string()),
                        };
                    }
                    StageResult {
                        stage: 1,
                        name,
                        passed: true,
                        error: None,
                    }
                }
                Err(e) => StageResult {
                    stage: 1,
                    name,
                    passed: false,
                    error: Some(e),
                },
            },
            Err(e) => StageResult {
                stage: 1,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    pub async fn test_stage2_example_com(&self) -> StageResult {
        let name = "Stage 2: example.com A Record Resolution (93.184.216.34)".to_string();
        let tx_id = rand::random::<u16>();
        match self.send_query(tx_id, "example.com", 1).await {
            Ok(resp) => match Self::parse_response(&resp) {
                Ok(parsed) => {
                    if parsed.id != tx_id {
                        return StageResult {
                            stage: 2,
                            name,
                            passed: false,
                            error: Some(format!("Transaction ID mismatch: sent 0x{:04x}, got 0x{:04x}", tx_id, parsed.id)),
                        };
                    }
                    let has_a_record = parsed.answers.iter().any(|a| a.rtype == 1 && a.rdata == [93, 184, 216, 34])
                        || (parsed.ancount >= 1 && resp.windows(4).any(|w| w == [93, 184, 216, 34]));
                    if has_a_record {
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
                            error: Some(format!(
                                "Expected A record with IP 93.184.216.34, got ancount={}, answers={:?}",
                                parsed.ancount, parsed.answers
                            )),
                        }
                    }
                }
                Err(e) => StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(e),
                },
            },
            Err(e) => StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    pub async fn test_stage3_localhost(&self) -> StageResult {
        let name = "Stage 3: localhost A Record Resolution (127.0.0.1)".to_string();
        let tx_id = rand::random::<u16>();
        match self.send_query(tx_id, "localhost", 1).await {
            Ok(resp) => match Self::parse_response(&resp) {
                Ok(parsed) => {
                    if parsed.id != tx_id {
                        return StageResult {
                            stage: 3,
                            name,
                            passed: false,
                            error: Some(format!("Transaction ID mismatch: sent 0x{:04x}, got 0x{:04x}", tx_id, parsed.id)),
                        };
                    }
                    let has_local = parsed.answers.iter().any(|a| a.rtype == 1 && a.rdata == [127, 0, 0, 1])
                        || (parsed.ancount >= 1 && resp.windows(4).any(|w| w == [127, 0, 0, 1]));
                    if has_local {
                        StageResult {
                            stage: 3,
                            name,
                            passed: true,
                            error: None,
                        }
                    } else {
                        StageResult {
                            stage: 3,
                            name,
                            passed: false,
                            error: Some(format!("Expected A record 127.0.0.1, got ancount={}", parsed.ancount)),
                        }
                    }
                }
                Err(e) => StageResult {
                    stage: 3,
                    name,
                    passed: false,
                    error: Some(e),
                },
            },
            Err(e) => StageResult {
                stage: 3,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    pub async fn test_stage4_txt_record(&self) -> StageResult {
        let name = "Stage 4: test.example.com TXT Record Query".to_string();
        let tx_id = rand::random::<u16>();
        match self.send_query(tx_id, "test.example.com", 16).await {
            Ok(resp) => match Self::parse_response(&resp) {
                Ok(parsed) => {
                    if parsed.id != tx_id {
                        return StageResult {
                            stage: 4,
                            name,
                            passed: false,
                            error: Some(format!("Transaction ID mismatch: sent 0x{:04x}, got 0x{:04x}", tx_id, parsed.id)),
                        };
                    }
                    let s = String::from_utf8_lossy(&resp);
                    let has_txt = parsed.answers.iter().any(|a| a.rtype == 16 && String::from_utf8_lossy(&a.rdata).contains("subdollar-benchmark"))
                        || s.contains("subdollar-benchmark");
                    if has_txt {
                        StageResult {
                            stage: 4,
                            name,
                            passed: true,
                            error: None,
                        }
                    } else {
                        StageResult {
                            stage: 4,
                            name,
                            passed: false,
                            error: Some("TXT record payload 'subdollar-benchmark' not found".to_string()),
                        }
                    }
                }
                Err(e) => StageResult {
                    stage: 4,
                    name,
                    passed: false,
                    error: Some(e),
                },
            },
            Err(e) => StageResult {
                stage: 4,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    pub async fn test_stage5_nxdomain(&self) -> StageResult {
        let name = "Stage 5: Unknown Domain NXDOMAIN / Error Response".to_string();
        let tx_id = rand::random::<u16>();
        match self.send_query(tx_id, "nonexistent.subdollar.invalid", 1).await {
            Ok(resp) => match Self::parse_response(&resp) {
                Ok(parsed) => {
                    if parsed.id != tx_id {
                        return StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(format!("Transaction ID mismatch: sent 0x{:04x}, got 0x{:04x}", tx_id, parsed.id)),
                        };
                    }
                    if parsed.rcode == 3 || parsed.ancount == 0 {
                        StageResult {
                            stage: 5,
                            name,
                            passed: true,
                            error: None,
                        }
                    } else {
                        StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(format!(
                                "Expected RCODE 3 or ANCOUNT 0, got rcode={}, ancount={}",
                                parsed.rcode, parsed.ancount
                            )),
                        }
                    }
                }
                Err(e) => StageResult {
                    stage: 5,
                    name,
                    passed: false,
                    error: Some(e),
                },
            },
            Err(e) => StageResult {
                stage: 5,
                name,
                passed: false,
                error: Some(e),
            },
        }
    }

    pub async fn test_stage6_concurrency(&self) -> StageResult {
        let name = "Stage 6: Concurrent Burst Query Handling (10 queries)".to_string();
        let mut tasks = Vec::new();

        for _ in 0..10 {
            let port = self.target_port;
            let tx_id = rand::random::<u16>();
            tasks.push(tokio::spawn(async move {
                let v = DnsVerifier::new(port, None);
                let resp = v.send_query(tx_id, "example.com", 1).await?;
                let parsed = DnsVerifier::parse_response(&resp)?;
                if parsed.id != tx_id {
                    return Err("ID mismatch".to_string());
                }
                Ok(())
            }));
        }

        let mut success = 0;
        for t in tasks {
            if let Ok(Ok(_)) = t.await {
                success += 1;
            }
        }

        if success >= 8 {
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
                    "Only {} of 10 concurrent queries succeeded with matched IDs",
                    success
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn start_mock_dns_server() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();

        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                if let Ok((len, src)) = socket.recv_from(&mut buf).await {
                    if len < 12 {
                        continue;
                    }
                    let id = &buf[0..2];
                    let qname_end = buf[12..len].iter().position(|&b| b == 0).unwrap_or(0) + 12;
                    let domain_raw = String::from_utf8_lossy(&buf[12..qname_end]);

                    let mut resp = Vec::new();
                    resp.extend_from_slice(id);

                    if domain_raw.contains("nonexistent") {
                        // NXDOMAIN (flags = 0x8183)
                        resp.extend_from_slice(&[
                            0x81, 0x83, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                        ]);
                        resp.extend_from_slice(&buf[12..len]);
                    } else if domain_raw.contains("localhost") {
                        // 127.0.0.1
                        resp.extend_from_slice(&[
                            0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                        ]);
                        resp.extend_from_slice(&buf[12..len]);
                        resp.extend_from_slice(&[
                            0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x04,
                            127, 0, 0, 1,
                        ]);
                    } else if domain_raw.contains("test") {
                        // TXT record
                        let txt = b"subdollar-benchmark";
                        resp.extend_from_slice(&[
                            0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                        ]);
                        resp.extend_from_slice(&buf[12..len]);
                        resp.extend_from_slice(&[
                            0xc0, 0x0c, 0x00, 0x10, 0x00, 0x01, 0x00, 0x00, 0x01, 0x2c,
                        ]);
                        let rdlen = (txt.len() + 1) as u16;
                        resp.extend_from_slice(&rdlen.to_be_bytes());
                        resp.push(txt.len() as u8);
                        resp.extend_from_slice(txt);
                    } else {
                        // example.com -> 93.184.216.34
                        resp.extend_from_slice(&[
                            0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                        ]);
                        resp.extend_from_slice(&buf[12..len]);
                        resp.extend_from_slice(&[
                            0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x04,
                            93, 184, 216, 34,
                        ]);
                    }

                    let _ = socket.send_to(&resp, src).await;
                }
            }
        });

        port
    }

    #[tokio::test]
    async fn test_dns_verifier_all_stages_pass() {
        let port = start_mock_dns_server().await;
        let verifier = DnsVerifier::new(port, None);
        assert!(verifier.wait_for_ready(5).await);
        let summary = verifier.run_all().await;
        assert_eq!(
            summary.passed_count, 6,
            "Stages failed: {:?}",
            summary.stages
        );
        assert_eq!(summary.total_stages, 6);
        assert_eq!(summary.pass_rate, 100.0);
    }

    #[tokio::test]
    async fn test_dns_verifier_unreachable() {
        let verifier = DnsVerifier::new(59989, None);
        let res = verifier.test_stage1_handshake().await;
        assert!(!res.passed);
        assert!(!verifier.wait_for_ready(1).await);
    }
    #[test]
    fn test_dns_parser_handles_standard_and_rejects_truncated() {
        // Truncated packet (< 12 bytes)
        assert!(DnsVerifier::parse_response(&[0x12, 0x34]).is_err());

        // Valid 12-byte header with no questions and no answers
        let hdr = [0x12, 0x34, 0x81, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let parsed = DnsVerifier::parse_response(&hdr).unwrap();
        assert_eq!(parsed.id, 0x1234);
        assert!(parsed.qr);
        assert_eq!(parsed.ancount, 0);
    }
}
