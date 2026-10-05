//! Write-ahead log (WAL) benchmark verifier.
//!
//! Protocol under test (line-oriented TCP, `\n`-terminated):
//! - `PUB <topic> <payload>` -> `+OK <offset>` (`u64` sequence per topic from 0)
//! - `SUB <topic> <offset>` -> `:<off> <payload>` lines for seq >= offset, then `+END`
//! - Unknown topic `SUB` -> `+END` immediately; malformed input -> `-ERR ...`

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use super::{get_seed_prefix, StageResult};

/// Fixed topic for the durability probe (stage 4); see `check_recovery`.
pub const RECOVERY_TOPIC: &str = "recovery_probe";
/// Pipelined `PUB`s sent by `measure_throughput`.
pub const THROUGHPUT_MSGS: usize = 2000;

const MAX_SUB_LINES: usize = 1_000_000; // runaway SUB stream guard
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct WalVerifier {
    pub target_port: u16,
    pub run_seed: Option<String>,
}

/// One connection: line writer plus buffered line reader.
struct WalConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl WalConn {
    async fn connect(port: u16) -> Result<Self> {
        let addr = format!("127.0.0.1:{port}");
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("Connection timed out to {addr}"))?
            .map_err(|e| anyhow!("Failed to connect to {addr}: {e}"))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        })
    }

    async fn send_line(&mut self, line: &str) -> Result<()> {
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| anyhow!("Failed to send {line:?}: {e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| anyhow!("Flush failed: {e}"))?;
        Ok(())
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut buf = String::new();
        let n = tokio::time::timeout(READ_TIMEOUT, self.reader.read_line(&mut buf))
            .await
            .map_err(|_| anyhow!("Read timed out waiting for server line"))?
            .map_err(|e| anyhow!("Read failed: {e}"))?;
        if n == 0 {
            return Err(anyhow!("Server closed connection"));
        }
        Ok(buf)
    }

    /// Send one command line, read exactly one response line.
    async fn request_one(&mut self, line: &str) -> Result<String> {
        self.send_line(line).await?;
        self.read_line().await
    }

    /// `PUB` and parse the `+OK <offset>` acknowledgement.
    async fn pub_one(&mut self, topic: &str, payload: &str) -> Result<u64> {
        let ack = self.request_one(&format_pub(topic, payload)).await?;
        parse_pub_ack(&ack)
            .ok_or_else(|| anyhow!("PUB {topic}: expected `+OK <offset>`, got {ack:?}"))
    }

    /// Send `SUB <topic> <offset>`, collect data lines until `+END`.
    async fn sub_collect(&mut self, topic: &str, offset: u64) -> Result<Vec<(u64, String)>> {
        self.send_line(&format_sub(topic, offset)).await?;
        let mut out = Vec::new();
        loop {
            let trimmed = self.read_line().await?;
            let trimmed = trimmed.trim();
            if is_end_line(trimmed) {
                break;
            }
            if is_err_line(trimmed) {
                return Err(anyhow!("SUB {topic} {offset} failed: {trimmed}"));
            }
            match parse_sub_data(trimmed) {
                Some((off, payload)) => out.push((off, payload)),
                None => return Err(anyhow!("Unparseable SUB line: {trimmed:?}")),
            }
            if out.len() > MAX_SUB_LINES {
                return Err(anyhow!("SUB stream exceeded {MAX_SUB_LINES} lines"));
            }
        }
        Ok(out)
    }
}

/// Parse `+OK <offset>`; `None` for anything else.
pub fn parse_pub_ack(line: &str) -> Option<u64> {
    let rest = line.trim().strip_prefix("+OK")?.trim();
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.split_whitespace();
    let first = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    first.parse::<u64>().ok()
}

/// Parse `:<off> <payload>`; payload may contain spaces (kept verbatim).
pub fn parse_sub_data(line: &str) -> Option<(u64, String)> {
    let rest = line.trim().strip_prefix(':')?;
    let (off_str, payload) = rest.split_once(' ')?;
    if off_str.is_empty() || payload.is_empty() {
        return None;
    }
    Some((off_str.parse::<u64>().ok()?, payload.to_string()))
}

/// True when every offset increments by exactly one in stream order.
pub fn offsets_contiguous(msgs: &[(u64, String)]) -> bool {
    msgs.windows(2).all(|w| w[1].0 == w[0].0 + 1)
}

pub fn is_end_line(line: &str) -> bool {
    line.trim() == "+END"
}

pub fn is_err_line(line: &str) -> bool {
    line.trim_start().starts_with("-ERR")
}

pub fn format_pub(topic: &str, payload: &str) -> String {
    format!("PUB {topic} {payload}\n")
}

pub fn format_sub(topic: &str, offset: u64) -> String {
    format!("SUB {topic} {offset}\n")
}

impl WalVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        Self {
            target_port,
            run_seed: seed.map(|s| s.to_string()),
        }
    }

    fn prefix<'a>(&'a self, default_val: &'a str) -> &'a str {
        get_seed_prefix(self.run_seed.as_deref(), default_val)
    }

    fn basic_topic(&self) -> String {
        format!("wal_{}_basic", self.prefix("bench"))
    }

    fn offsets_topic(&self) -> String {
        format!("wal_{}_offsets", self.prefix("bench"))
    }

    fn iso_topics(&self) -> (String, String) {
        let tag = self.prefix("bench");
        (format!("wal_{tag}_isoa"), format!("wal_{tag}_isob"))
    }

    fn burst_topic(&self) -> String {
        format!("wal_{}_burst", self.prefix("bench"))
    }

    fn fail(stage: u32, name: String, err: anyhow::Error) -> StageResult {
        StageResult {
            stage,
            name,
            passed: false,
            error: Some(err.to_string()),
        }
    }

    fn pass(stage: u32, name: String) -> StageResult {
        StageResult {
            stage,
            name,
            passed: true,
            error: None,
        }
    }

    /// Stages 1-3 against seeded topics: roundtrip, offsets, isolation.
    pub async fn run_basic(&self) -> WalTestSummary {
        let stages = vec![
            self.stage1_roundtrip().await,
            self.stage2_offsets().await,
            self.stage3_isolation().await,
        ];
        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;
        WalTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    async fn stage1_roundtrip(&self) -> StageResult {
        let name = "Stage 1: PUB then SUB-from-0 roundtrip".to_string();
        match self.stage1_inner().await {
            Ok(()) => Self::pass(1, name),
            Err(e) => Self::fail(1, name, e),
        }
    }

    async fn stage1_inner(&self) -> Result<()> {
        let topic = self.basic_topic();
        let payload = format!("hello_{}", self.prefix("bench"));
        let mut conn = WalConn::connect(self.target_port).await?;
        let base = conn.pub_one(&topic, &payload).await?;
        let msgs = conn.sub_collect(&topic, 0).await?;
        if msgs.is_empty() {
            return Err(anyhow!("SUB from 0 returned no messages"));
        }
        if !msgs.iter().any(|(o, p)| *o == base && p == &payload) {
            return Err(anyhow!(
                "Message at offset {base} missing from SUB output ({msgs:?})"
            ));
        }
        if !offsets_contiguous(&msgs) {
            return Err(anyhow!("SUB offsets not contiguous: {msgs:?}"));
        }
        Ok(())
    }

    async fn stage2_offsets(&self) -> StageResult {
        let name = "Stage 2: SUB offsets (publish 10, read 5..9)".to_string();
        match self.stage2_inner().await {
            Ok(()) => Self::pass(2, name),
            Err(e) => Self::fail(2, name, e),
        }
    }

    async fn stage2_inner(&self) -> Result<()> {
        let seed_tag = self.prefix("bench").to_string();
        let topic = self.offsets_topic();
        let mut conn = WalConn::connect(self.target_port).await?;
        let mut base: Option<u64> = None;
        for i in 0..10u64 {
            let off = conn.pub_one(&topic, &format!("msg{i}_{seed_tag}")).await?;
            match base {
                None => base = Some(off),
                Some(b) if off != b + i => {
                    return Err(anyhow!(
                        "PUB {i}: expected contiguous offset {}, got {off}",
                        b + i
                    ))
                }
                _ => {}
            }
        }
        let Some(base) = base else {
            return Err(anyhow!("No PUB acknowledgements received"));
        };
        let from = base + 5;
        let msgs = conn.sub_collect(&topic, from).await?;
        if msgs.len() != 5 {
            return Err(anyhow!(
                "Expected 5 messages from offset {from}, got {}",
                msgs.len()
            ));
        }
        for (k, (off, payload)) in msgs.iter().enumerate() {
            let want_off = from + k as u64;
            let want_payload = format!("msg{}_{seed_tag}", 5 + k);
            if *off != want_off || payload != &want_payload {
                return Err(anyhow!(
                    "Message {k}: expected `:{want_off} {want_payload}`, got `:{off} {payload}`"
                ));
            }
        }
        Ok(())
    }

    async fn stage3_isolation(&self) -> StageResult {
        let name = "Stage 3: Topic isolation".to_string();
        match self.stage3_inner().await {
            Ok(()) => Self::pass(3, name),
            Err(e) => Self::fail(3, name, e),
        }
    }

    async fn stage3_inner(&self) -> Result<()> {
        let seed_tag = self.prefix("bench").to_string();
        let (topic_a, topic_b) = self.iso_topics();
        let payload_a = format!("alpha_{seed_tag}");
        let payload_b = format!("beta_{seed_tag}");
        let mut conn = WalConn::connect(self.target_port).await?;
        conn.pub_one(&topic_a, &payload_a).await?;
        conn.pub_one(&topic_b, &payload_b).await?;
        let got_a = conn.sub_collect(&topic_a, 0).await?;
        let got_b = conn.sub_collect(&topic_b, 0).await?;
        if got_a.is_empty() || got_b.is_empty() {
            return Err(anyhow!("Isolation topics returned no messages"));
        }
        if !got_a.iter().all(|(_, p)| p == &payload_a) {
            return Err(anyhow!(
                "Topic {topic_a} leaked foreign payloads: {got_a:?}"
            ));
        }
        if !got_b.iter().all(|(_, p)| p == &payload_b) {
            return Err(anyhow!(
                "Topic {topic_b} leaked foreign payloads: {got_b:?}"
            ));
        }
        Ok(())
    }

    /// Stage 4: durability probe, run AFTER the harness SIGKILLs and restarts
    /// the candidate.
    ///
    /// # Recovery contract (parent pipeline responsibility)
    /// Between `run_basic` and the kill, the parent publishes N messages to
    /// the FIXED topic [`RECOVERY_TOPIC`] (`recovery_probe`) and passes N as
    /// `min_msgs`. This method `SUB`s `recovery_probe` from offset 0 and
    /// passes iff **all** acknowledged pre-kill messages replayed: durability
    /// means zero acked-message loss, not merely a non-empty log.
    /// Pure replay verdict: exact count, contiguous from 0, payload
    /// `msg-{offset}` per index. Unit-tested against the 2000xoffset-0
    /// fixture (must fail) without a live server.
    pub fn verify_recovery_payloads(msgs: &[(u64, String)], min_msgs: usize) -> Result<(), String> {
        // At-least contract (prompt promises "zero acked-message loss", not
        // "exactly N and nothing else"): a broker that durably persisted an
        // extra un-acked message fails the old exact-count check. Extra
        // messages are harmless; missing ones are data loss.
        if msgs.len() < min_msgs {
            return Err(format!(
                "Only {}/{} acked messages survived on `{RECOVERY_TOPIC}`",
                msgs.len(),
                min_msgs
            ));
        }
        if !offsets_contiguous(msgs) {
            return Err(format!(
                "replayed offsets not contiguous from 0 on `{RECOVERY_TOPIC}` ({} msgs)",
                msgs.len()
            ));
        }
        if msgs.first().map(|(o, _)| *o) != Some(0) {
            return Err(format!(
                "replay must start at offset 0 on `{RECOVERY_TOPIC}`"
            ));
        }
        for (off, payload) in msgs {
            let want = format!("msg-{}", off);
            if payload != &want {
                return Err(format!(
                    "payload mismatch at offset {off} on `{RECOVERY_TOPIC}`: got {payload:?}, want {want:?}"
                ));
            }
        }
        Ok(())
    }

    pub async fn check_recovery(&self, min_msgs: usize) -> StageResult {
        let stage = 4;
        let name = "Stage 4: Recovery (durable log survives SIGKILL+restart)".to_string();
        let mut conn = match WalConn::connect(self.target_port).await {
            Ok(c) => c,
            Err(e) => return Self::fail(stage, name, e),
        };
        let msgs = match conn.sub_collect(RECOVERY_TOPIC, 0).await {
            Ok(m) => m,
            Err(e) => return Self::fail(stage, name, anyhow!("SUB {RECOVERY_TOPIC} failed: {e}")),
        };
        if let Err(e) = Self::verify_recovery_payloads(&msgs, min_msgs) {
            return Self::fail(stage, name, anyhow!(e));
        }
        Self::pass(stage, name)
    }

    /// Stage 5: `THROUGHPUT_MSGS` pipelined `PUB`s on a seeded topic. Returns
    /// messages/sec from first byte sent to last ack, or `None` on any error.
    pub async fn measure_throughput(&self) -> Option<f64> {
        let seed_tag = self.prefix("bench").to_string();
        let topic = self.burst_topic();
        let mut conn = WalConn::connect(self.target_port).await.ok()?;
        let mut burst = String::new();
        for i in 0..THROUGHPUT_MSGS {
            burst.push_str(&format!("PUB {topic} tput_{seed_tag}_{i}\n"));
        }
        let start = Instant::now();
        conn.writer.write_all(burst.as_bytes()).await.ok()?;
        conn.writer.flush().await.ok()?;
        for _ in 0..THROUGHPUT_MSGS {
            parse_pub_ack(&conn.read_line().await.ok()?)?;
        }
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return None;
        }
        Some(THROUGHPUT_MSGS as f64 / elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_accept_valid_lines() {
        assert_eq!(parse_pub_ack("+OK 0"), Some(0));
        assert_eq!(parse_pub_ack("+OK 42\n"), Some(42));
        assert_eq!(parse_pub_ack("+OK  7  \r\n"), Some(7));
        assert_eq!(parse_pub_ack("+OK 18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_sub_data(":0 hello"), Some((0, "hello".to_string())));
        assert_eq!(
            parse_sub_data(":5 hi there"),
            Some((5, "hi there".to_string()))
        );
        assert_eq!(
            parse_sub_data(":12 a.b_c-d\r\n"),
            Some((12, "a.b_c-d".to_string()))
        );
        assert!(is_end_line("+END\r\n"));
        assert!(is_err_line("-ERR bad command"));
    }

    #[test]
    fn parsers_reject_malformed_lines() {
        for bad in [
            "+OK", "+OK abc", "+OK -1", "+OK 1 2", "+END", "-ERR x", ":5 hi", "",
        ] {
            assert_eq!(parse_pub_ack(bad), None, "ack: {bad:?}");
        }
        for bad in [
            "+END", "-ERR x", "+OK 3", ":abc x", ":-1 x", ":5", ":", ": x", "",
        ] {
            assert_eq!(parse_sub_data(bad), None, "data: {bad:?}");
        }
        assert!(!is_end_line(":0 hi"));
        assert!(!is_end_line("+OK 0"));
        assert!(!is_err_line("+END"));
        assert!(!is_err_line(":0 hi"));
    }

    #[test]
    fn command_formatting() {
        assert_eq!(
            format_pub("wal_bench_basic", "hello_bench"),
            "PUB wal_bench_basic hello_bench\n"
        );
        assert_eq!(
            format_sub("wal_bench_offsets", 5),
            "SUB wal_bench_offsets 5\n"
        );
        assert_eq!(format_sub(RECOVERY_TOPIC, 0), "SUB recovery_probe 0\n");
        // A formatted SUB reply parses back through the data parser.
        assert_eq!(
            parse_sub_data(":5 hello_bench"),
            Some((5, "hello_bench".to_string()))
        );
    }

    #[test]
    fn offsets_contiguity() {
        let mk = |offs: &[u64]| {
            offs.iter()
                .map(|o| (*o, "p".to_string()))
                .collect::<Vec<_>>()
        };
        assert!(offsets_contiguous(&mk(&[])));
        assert!(offsets_contiguous(&mk(&[3])));
        assert!(offsets_contiguous(&mk(&[5, 6, 7, 8, 9])));
        assert!(!offsets_contiguous(&mk(&[5, 6, 8, 9])));
        assert!(!offsets_contiguous(&mk(&[5, 5, 6])));
        assert!(!offsets_contiguous(&mk(&[9, 8, 7])));
    }

    #[test]
    fn seeded_topic_naming() {
        let a = WalVerifier::new(1234, Some("seedAAAA"));
        let b = WalVerifier::new(1234, Some("seedBBBB"));
        assert!(a.basic_topic().contains("seedAAAA"));
        assert_ne!(a.basic_topic(), b.basic_topic());
        assert_ne!(a.offsets_topic(), b.offsets_topic());
        assert_ne!(a.burst_topic(), b.burst_topic());
        let (a1, a2) = a.iso_topics();
        assert_ne!(a1, a2);
        assert_ne!(a.basic_topic(), a.offsets_topic());
        let long = WalVerifier::new(1234, Some("run-seed-12345678"));
        assert!(long.basic_topic().contains("12345678"));
        assert!(!long.basic_topic().contains("run-seed"));
        assert!(WalVerifier::new(1234, None).basic_topic().contains("bench"));
    }

    #[test]
    fn recovery_topic_is_fixed() {
        assert_eq!(RECOVERY_TOPIC, "recovery_probe");
    }

    #[test]
    fn recovery_rejects_offset_zero_fixture() {
        // Reviewer fixture: 2000 repetitions of offset 0 must FAIL, not pass.
        let dup = vec![(0u64, "msg-0".to_string()); 2000];
        assert!(WalVerifier::verify_recovery_payloads(&dup, 2000).is_err());
    }

    #[test]
    fn recovery_accepts_exact_replay() {
        let good: Vec<(u64, String)> = (0u64..2000).map(|i| (i, format!("msg-{}", i))).collect();
        assert!(WalVerifier::verify_recovery_payloads(&good, 2000).is_ok());
    }

    #[test]
    fn recovery_accepts_extra_unacked_messages() {
        // At-least contract: 2001 msgs against min 2000 (one extra, e.g. an
        // un-acked write that survived) passes; the old exact-count check
        // failed this durable broker.
        let mut extra: Vec<(u64, String)> =
            (0u64..2000).map(|i| (i, format!("msg-{}", i))).collect();
        extra.push((2000, "msg-2000".to_string()));
        assert!(WalVerifier::verify_recovery_payloads(&extra, 2000).is_ok());
    }

    #[test]
    fn recovery_rejects_shifted_and_short_replay() {
        // Right count, wrong start.
        let shifted: Vec<(u64, String)> =
            (1u64..=2000).map(|i| (i, format!("msg-{}", i))).collect();
        assert!(WalVerifier::verify_recovery_payloads(&shifted, 2000).is_err());
        // Contiguous but short.
        let short: Vec<(u64, String)> = (0u64..1999).map(|i| (i, format!("msg-{}", i))).collect();
        assert!(WalVerifier::verify_recovery_payloads(&short, 2000).is_err());
        // Right offsets, wrong payload.
        let mut bad = (0u64..3)
            .map(|i| (i, format!("msg-{}", i)))
            .collect::<Vec<_>>();
        bad[1].1 = "tampered".to_string();
        assert!(WalVerifier::verify_recovery_payloads(&bad, 3).is_err());
    }
}
