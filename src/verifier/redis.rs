use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

use super::StageResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct RedisVerifier {
    pub target_port: u16,
    pub run_seed: Option<String>,
}

impl RedisVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        Self {
            target_port,
            run_seed: seed.map(|s| s.to_string()),
        }
    }

    pub fn get_seed_prefix<'a>(&'a self, default_val: &'a str) -> &'a str {
        super::get_seed_prefix(self.run_seed.as_deref(), default_val)
    }

    pub fn format_resp_cmd(args: &[&str]) -> String {
        let mut payload = format!("*{}\r\n", args.len());
        for arg in args {
            payload.push_str(&format!("${}\r\n{}\r\n", arg.len(), arg));
        }
        payload
    }

    pub async fn run_all(&self) -> RedisTestSummary {
        let mut stages = Vec::new();

        stages.push(self.test_stage1_handshake().await);
        stages.push(self.test_stage2_key_value().await);
        stages.push(self.test_stage3_expiration().await);
        stages.push(self.test_stage4_counters().await);
        stages.push(self.test_stage5_pubsub().await);

        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;

        RedisTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    async fn connect(&self) -> Result<TcpStream> {
        let addr = format!("127.0.0.1:{}", self.target_port);
        tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("Connection timed out to {}", addr))?
            .map_err(|e| anyhow!("Failed to connect to {}: {}", addr, e))
    }

    /// Parses a complete RESP frame from the beginning of `buf`, returning the total number
    /// of consumed bytes if complete, or `None` if the frame is partial/incomplete.
    pub fn parse_resp_frame(buf: &[u8]) -> Option<usize> {
        if buf.is_empty() {
            return None;
        }
        match buf[0] {
            b'+' | b'-' | b':' => {
                let pos = buf.windows(2).position(|w| w == b"\r\n")?;
                Some(pos + 2)
            }
            b'$' => {
                let pos = buf.windows(2).position(|w| w == b"\r\n")?;
                let len_str = std::str::from_utf8(&buf[1..pos]).ok()?.trim();
                if len_str == "-1" {
                    return Some(pos + 2);
                }
                let len = len_str.parse::<usize>().ok()?;
                let total = pos + 2 + len + 2;
                if buf.len() >= total {
                    Some(total)
                } else {
                    None
                }
            }
            b'*' => {
                let pos = buf.windows(2).position(|w| w == b"\r\n")?;
                let count_str = std::str::from_utf8(&buf[1..pos]).ok()?.trim();
                if count_str == "-1" || count_str == "0" {
                    return Some(pos + 2);
                }
                let count = count_str.parse::<usize>().ok()?;
                let mut offset = pos + 2;
                for _ in 0..count {
                    if offset >= buf.len() {
                        return None;
                    }
                    let consumed = Self::parse_resp_frame(&buf[offset..])?;
                    offset += consumed;
                }
                Some(offset)
            }
            _ => {
                let pos = buf.windows(2).position(|w| w == b"\r\n")?;
                Some(pos + 2)
            }
        }
    }

    pub fn is_complete_resp_frame(buf: &[u8]) -> bool {
        Self::parse_resp_frame(buf).is_some()
    }

    async fn send_resp_cmd(stream: &mut TcpStream, args: &[&str]) -> Result<String> {
        let payload = Self::format_resp_cmd(args);

        stream.write_all(payload.as_bytes()).await?;
        stream.flush().await?;

        Self::read_resp_reply(stream).await
    }

    /// Read one complete RESP frame (used for server-pushed messages too).
    async fn read_resp_reply(stream: &mut TcpStream) -> Result<String> {
        let mut buf = Vec::new();
        let mut temp = [0u8; 1024];
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(3);

        while !Self::is_complete_resp_frame(&buf) {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                if !buf.is_empty() {
                    return Err(anyhow!(
                        "Read timed out with partial incomplete RESP frame: {:?}",
                        String::from_utf8_lossy(&buf)
                    ));
                }
                return Err(anyhow!("Read timed out"));
            }
            let remaining = timeout - elapsed;

            let n = tokio::time::timeout(remaining, stream.read(&mut temp))
                .await
                .map_err(|_| anyhow!("Read timed out"))??;

            if n == 0 {
                if buf.is_empty() {
                    return Err(anyhow!("Server closed connection prematurely"));
                }
                return Err(anyhow!(
                    "Server closed connection with incomplete RESP frame: {:?}",
                    String::from_utf8_lossy(&buf)
                ));
            }

            buf.extend_from_slice(&temp[..n]);
        }

        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    /// Advisory robustness probes (NOT scored): malformed-input survival
    /// and slow-sender isolation. A server may close the offending
    /// connection — that is acceptable behavior. What passes is *service*
    /// survival: a fresh connection must serve PING afterwards, and a
    /// half-open sender must not stall other clients. Never called from
    /// `run_all`; the pipeline logs these separately under [ROBUST].
    pub async fn run_robustness(&self) -> Vec<StageResult> {
        vec![
            self.robust_malformed().await,
            self.robust_slow_sender().await,
        ]
    }

    async fn robust_malformed(&self) -> StageResult {
        let name = "Robust 1: malformed input survival (advisory)".to_string();
        let fail = |e: String| StageResult {
            stage: 101,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        // Binary garbage + a truncated array header on one connection.
        let mut dirty = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("connect for garbage probe: {e}")),
        };
        use tokio::io::AsyncWriteExt;
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            dirty.write_all(b"\xff\xfe\x00bad\r\n*3\r\n$3\r\nGE"),
        )
        .await;
        // Service must survive on a FRESH connection (same-socket survival
        // is not required — closing garbage senders is legitimate).
        drop(dirty);
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("reconnect after garbage: {e}")),
        };
        match tokio::time::timeout(
            Duration::from_secs(5),
            Self::send_resp_cmd(&mut stream, &["PING"]),
        )
        .await
        {
            Ok(Ok(r)) if r.trim() == "+PONG" => StageResult {
                stage: 101,
                name,
                passed: true,
                error: None,
            },
            Ok(Ok(r)) => fail(format!("PING after garbage got {r:?}, want +PONG")),
            Ok(Err(e)) => fail(format!("PING after garbage failed: {e}")),
            Err(_) => fail("PING after garbage timed out (server wedged?)".to_string()),
        }
    }

    async fn robust_slow_sender(&self) -> StageResult {
        let name = "Robust 2: slow sender + half-open isolation (advisory)".to_string();
        let fail = |e: String| StageResult {
            stage: 102,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        use tokio::io::AsyncWriteExt;
        // Half-open sender: partial frame, long pause, then completion.
        let mut slow = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("connect slow sender: {e}")),
        };
        if tokio::time::timeout(Duration::from_secs(3), slow.write_all(b"*2\r\n$3\r\nGE"))
            .await
            .is_err()
        {
            return fail("partial write failed".to_string());
        }
        // While the sender is half-open, an independent client must work.
        let mut other = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("parallel connect during half-open: {e}")),
        };
        match tokio::time::timeout(
            Duration::from_secs(5),
            Self::send_resp_cmd(&mut other, &["PING"]),
        )
        .await
        {
            Ok(Ok(r)) if r.trim() == "+PONG" => {}
            Ok(Ok(r)) => return fail(format!("parallel PING got {r:?} during half-open")),
            Ok(Err(e)) => return fail(format!("parallel PING failed during half-open: {e}")),
            Err(_) => {
                return fail(
                    "parallel PING timed out during half-open (single-threaded server?)"
                        .to_string(),
                )
            }
        }
        // Complete the slow frame; expect a well-formed bulk reply.
        if tokio::time::timeout(
            Duration::from_secs(3),
            slow.write_all(b"T\r\n$3\r\nfoo\r\n"),
        )
        .await
        .is_err()
        {
            return fail("completion write failed".to_string());
        }
        match tokio::time::timeout(Duration::from_secs(5), Self::read_resp_reply(&mut slow)).await {
            Ok(Ok(r)) if r.contains("foo") => StageResult {
                stage: 102,
                name,
                passed: true,
                error: None,
            },
            Ok(Ok(r)) => fail(format!("slow-frame reply {r:?}, want bulk foo")),
            Ok(Err(e)) => fail(format!("slow-frame read failed: {e}")),
            Err(_) => fail("slow-frame reply timed out".to_string()),
        }
    }
    /// Advisory differential oracle (NOT scored): replays a seeded random
    /// op stream against the candidate AND the official reference and
    /// requires equivalent replies. A hardcoded routing table cannot
    /// answer random keys; a protocol-faithful server can. Error replies
    /// compare by category (`-ERR`) — message text legitimately differs
    /// across implementations — while value-carrying replies must match
    /// byte-for-byte. Never called from `run_all`; the pipeline logs the
    /// single verdict stage separately under [ROBUST].
    pub async fn run_differential(&self, ref_port: u16) -> Vec<StageResult> {
        let name = "Differential vs ref-redis on seeded traffic (advisory)".to_string();
        let fail = |e: String| StageResult {
            stage: 103,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        let addr = format!("127.0.0.1:{}", ref_port);
        let mut reference =
            match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await {
                Ok(Ok(s)) => s,
                _ => {
                    return vec![fail(format!(
                        "reference unreachable at {addr} (harness, not candidate)"
                    ))]
                }
            };
        let mut candidate = match self.connect().await {
            Ok(s) => s,
            Err(e) => return vec![fail(format!("candidate connect: {e}"))],
        };
        // Seeded xorshift: identical stream every run of this seed, no rng crate needed.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        self.get_seed_prefix("test").hash(&mut h);
        "differential-v1".hash(&mut h);
        let mut rng = h.finish() | 1;
        fn xnext(rng: &mut u64) -> u64 {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            *rng
        }
        const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_- ";
        let tag: String = (0..6)
            .map(|_| ALPHA[(xnext(&mut rng) % ALPHA.len() as u64) as usize] as char)
            .collect();
        let keys: Vec<String> = (0..8).map(|i| format!("diff_{tag}_{i}")).collect();
        let ctr = format!("diff_{tag}_ctr");
        // (opname, args) with owned strings: the stream is fixed per seed.
        let mut ops: Vec<(String, Vec<String>)> = Vec::new();
        for _ in 0..24 {
            let k = keys[(xnext(&mut rng) % 8) as usize].clone();
            match xnext(&mut rng) % 6 {
                0 => {
                    let vlen = (xnext(&mut rng) % 17) as usize;
                    let v: String = (0..vlen)
                        .map(|_| ALPHA[(xnext(&mut rng) % ALPHA.len() as u64) as usize] as char)
                        .collect();
                    ops.push(("SET".into(), vec![k, v]))
                }
                1 => ops.push(("GET".into(), vec![k])),
                2 => ops.push(("DEL".into(), vec![k])),
                3 => ops.push(("EXISTS".into(), vec![k])),
                4 => ops.push(("INCR".into(), vec![ctr.clone()])),
                _ => ops.push(("DECR".into(), vec![ctr.clone()])),
            }
        }
        let body = async {
            for (i, (op, args)) in ops.iter().enumerate() {
                let arg_refs: Vec<&str> = std::iter::once(op.as_str())
                    .chain(args.iter().map(|s| s.as_str()))
                    .collect();
                let want = Self::send_resp_cmd(&mut reference, &arg_refs)
                    .await
                    .map_err(|e| format!("reference op #{i} {op} failed: {e}"))?;
                let got = Self::send_resp_cmd(&mut candidate, &arg_refs)
                    .await
                    .map_err(|e| format!("candidate op #{i} {op} failed: {e}"))?;
                let (nw, ng) = (Self::normalize_reply(&want), Self::normalize_reply(&got));
                if nw != ng {
                    return Err(format!(
                        "divergence at op #{i} {op} {args:?}: ref={want:?} candidate={got:?}"
                    ));
                }
            }
            // Best-effort cleanup of our keys on both sides; failures ignored.
            for k in keys.iter().chain(std::iter::once(&ctr)) {
                let _ = Self::send_resp_cmd(&mut reference, &["DEL", k]).await;
                let _ = Self::send_resp_cmd(&mut candidate, &["DEL", k]).await;
            }
            Ok::<(), String>(())
        };
        match tokio::time::timeout(Duration::from_secs(90), body).await {
            Ok(Ok(())) => vec![StageResult {
                stage: 103,
                name,
                passed: true,
                error: None,
            }],
            Ok(Err(e)) => vec![fail(e)],
            Err(_) => vec![fail("differential stream timed out after 90s".to_string())],
        }
    }

    /// Reply equivalence classes: identical frames match; error payloads
    /// reduce to their `-ERR` category since message text legitimately
    /// differs between implementations.
    fn normalize_reply(reply: &str) -> String {
        let t = reply.trim_end_matches(['\r', '\n']);
        if t.starts_with('-') {
            return "-ERR".to_string();
        }
        t.to_string()
    }
    pub async fn test_stage1_handshake(&self) -> StageResult {
        let name = "Stage 1: PING & ECHO Handshake".to_string();
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => {
                return StageResult {
                    stage: 1,
                    name,
                    passed: false,
                    error: Some(e.to_string()),
                }
            }
        };

        let res = match Self::send_resp_cmd(&mut stream, &["PING"]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 1,
                    name,
                    passed: false,
                    error: Some(format!("PING failed: {}", e)),
                }
            }
        };

        if res.trim() != "+PONG" {
            return StageResult {
                stage: 1,
                name,
                passed: false,
                error: Some(format!("Expected +PONG\\r\\n, got: {:?}", res)),
            };
        }

        let prefix = self.get_seed_prefix("test");
        let echo_str = if prefix == "test" {
            "subdollar_test".to_string()
        } else {
            format!("echo_{}", prefix)
        };

        let res = match Self::send_resp_cmd(&mut stream, &["ECHO", &echo_str]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 1,
                    name,
                    passed: false,
                    error: Some(format!("ECHO failed: {}", e)),
                }
            }
        };

        let expected = format!("${}\r\n{}", echo_str.len(), echo_str);
        if res.trim() != expected && res != format!("{}\r\n", expected) {
            return StageResult {
                stage: 1,
                name,
                passed: false,
                error: Some(format!(
                    "Expected ECHO with '{}\\r\\n', got: {:?}",
                    expected, res
                )),
            };
        }

        StageResult {
            stage: 1,
            name,
            passed: true,
            error: None,
        }
    }

    pub async fn test_stage2_key_value(&self) -> StageResult {
        let name = "Stage 2: Basic Key-Value (SET/GET/DEL/EXISTS)".to_string();
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(e.to_string()),
                }
            }
        };

        let seed_prefix = self.get_seed_prefix("bench");
        let key = format!("k_{}", seed_prefix);
        let val = format!("v_{}", seed_prefix);

        let set_res = match Self::send_resp_cmd(&mut stream, &["SET", &key, &val]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(format!("SET failed: {}", e)),
                }
            }
        };
        if set_res.trim() != "+OK" {
            return StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(format!("Expected +OK, got {:?}", set_res)),
            };
        }

        let get_res = match Self::send_resp_cmd(&mut stream, &["GET", &key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(format!("GET failed: {}", e)),
                }
            }
        };
        let expected_val = format!("${}\r\n{}", val.len(), val);
        if get_res.trim() != expected_val && get_res != format!("{}\r\n", expected_val) {
            return StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(format!("Expected '{}', got {:?}", expected_val, get_res)),
            };
        }

        let exists_res = match Self::send_resp_cmd(&mut stream, &["EXISTS", &key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(format!("EXISTS failed: {}", e)),
                }
            }
        };
        if exists_res.trim() != ":1" {
            return StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(format!("Expected :1, got {:?}", exists_res)),
            };
        }

        let del_res = match Self::send_resp_cmd(&mut stream, &["DEL", &key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(format!("DEL failed: {}", e)),
                }
            }
        };
        if del_res.trim() != ":1" {
            return StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(format!("Expected :1 from DEL, got {:?}", del_res)),
            };
        }

        let get_nil = match Self::send_resp_cmd(&mut stream, &["GET", &key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(format!("GET after DEL failed: {}", e)),
                }
            }
        };
        if get_nil.trim() != "$-1" {
            return StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(format!("Expected nil ($-1\\r\\n), got {:?}", get_nil)),
            };
        }

        let exists_zero = match Self::send_resp_cmd(&mut stream, &["EXISTS", &key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(format!("EXISTS after DEL failed: {}", e)),
                }
            }
        };
        if exists_zero.trim() != ":0" {
            return StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(format!("Expected :0, got {:?}", exists_zero)),
            };
        }

        // TCP Pipelining Check: write multiple commands in a single write buffer
        let pipe_k1 = format!("{}_p1", key);
        let pipe_k2 = format!("{}_p2", key);
        let pipe_payload = format!(
            "{}{}{}{}",
            Self::format_resp_cmd(&["SET", &pipe_k1, "v1"]),
            Self::format_resp_cmd(&["SET", &pipe_k2, "v2"]),
            Self::format_resp_cmd(&["GET", &pipe_k1]),
            Self::format_resp_cmd(&["GET", &pipe_k2])
        );
        if stream.write_all(pipe_payload.as_bytes()).await.is_ok() {
            let _ = stream.flush().await;
            let mut read_buf = Vec::new();
            let mut temp = [0u8; 1024];
            let start = std::time::Instant::now();
            while read_buf.len() < 24 && start.elapsed() < Duration::from_millis(600) {
                if let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_millis(200), stream.read(&mut temp)).await
                {
                    if n == 0 {
                        break;
                    }
                    read_buf.extend_from_slice(&temp[..n]);
                } else {
                    break;
                }
            }
            let pipe_resp = String::from_utf8_lossy(&read_buf);
            if !pipe_resp.contains("+OK") || !pipe_resp.contains("v1") {
                return StageResult {
                    stage: 2,
                    name,
                    passed: false,
                    error: Some(format!(
                        "TCP pipelining failed, got response: {:?}",
                        pipe_resp
                    )),
                };
            }
        }

        StageResult {
            stage: 2,
            name,
            passed: true,
            error: None,
        }
    }

    pub async fn test_stage3_expiration(&self) -> StageResult {
        let name = "Stage 3: Expiration (SET ... PX <ms>)".to_string();
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => {
                return StageResult {
                    stage: 3,
                    name,
                    passed: false,
                    error: Some(e.to_string()),
                }
            }
        };

        let seed_prefix = self.get_seed_prefix("ttl");
        let ttl_key = format!("ttl_{}", seed_prefix);
        let ttl_val = format!("val_{}", seed_prefix);

        let set_res =
            match Self::send_resp_cmd(&mut stream, &["SET", &ttl_key, &ttl_val, "PX", "500"]).await
            {
                Ok(r) => r,
                Err(e) => {
                    return StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!("SET PX failed: {}", e)),
                    }
                }
            };
        if set_res.trim() != "+OK" {
            return StageResult {
                stage: 3,
                name,
                passed: false,
                error: Some(format!("Expected +OK for SET PX, got {:?}", set_res)),
            };
        }

        let get_fast = match Self::send_resp_cmd(&mut stream, &["GET", &ttl_key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 3,
                    name,
                    passed: false,
                    error: Some(format!("Immediate GET failed: {}", e)),
                }
            }
        };
        let expected_fast = format!("${}\r\n{}", ttl_val.len(), ttl_val);
        if get_fast.trim() != expected_fast && get_fast != format!("{}\r\n", expected_fast) {
            return StageResult {
                stage: 3,
                name,
                passed: false,
                error: Some(format!(
                    "Expected immediate GET to return '{}', got {:?}",
                    expected_fast, get_fast
                )),
            };
        }

        sleep(Duration::from_millis(1000)).await;

        let get_expired = match Self::send_resp_cmd(&mut stream, &["GET", &ttl_key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 3,
                    name,
                    passed: false,
                    error: Some(format!("Post-TTL GET failed: {}", e)),
                }
            }
        };
        if get_expired.trim() != "$-1" {
            return StageResult {
                stage: 3,
                name,
                passed: false,
                error: Some(format!(
                    "Expected expired key to return nil ($-1\\r\\n), got {:?}",
                    get_expired
                )),
            };
        }

        StageResult {
            stage: 3,
            name,
            passed: true,
            error: None,
        }
    }

    pub async fn test_stage4_counters(&self) -> StageResult {
        let name = "Stage 4: INCR & DECR Arithmetic".to_string();
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => {
                return StageResult {
                    stage: 4,
                    name,
                    passed: false,
                    error: Some(e.to_string()),
                }
            }
        };

        let seed_prefix = self.get_seed_prefix("num");
        let counter_key = format!("cnt_{}", seed_prefix);
        let non_num_key = format!("str_{}", seed_prefix);

        let incr1 = match Self::send_resp_cmd(&mut stream, &["INCR", &counter_key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 4,
                    name,
                    passed: false,
                    error: Some(format!("INCR 1 failed: {}", e)),
                }
            }
        };
        if incr1.trim() != ":1" {
            return StageResult {
                stage: 4,
                name,
                passed: false,
                error: Some(format!("Expected :1, got {:?}", incr1)),
            };
        }

        let incr2 = match Self::send_resp_cmd(&mut stream, &["INCR", &counter_key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 4,
                    name,
                    passed: false,
                    error: Some(format!("INCR 2 failed: {}", e)),
                }
            }
        };
        if incr2.trim() != ":2" {
            return StageResult {
                stage: 4,
                name,
                passed: false,
                error: Some(format!("Expected :2, got {:?}", incr2)),
            };
        }

        let decr1 = match Self::send_resp_cmd(&mut stream, &["DECR", &counter_key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 4,
                    name,
                    passed: false,
                    error: Some(format!("DECR failed: {}", e)),
                }
            }
        };
        if decr1.trim() != ":1" {
            return StageResult {
                stage: 4,
                name,
                passed: false,
                error: Some(format!("Expected :1, got {:?}", decr1)),
            };
        }

        // Negative test: non-numeric increment error handling
        let _ = Self::send_resp_cmd(&mut stream, &["SET", &non_num_key, "invalid_number"]).await;
        let err_res = match Self::send_resp_cmd(&mut stream, &["INCR", &non_num_key]).await {
            Ok(r) => r,
            Err(e) => {
                return StageResult {
                    stage: 4,
                    name,
                    passed: false,
                    error: Some(format!("Negative test failed: {}", e)),
                }
            }
        };
        if !err_res.starts_with('-') {
            return StageResult {
                stage: 4,
                name,
                passed: false,
                error: Some(format!("Expected RESP error (-ERR ...), got {:?}", err_res)),
            };
        }

        StageResult {
            stage: 4,
            name,
            passed: true,
            error: None,
        }
    }

    pub async fn test_stage5_pubsub(&self) -> StageResult {
        let name = "Stage 5: PUB/SUB fan-out".to_string();
        let fail = |e: String| StageResult {
            stage: 5,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        let chan = format!("chan_{}", self.get_seed_prefix("fan"));
        let mut sub = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("subscriber connect: {}", e)),
        };
        let conf = match Self::send_resp_cmd(&mut sub, &["SUBSCRIBE", &chan]).await {
            Ok(r) => r,
            Err(e) => return fail(format!("SUBSCRIBE: {}", e)),
        };
        let conf_low = conf.to_lowercase();
        if !(conf_low.contains("subscribe") && conf.contains(&chan)) {
            return fail(format!("bad subscribe-confirm: {:?}", conf));
        }
        let mut pb = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("publisher connect: {}", e)),
        };
        let n = match Self::send_resp_cmd(&mut pb, &["PUBLISH", &chan, "hello-fanout"]).await {
            Ok(r) => r,
            Err(e) => return fail(format!("PUBLISH: {}", e)),
        };
        if n.trim() != ":1" {
            return fail(format!("PUBLISH receivers: expected :1, got {:?}", n));
        }
        let pushed = match Self::read_resp_reply(&mut sub).await {
            Ok(r) => r,
            Err(e) => return fail(format!("broadcast read: {}", e)),
        };
        if !(pushed.contains("message")
            && pushed.contains(&chan)
            && pushed.contains("hello-fanout"))
        {
            return fail(format!("bad broadcast frame: {:?}", pushed));
        }
        StageResult {
            stage: 5,
            name,
            passed: true,
            error: None,
        }
    }

    /// AOF probe keypair (seeded so parallel runs never collide).
    pub fn probe_pair(&self) -> (String, String) {
        let seed = self.get_seed_prefix("aof").to_string();
        (
            format!("persist_probe_{}", seed),
            format!("smoked-{}", seed),
        )
    }

    pub async fn write_probe_key(&self) -> Result<()> {
        let (k, v) = self.probe_pair();
        let mut stream = self.connect().await?;
        let r = Self::send_resp_cmd(&mut stream, &["SET", &k, &v]).await?;
        if r.trim() != "+OK" {
            return Err(anyhow!("SET probe: {:?}", r));
        }
        Ok(())
    }

    /// Stage 6: run AFTER the harness SIGKILL+restarts the candidate.
    pub async fn check_persistence(&self) -> StageResult {
        let name = "Stage 6: AOF crash recovery".to_string();
        let fail = |e: String| StageResult {
            stage: 6,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        let (k, v) = self.probe_pair();
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("reconnect: {}", e)),
        };
        let got = match Self::send_resp_cmd(&mut stream, &["GET", &k]).await {
            Ok(r) => r,
            Err(e) => return fail(format!("GET probe: {}", e)),
        };
        if !got.contains(&v) {
            return fail(format!("probe key lost or wrong: {:?}", got));
        }
        StageResult {
            stage: 6,
            name,
            passed: true,
            error: None,
        }
    }

    /// Parse an INFO bulk reply into a field map. Accepts the raw RESP frame
    /// (`$N\r\n...`) or a bare field block; keys are lowercased; section
    /// headers (`# ...`) and blank lines are ignored.
    pub fn parse_info(reply: &str) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let mut body = reply.trim_start();
        if let Some(rest) = body.strip_prefix('$') {
            if let Some(nl) = rest.find("\r\n") {
                body = &rest[nl + 2..];
            }
        }
        for line in body.split(['\n', '\r']) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                if !k.trim().is_empty() && !v.trim().is_empty() {
                    out.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
        }
        out
    }

    async fn fetch_info(
        stream: &mut TcpStream,
    ) -> Result<std::collections::HashMap<String, String>, String> {
        match Self::send_resp_cmd(stream, &["INFO"]).await {
            Ok(r) => {
                if r.starts_with('-') {
                    return Err(format!("INFO error reply: {}", r.trim()));
                }
                Ok(Self::parse_info(&r))
            }
            Err(e) => Err(format!("INFO failed: {e}")),
        }
    }

    /// Stage 7 (campaign observability ticket; explicit acceptance only):
    /// INFO must expose cumulative counters whose DELTAS track randomized
    /// scripted traffic. Static or hardcoded values fail the delta checks,
    /// and the counts differ every run, so a routing table cannot pass.
    pub async fn test_stage_info(&self) -> StageResult {
        let name = "Stage 7: INFO metrics & counters".to_string();
        let fail = |e: String| StageResult {
            stage: 7,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        const REQUIRED: &[&str] = &[
            "total_connections_received",
            "total_commands_processed",
            "keyspace_hits",
            "keyspace_misses",
            "expired_keys",
            "rejected_connections",
            "connected_clients",
            "uptime_in_seconds",
        ];
        let num = |m: &std::collections::HashMap<String, String>, f: &str| {
            m.get(f).and_then(|v| v.trim().parse::<i64>().ok())
        };

        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("connect: {e}")),
        };
        let info0 = match Self::fetch_info(&mut stream).await {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        for f in REQUIRED {
            if !info0.contains_key(*f) {
                return fail(format!(
                    "INFO is missing required field '{f}' ({} fields returned)",
                    info0.len()
                ));
            }
            if num(&info0, f).is_none() {
                return fail(format!("INFO field '{f}' is not an integer"));
            }
        }

        // Randomized traffic counts per run (anti-hardcode).
        let mut h = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        self.get_seed_prefix("info").hash(&mut h);
        "info-stage-v1".hash(&mut h);
        let mut rng = h.finish() | 1;
        fn xnext(r: &mut u64) -> u64 {
            *r ^= *r << 13;
            *r ^= *r >> 7;
            *r ^= *r << 17;
            *r
        }
        let hits = 5 + (xnext(&mut rng) % 16) as usize;
        let misses = 5 + (xnext(&mut rng) % 16) as usize;
        let tag = format!("{:08x}", xnext(&mut rng));

        let mut expect_commands: i64 = 0;
        for i in 0..hits {
            let k = format!("info_{tag}_{i}");
            let v = format!("v{i}");
            match Self::send_resp_cmd(&mut stream, &["SET", &k, &v]).await {
                Ok(r) if r.trim() == "+OK" => {}
                Ok(r) => return fail(format!("SET {k}: {r:?}")),
                Err(e) => return fail(format!("SET {k} failed: {e}")),
            }
            expect_commands += 1;
            match Self::send_resp_cmd(&mut stream, &["GET", &k]).await {
                Ok(r) if r.contains(&v) => {}
                Ok(r) => return fail(format!("GET {k}: {r:?}")),
                Err(e) => return fail(format!("GET {k} failed: {e}")),
            }
            expect_commands += 1;
        }
        for i in 0..misses {
            let k = format!("info_miss_{tag}_{i}");
            match Self::send_resp_cmd(&mut stream, &["GET", &k]).await {
                Ok(r) if r.starts_with("$-1") => {}
                Ok(r) => return fail(format!("GET missing {k}: {r:?}")),
                Err(e) => return fail(format!("GET missing {k} failed: {e}")),
            }
            expect_commands += 1;
        }
        // One expiring key: touching it after the deadline must count it.
        let ek = format!("info_exp_{tag}");
        match Self::send_resp_cmd(&mut stream, &["SET", &ek, "1", "PX", "150"]).await {
            Ok(r) if r.trim() == "+OK" => {}
            Ok(r) => return fail(format!("SET PX: {r:?}")),
            Err(e) => return fail(format!("SET PX failed: {e}")),
        }
        expect_commands += 1;
        sleep(Duration::from_millis(450)).await;
        match Self::send_resp_cmd(&mut stream, &["GET", &ek]).await {
            Ok(r) if r.starts_with("$-1") => {}
            Ok(r) => return fail(format!("expired key still present: {r:?}")),
            Err(e) => return fail(format!("GET expired failed: {e}")),
        }
        expect_commands += 1;

        let info1 = match Self::fetch_info(&mut stream).await {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        let delta = |f: &str| -> Option<i64> { Some(num(&info1, f)? - num(&info0, f)?) };
        for f in REQUIRED {
            match delta(f) {
                Some(v) if v >= 0 => {}
                Some(v) => return fail(format!("counter '{f}' went backwards ({v})")),
                None => return fail(format!("counter '{f}' missing or unparsable after traffic")),
            }
        }
        let hits_d = delta("keyspace_hits").unwrap_or(0);
        let miss_d = delta("keyspace_misses").unwrap_or(0);
        let cmds_d = delta("total_commands_processed").unwrap_or(0);
        let conns_d = delta("total_connections_received").unwrap_or(0);
        let exp_d = delta("expired_keys").unwrap_or(0);
        if hits_d < hits as i64 {
            return fail(format!(
                "keyspace_hits delta {hits_d} < executed hits {hits}"
            ));
        }
        if miss_d < misses as i64 {
            return fail(format!(
                "keyspace_misses delta {miss_d} < executed misses {misses}"
            ));
        }
        if cmds_d < expect_commands {
            return fail(format!(
                "total_commands_processed delta {cmds_d} < executed {expect_commands}"
            ));
        }
        if conns_d < 1 {
            return fail(format!(
                "total_connections_received did not grow (delta {conns_d})"
            ));
        }
        if exp_d < 1 {
            return fail(format!(
                "expired_keys did not grow after a key expired (delta {exp_d})"
            ));
        }
        if num(&info1, "connected_clients").unwrap_or(0) < 1 {
            return fail("connected_clients < 1 while a client is connected".to_string());
        }
        StageResult {
            stage: 7,
            name,
            passed: true,
            error: None,
        }
    }

    /// Stage 8 (campaign ENOSPC incident): the pipeline runs the candidate
    /// with `SDB_DATA_DIR` on a small tmpfs. Protocol writes fill the volume
    /// until persistence fails; then:
    ///   - previously acked data still reads back and PING stays alive
    ///     (no crash, no wedge),
    ///   - a write that cannot be persisted returns an error and is NOT
    ///     applied (GET of the failed key is nil) — never a silent ack,
    ///   - further writes answer without hanging (error preferred; success
    ///     is tolerated only if the server reclaimed space).
    ///
    /// Recovery-after-free and replay-after-incident are out of scope (they
    /// need to free/reach the container's tmpfs); see TODO_CAMPAIGNS C2.
    pub async fn test_stage_enospc(&self) -> StageResult {
        let name = "Stage 8: ENOSPC incident (log volume full)".to_string();
        let fail = |e: String| StageResult {
            stage: 8,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };

        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("connect: {e}")),
        };
        match Self::send_resp_cmd(&mut stream, &["PING"]).await {
            Ok(r) if r.trim() == "+PONG" => {}
            Ok(r) => return fail(format!("PING before fill: {r:?}")),
            Err(e) => return fail(format!("PING before fill failed: {e}")),
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        self.get_seed_prefix("enospc").hash(&mut h);
        "enospc-stage-v1".hash(&mut h);
        let tag = format!("{:08x}", (h.finish() ^ (h.finish() >> 32)) as u32);
        let seed_k = format!("enospc_seed_{tag}");
        let seed_v = format!("seed-{tag}");
        match Self::send_resp_cmd(&mut stream, &["SET", &seed_k, &seed_v]).await {
            Ok(r) if r.trim() == "+OK" => {}
            Ok(r) => return fail(format!("seed SET: {r:?}")),
            Err(e) => return fail(format!("seed SET failed: {e}")),
        }

        // Fill with 240KB values: an 8MB volume covers ~30 successful
        // writes; the 80-attempt bound (≈19MB of payload) guarantees
        // termination even if the log is not on the small volume.
        let value = "x".repeat(240 * 1024);
        let mut failed_key: Option<usize> = None;
        for i in 0..80usize {
            let k = format!("enospc_fill_{tag}_{i}");
            match Self::send_resp_cmd(&mut stream, &["SET", &k, &value]).await {
                Ok(r) if r.trim() == "+OK" => continue,
                Ok(r) if r.starts_with('-') => {
                    failed_key = Some(i);
                    break;
                }
                Ok(r) => return fail(format!("fill SET {i} unexpected reply {r:?}")),
                Err(e) => {
                    return fail(format!(
                        "fill SET {i} connection failed (server died or wedged): {e}"
                    ))
                }
            }
        }
        let Some(fk) = failed_key else {
            return fail(
                "writes never failed after ~19MB: the log volume never filled (is the log on SDB_DATA_DIR?)"
                    .to_string(),
            );
        };

        // Previously acked data still reads back.
        match Self::send_resp_cmd(&mut stream, &["GET", &seed_k]).await {
            Ok(r) if r.contains(&seed_v) => {}
            Ok(r) => return fail(format!("seed read with full volume: {r:?}")),
            Err(e) => return fail(format!("seed read failed with full volume: {e}")),
        }
        // Alive.
        match Self::send_resp_cmd(&mut stream, &["PING"]).await {
            Ok(r) if r.trim() == "+PONG" => {}
            Ok(r) => return fail(format!("PING with full volume: {r:?}")),
            Err(e) => return fail(format!("PING failed with full volume (server died?): {e}")),
        }
        // Never apply unpersisted state.
        let fk_name = format!("enospc_fill_{tag}_{fk}");
        match Self::send_resp_cmd(&mut stream, &["GET", &fk_name]).await {
            Ok(r) if r.starts_with("$-1") => {}
            Ok(r) => return fail(format!("unpersisted write became visible: {r:?}")),
            Err(e) => return fail(format!("read of failed key failed: {e}")),
        }
        // Further writes must answer (hang/crash is the failure).
        match Self::send_resp_cmd(
            &mut stream,
            &["SET", &format!("enospc_after_{tag}"), &value],
        )
        .await
        {
            Ok(_) => {}
            Err(e) => return fail(format!("write after failure did not answer: {e}")),
        }
        StageResult {
            stage: 8,
            name,
            passed: true,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn test_resp_formatting() {
        assert_eq!(
            RedisVerifier::format_resp_cmd(&["PING"]),
            "*1\r\n$4\r\nPING\r\n"
        );
        assert_eq!(
            RedisVerifier::format_resp_cmd(&["ECHO", "hello"]),
            "*2\r\n$4\r\nECHO\r\n$5\r\nhello\r\n"
        );
        assert_eq!(
            RedisVerifier::format_resp_cmd(&["SET", "k", "v", "PX", "100"]),
            "*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n$2\r\nPX\r\n$3\r\n100\r\n"
        );
    }

    #[test]
    fn test_is_complete_resp_frame() {
        assert!(RedisVerifier::is_complete_resp_frame(b"+PONG\r\n"));
        assert!(RedisVerifier::is_complete_resp_frame(b":1\r\n"));
        assert!(RedisVerifier::is_complete_resp_frame(b":0\r\n"));
        assert!(RedisVerifier::is_complete_resp_frame(
            b"-ERR unknown command\r\n"
        ));
        assert!(RedisVerifier::is_complete_resp_frame(b"$-1\r\n"));
        assert!(!RedisVerifier::is_complete_resp_frame(b"+PONG"));
        assert!(!RedisVerifier::is_complete_resp_frame(
            b"$14\r\nsubdollar_test"
        ));
        assert!(RedisVerifier::is_complete_resp_frame(
            b"$14\r\nsubdollar_test\r\n"
        ));
        assert!(!RedisVerifier::is_complete_resp_frame(b""));

        // Array frames
        assert!(RedisVerifier::is_complete_resp_frame(b"*0\r\n"));
        assert!(RedisVerifier::is_complete_resp_frame(b"*-1\r\n"));
        assert!(RedisVerifier::is_complete_resp_frame(
            b"*2\r\n$4\r\nECHO\r\n$5\r\nhello\r\n"
        ));
        assert!(!RedisVerifier::is_complete_resp_frame(
            b"*2\r\n$4\r\nECHO\r\n"
        )); // only 1 of 2 elements
        assert!(!RedisVerifier::is_complete_resp_frame(
            b"*2\r\n$4\r\nECHO\r\n$5\r\nhel"
        )); // incomplete second element
        assert!(RedisVerifier::is_complete_resp_frame(
            b"*1\r\n*1\r\n:42\r\n"
        )); // nested complete array
        assert!(!RedisVerifier::is_complete_resp_frame(b"*1\r\n*1\r\n")); // nested incomplete array
    }

    /// Tolerant mock: ignores unknown bytes, answers PING and foo bulk.
    /// Models a server that survives garbage and serves concurrent clients.
    async fn spawn_tolerant_mock() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    loop {
                        let n = match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let upper = String::from_utf8_lossy(&buf[..n]).to_uppercase();
                        let reply = if upper.contains("PING") {
                            Some(&b"+PONG\r\n"[..])
                        } else if upper.contains("FOO") {
                            Some(&b"$3\r\nfoo\r\n"[..])
                        } else {
                            // Anything else (garbage, partials): stay open, stay silent.
                            None
                        };
                        if let Some(r) = reply {
                            if socket.write_all(r).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn test_robustness_passes_on_tolerant_server() {
        let port = spawn_tolerant_mock().await;
        let v = RedisVerifier::new(port, Some("robust"));
        let out = v.run_robustness().await;
        assert_eq!(out.len(), 2);
        assert!(out[0].passed, "malformed: {:?}", out[0].error);
        assert!(out[1].passed, "slow sender: {:?}", out[1].error);
        assert_eq!(out[0].stage, 101);
        assert_eq!(out[1].stage, 102);
    }

    /// In-memory RESP mock with real key/value + counter semantics.
    /// `flip_get` poisons one GET reply to simulate a divergent candidate.
    async fn spawn_kv_mock(flip_get: bool) -> u16 {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let db: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let db = db.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let mut poisoned = false;
                    loop {
                        let n = match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        // Parse one array frame: *N then N bulk strings.
                        let frame = (|| -> Option<(Vec<String>, usize)> {
                            let s = std::str::from_utf8(&buf).ok()?;
                            let mut lines = s.split("\r\n");
                            let head = lines.next()?;
                            let count: usize = head.strip_prefix('*')?.parse().ok()?;
                            let mut args = Vec::new();
                            let mut consumed = head.len() + 2;
                            for _ in 0..count {
                                let h = lines.next()?;
                                let len: usize = h.strip_prefix('$')?.parse().ok()?;
                                consumed += h.len() + 2;
                                let data = lines.next()?;
                                if data.len() != len {
                                    return None;
                                }
                                args.push(data.to_string());
                                consumed += len + 2;
                            }
                            Some((args, consumed))
                        })();
                        let Some((args, used)) = frame else { continue };
                        buf.drain(..used);
                        if args.is_empty() {
                            continue;
                        }
                        let reply = {
                            let mut db = db.lock().unwrap();
                            match args[0].to_uppercase().as_str() {
                                "PING" => "+PONG\r\n".to_string(),
                                "SET" if args.len() == 3 => {
                                    db.insert(args[1].clone(), args[2].clone());
                                    "+OK\r\n".to_string()
                                }
                                "GET" if args.len() == 2 => match db.get(&args[1]) {
                                    Some(_) if flip_get && !poisoned => {
                                        poisoned = true;
                                        format!("${}\r\nWRONG\r\n", "WRONG".len())
                                    }
                                    Some(v) => format!("${}\r\n{}\r\n", v.len(), v),
                                    None => "$-1\r\n".to_string(),
                                },
                                "DEL" => {
                                    let n = args[1..]
                                        .iter()
                                        .filter(|k| db.remove(*k).is_some())
                                        .count();
                                    format!(":{}\r\n", n)
                                }
                                "EXISTS" => {
                                    let n =
                                        args[1..].iter().filter(|k| db.contains_key(*k)).count();
                                    format!(":{}\r\n", n)
                                }
                                "INCR" | "DECR" if args.len() == 2 => {
                                    let cur: i64 =
                                        db.get(&args[1]).and_then(|v| v.parse().ok()).unwrap_or(0);
                                    let nv = if args[0].to_uppercase() == "INCR" {
                                        cur + 1
                                    } else {
                                        cur - 1
                                    };
                                    db.insert(args[1].clone(), nv.to_string());
                                    format!(":{}\r\n", nv)
                                }
                                _ => "-ERR unknown\r\n".to_string(),
                            }
                        };
                        if socket.write_all(reply.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    /// Drive run_differential against a mock reference port by pointing a
    /// verifier at the mock candidate and overriding the ref dial. The
    /// verifier under test dials `ref_port`, so a mock there suffices.
    #[tokio::test]
    async fn test_differential_passes_on_identical_servers() {
        let cand = spawn_kv_mock(false).await;
        // Reference mock shares nothing but implements identical semantics.
        let reference = spawn_kv_mock(false).await;
        // Point the verifier at the candidate mock; patch ref dial by
        // binding order: run against candidate, ref via second mock.
        let v = RedisVerifier::new(cand, Some("differential"));
        let out = v.run_differential(reference).await;
        assert_eq!(out.len(), 1);
        assert!(
            out[0].passed,
            "identical servers must agree: {:?}",
            out[0].error
        );
        assert_eq!(out[0].stage, 103);
    }

    #[tokio::test]
    async fn test_differential_catches_divergence() {
        let cand = spawn_kv_mock(true).await;
        let reference = spawn_kv_mock(false).await;
        let v = RedisVerifier::new(cand, Some("differential"));
        let out = v.run_differential(reference).await;
        assert!(!out[0].passed, "divergent GET must fail");
        let err = out[0].error.as_deref().unwrap_or("");
        assert!(
            err.contains("divergence at op"),
            "must locate the op: {err}"
        );
        assert!(err.contains("WRONG"), "must show both replies: {err}");
    }

    #[tokio::test]
    async fn test_differential_reference_down_is_graceful() {
        let cand = spawn_kv_mock(false).await;
        // Reserve then drop a port so nothing listens there.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let v = RedisVerifier::new(cand, Some("differential"));
        let out = v.run_differential(dead_port).await;
        assert!(!out[0].passed);
        assert!(
            out[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("reference unreachable"),
            "must blame the harness, not the candidate: {:?}",
            out[0].error
        );
    }

    #[test]
    fn test_normalize_reply_classes() {
        // Errors reduce to category; values compare exactly.
        assert_eq!(RedisVerifier::normalize_reply("-ERR nope\r\n"), "-ERR");
        assert_eq!(
            RedisVerifier::normalize_reply("-WRONGTYPE blah\r\n"),
            "-ERR"
        );
        assert_eq!(RedisVerifier::normalize_reply("+OK\r\n"), "+OK");
        assert_eq!(RedisVerifier::normalize_reply("$3\r\nfoo\r\n"), "$3\r\nfoo");
        assert_eq!(RedisVerifier::normalize_reply(":17\r\n"), ":17");
        assert_ne!(
            RedisVerifier::normalize_reply(":17\r\n"),
            RedisVerifier::normalize_reply(":18\r\n")
        );
    }
    #[tokio::test]
    async fn test_robustness_fails_when_server_dies() {
        // Fragile mock: dies on first garbage byte, so the fresh-connection
        // PING must fail and the probe must say so (not hang, not pass).
        use tokio::io::AsyncReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept exactly one connection, then vanish forever.
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 64];
                let _ = socket.read(&mut buf).await;
                // Drop without reply and close the listener: reconnects fail.
            }
        });
        // Give the listener a moment, then burn its single accept.
        let v = RedisVerifier::new(port, Some("robust"));
        let out = v.run_robustness().await;
        assert!(!out[0].passed, "dead server must fail the malformed probe");
        assert!(
            out[0].error.as_deref().unwrap_or("").contains("PING")
                || out[0].error.as_deref().unwrap_or("").contains("reconnect"),
            "error must name the failed step: {:?}",
            out[0].error
        );
    }
    #[test]
    fn test_parse_info_frame_and_bare() {
        let frame = "$57\r\n# Server\r\ntotal_connections_received:3\r\nuptime_in_seconds:12\r\nconnected_clients:1\r\n";
        let m = RedisVerifier::parse_info(frame);
        assert_eq!(
            m.get("total_connections_received").map(String::as_str),
            Some("3")
        );
        assert_eq!(m.get("uptime_in_seconds").map(String::as_str), Some("12"));
        assert_eq!(m.len(), 3, "blank and section-header lines are ignored");
        // Bare body (no RESP header) and lowercase normalization.
        let m2 = RedisVerifier::parse_info("keyspace_hits:5\nkeyspace_misses:2\n");
        assert_eq!(m2.get("keyspace_misses").map(String::as_str), Some("2"));
        let m3 = RedisVerifier::parse_info("UPTIME_IN_SECONDS:9\n");
        assert_eq!(m3.get("uptime_in_seconds").map(String::as_str), Some("9"));
    }

    /// Stateful RESP mock for stage 8: replies +OK to SETs until `fail_after`
    /// successful ones, then -ERR; GET echoes stored values; PING works.
    fn spawn_enospc_mock(listener: TcpListener, fail_after: Option<usize>) {
        tokio::spawn(async move {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = sock.into_split();
            let mut r = tokio::io::BufReader::new(r);
            let mut store = std::collections::HashMap::<String, String>::new();
            let mut sets = 0usize;
            use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
            loop {
                let mut header = String::new();
                match r.read_line(&mut header).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let header = header.trim_end();
                if !header.starts_with('*') {
                    continue;
                }
                let n: usize = header[1..].parse().unwrap_or(0);
                let mut argv = Vec::new();
                for _ in 0..n {
                    let mut lh = String::new();
                    if r.read_line(&mut lh).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let lh = lh.trim_end();
                    let Some(rest) = lh.strip_prefix('$') else {
                        return;
                    };
                    let len: usize = rest.parse().unwrap_or(0);
                    let mut payload = vec![0u8; len + 2];
                    if r.read_exact(&mut payload).await.is_err() {
                        return;
                    }
                    argv.push(String::from_utf8_lossy(&payload[..len]).to_string());
                }
                let cmd = argv.first().cloned().unwrap_or_default().to_uppercase();
                let reply = match cmd.as_str() {
                    "PING" => "+PONG\r\n".to_string(),
                    "SET" => {
                        sets += 1;
                        if fail_after.map(|nf| sets > nf).unwrap_or(false) {
                            "-ERR persistence failure: no space left on device\r\n".to_string()
                        } else {
                            if argv.len() >= 3 {
                                store.insert(argv[1].clone(), argv[2].clone());
                            }
                            "+OK\r\n".to_string()
                        }
                    }
                    "GET" => {
                        let k = argv.get(1).cloned().unwrap_or_default();
                        match store.get(&k) {
                            Some(v) => format!("${}\r\n{}\r\n", v.len(), v),
                            None => "$-1\r\n".to_string(),
                        }
                    }
                    _ => "-ERR unknown command\r\n".to_string(),
                };
                if w.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }

    #[tokio::test]
    async fn test_mock_stage8_enospc_pass_and_never_full() {
        // PASS: the volume reports full after 12 successful writes.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        spawn_enospc_mock(l, Some(12));
        let r = RedisVerifier::new(port, Some("enospc-pass"))
            .test_stage_enospc()
            .await;
        assert!(r.passed, "expected pass: {:?}", r.error);

        // FAIL: writes never fail -> the log is not on the small volume.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        spawn_enospc_mock(l, None);
        let r = RedisVerifier::new(port, Some("enospc-never"))
            .test_stage_enospc()
            .await;
        assert!(!r.passed);
        assert!(r.error.unwrap_or_default().contains("never filled"));
    }

    #[tokio::test]
    async fn test_mock_redis_handshake_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];

                if let Ok(n) = socket.read(&mut buf).await {
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.contains("PING") {
                        let _ = socket.write_all(b"+PONG\r\n").await;
                    }
                }

                if let Ok(n) = socket.read(&mut buf).await {
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.contains("ECHO") {
                        let _ = socket.write_all(b"$14\r\nsubdollar_test\r\n").await;
                    }
                }
            }
        });

        let verifier = RedisVerifier::new(port, None);
        let res = verifier.test_stage1_handshake().await;
        assert!(
            res.passed,
            "Stage 1 should pass with mock server: {:?}",
            res.error
        );
    }

    #[tokio::test]
    async fn test_mock_redis_connection_refused() {
        let verifier = RedisVerifier::new(59999, None);
        let res = verifier.test_stage1_handshake().await;
        assert!(!res.passed);
        assert!(res.error.is_some());
    }

    #[tokio::test]
    async fn test_mock_redis_run_all_stages() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let mut stage2_deleted = false;
                    let mut stage3_count = 0;
                    let mut counter = 0i64;

                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let upper = req.to_uppercase();

                        if upper.contains("SET") && upper.contains("_P1") {
                            let _ = socket
                                .write_all(b"+OK\r\n+OK\r\n$2\r\nv1\r\n$2\r\nv2\r\n")
                                .await;
                            continue;
                        }
                        if upper.contains("INCR")
                            && (upper.contains("STR_NUM")
                                || upper.contains("BAD_INT")
                                || upper.contains("STR_MYRUN"))
                        {
                            let _ = socket
                                .write_all(b"-ERR value is not an integer or out of range\r\n")
                                .await;
                            continue;
                        }
                        if upper.contains("PING") {
                            let _ = socket.write_all(b"+PONG\r\n").await;
                        } else if upper.contains("ECHO") {
                            let _ = socket.write_all(b"$14\r\nsubdollar_test\r\n").await;
                        } else if upper.contains("SET") {
                            let _ = socket.write_all(b"+OK\r\n").await;
                        } else if upper.contains("GET") && upper.contains("TTL") {
                            stage3_count += 1;
                            if stage3_count == 1 {
                                let _ = socket.write_all(b"$7\r\nval_ttl\r\n").await;
                            } else {
                                let _ = socket.write_all(b"$-1\r\n").await;
                            }
                        } else if upper.contains("GET") && upper.contains("K_BENCH") {
                            if !stage2_deleted {
                                let _ = socket.write_all(b"$7\r\nv_bench\r\n").await;
                            } else {
                                let _ = socket.write_all(b"$-1\r\n").await;
                            }
                        } else if upper.contains("EXISTS") && upper.contains("K_BENCH") {
                            if !stage2_deleted {
                                let _ = socket.write_all(b":1\r\n").await;
                            } else {
                                let _ = socket.write_all(b":0\r\n").await;
                            }
                        } else if upper.contains("DEL") && upper.contains("K_BENCH") {
                            stage2_deleted = true;
                            let _ = socket.write_all(b":1\r\n").await;
                        } else if upper.contains("INCR") && upper.contains("CNT_NUM") {
                            counter += 1;
                            let _ = socket
                                .write_all(format!(":{}\r\n", counter).as_bytes())
                                .await;
                        } else if upper.contains("DECR") && upper.contains("CNT_NUM") {
                            counter -= 1;
                            let _ = socket
                                .write_all(format!(":{}\r\n", counter).as_bytes())
                                .await;
                        } else if upper.contains("INCR") && upper.contains("STR_NUM") {
                            let _ = socket
                                .write_all(b"-ERR value is not an integer or out of range\r\n")
                                .await;
                        } else if upper.contains("PUBLISH") {
                            let _ = socket.write_all(b":1\r\n").await;
                        } else if upper.contains("SUBSCRIBE") {
                            // Echo channel, confirm, then push a canned broadcast.
                            let parts: Vec<&str> = req.split("\r\n").collect();
                            let mut chan = "chan".to_string();
                            if let Some(i) =
                                parts.iter().position(|x| x.to_uppercase() == "SUBSCRIBE")
                            {
                                if let Some(c) = parts.get(i + 2) {
                                    chan = c.to_string();
                                }
                            }
                            let conf = format!(
                                "*3\r\n$9\r\nsubscribe\r\n${}\r\n{}\r\n:1\r\n",
                                chan.len(),
                                chan
                            );
                            let _ = socket.write_all(conf.as_bytes()).await;
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            let push = format!(
                                "*3\r\n$7\r\nmessage\r\n${}\r\n{}\r\n$12\r\nhello-fanout\r\n",
                                chan.len(),
                                chan
                            );
                            let _ = socket.write_all(push.as_bytes()).await;
                        } else {
                            let _ = socket.write_all(b"+OK\r\n").await;
                        }
                    }
                });
            }
        });

        let verifier = RedisVerifier::new(port, None);
        let summary = verifier.run_all().await;
        assert_eq!(
            summary.passed_count, 5,
            "Summary failed: {:?}",
            summary.stages
        );
        assert_eq!(summary.pass_rate, 100.0);
    }

    #[tokio::test]
    async fn test_mock_redis_run_all_stages_seeded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seed = "myrun123";

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let mut stage2_deleted = false;
                    let mut stage3_count = 0;
                    let mut counter = 0i64;

                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let upper = req.to_uppercase();

                        if upper.contains("SET") && upper.contains("_P1") {
                            let _ = socket
                                .write_all(b"+OK\r\n+OK\r\n$2\r\nv1\r\n$2\r\nv2\r\n")
                                .await;
                            continue;
                        }
                        if upper.contains("INCR")
                            && (upper.contains("STR_NUM")
                                || upper.contains("BAD_INT")
                                || upper.contains("STR_MYRUN"))
                        {
                            let _ = socket
                                .write_all(b"-ERR value is not an integer or out of range\r\n")
                                .await;
                            continue;
                        }
                        if upper.contains("PING") {
                            let _ = socket.write_all(b"+PONG\r\n").await;
                        } else if upper.contains("ECHO") {
                            let echo_str = "echo_myrun123";
                            let resp = format!("${}\r\n{}\r\n", echo_str.len(), echo_str);
                            let _ = socket.write_all(resp.as_bytes()).await;
                        } else if upper.contains("SET") {
                            let _ = socket.write_all(b"+OK\r\n").await;
                        } else if upper.contains("GET") && upper.contains("TTL_MYRUN123") {
                            stage3_count += 1;
                            if stage3_count == 1 {
                                let val = "val_myrun123";
                                let resp = format!("${}\r\n{}\r\n", val.len(), val);
                                let _ = socket.write_all(resp.as_bytes()).await;
                            } else {
                                let _ = socket.write_all(b"$-1\r\n").await;
                            }
                        } else if upper.contains("GET") && upper.contains("K_MYRUN123") {
                            if !stage2_deleted {
                                let val = "v_myrun123";
                                let resp = format!("${}\r\n{}\r\n", val.len(), val);
                                let _ = socket.write_all(resp.as_bytes()).await;
                            } else {
                                let _ = socket.write_all(b"$-1\r\n").await;
                            }
                        } else if upper.contains("EXISTS") && upper.contains("K_MYRUN123") {
                            if !stage2_deleted {
                                let _ = socket.write_all(b":1\r\n").await;
                            } else {
                                let _ = socket.write_all(b":0\r\n").await;
                            }
                        } else if upper.contains("DEL") && upper.contains("K_MYRUN123") {
                            stage2_deleted = true;
                            let _ = socket.write_all(b":1\r\n").await;
                        } else if upper.contains("INCR") && upper.contains("CNT_MYRUN123") {
                            counter += 1;
                            let _ = socket
                                .write_all(format!(":{}\r\n", counter).as_bytes())
                                .await;
                        } else if upper.contains("DECR") && upper.contains("CNT_MYRUN123") {
                            counter -= 1;
                            let _ = socket
                                .write_all(format!(":{}\r\n", counter).as_bytes())
                                .await;
                        } else if upper.contains("INCR") && upper.contains("STR_MYRUN123") {
                            let _ = socket
                                .write_all(b"-ERR value is not an integer or out of range\r\n")
                                .await;
                        } else if upper.contains("PUBLISH") {
                            let _ = socket.write_all(b":1\r\n").await;
                        } else if upper.contains("SUBSCRIBE") {
                            // Echo channel, confirm, then push a canned broadcast.
                            let parts: Vec<&str> = req.split("\r\n").collect();
                            let mut chan = "chan".to_string();
                            if let Some(i) =
                                parts.iter().position(|x| x.to_uppercase() == "SUBSCRIBE")
                            {
                                if let Some(c) = parts.get(i + 2) {
                                    chan = c.to_string();
                                }
                            }
                            let conf = format!(
                                "*3\r\n$9\r\nsubscribe\r\n${}\r\n{}\r\n:1\r\n",
                                chan.len(),
                                chan
                            );
                            let _ = socket.write_all(conf.as_bytes()).await;
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            let push = format!(
                                "*3\r\n$7\r\nmessage\r\n${}\r\n{}\r\n$12\r\nhello-fanout\r\n",
                                chan.len(),
                                chan
                            );
                            let _ = socket.write_all(push.as_bytes()).await;
                        } else {
                            let _ = socket.write_all(b"+OK\r\n").await;
                        }
                    }
                });
            }
        });

        let verifier = RedisVerifier::new(port, Some(seed));
        let summary = verifier.run_all().await;
        assert_eq!(
            summary.passed_count, 5,
            "Seeded summary failed: {:?}",
            summary.stages
        );
        assert_eq!(summary.pass_rate, 100.0);
    }

    #[tokio::test]
    async fn test_mock_redis_stage_failure_modes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let upper = req.to_uppercase();
                        if upper.contains("PING") {
                            let _ = socket.write_all(b"+WRONG\r\n").await;
                        } else if upper.contains("SET") {
                            let _ = socket.write_all(b"-ERR fail\r\n").await;
                        } else if upper.contains("INCR") {
                            let _ = socket.write_all(b"+NOT_INT\r\n").await;
                        } else {
                            let _ = socket.write_all(b"-ERR generic\r\n").await;
                        }
                    }
                });
            }
        });

        let verifier = RedisVerifier::new(port, None);
        let s1 = verifier.test_stage1_handshake().await;
        assert!(!s1.passed);
        assert!(s1.error.is_some());

        let s2 = verifier.test_stage2_key_value().await;
        assert!(!s2.passed);
        assert!(s2.error.is_some());

        let s3 = verifier.test_stage3_expiration().await;
        assert!(!s3.passed);
        assert!(s3.error.is_some());

        let s4 = verifier.test_stage4_counters().await;
        assert!(!s4.passed);
        assert!(s4.error.is_some());
    }

    #[tokio::test]
    async fn test_mock_redis_server_premature_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                // Drop socket immediately
                drop(socket);
            }
        });

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        let res = RedisVerifier::send_resp_cmd(&mut stream, &["PING"]).await;
        assert!(res.is_err());
    }
}
