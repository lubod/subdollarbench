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
        // Self-seeding: the slow frame's answer must be known regardless of
        // what earlier verifications (or the model's own smoke tests) left
        // in a durable WAL. The old version assumed `GET foo` returns the
        // value "foo" and failed advisory on any candidate carrying a `foo`
        // key (live 2026-10-14: an agent's own `SET foo bar` made every
        // slow-sender check red).
        let slow_key = format!("slow_{}", self.get_seed_prefix("snd"));
        let slow_val = "slow-ok";
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
        // Seed the answer the completing frame should observe.
        match Self::send_resp_cmd(&mut other, &["SET", &slow_key, slow_val]).await {
            Ok(r) if r.trim() == "+OK" => {}
            Ok(r) => return fail(format!("seed SET got {r:?} during half-open")),
            Err(e) => return fail(format!("seed SET failed during half-open: {e}")),
        }
        // Complete the slow frame; expect a well-formed bulk reply.
        let completion = format!("T\r\n${}\r\n{}\r\n", slow_key.len(), slow_key);
        if tokio::time::timeout(
            Duration::from_secs(3),
            slow.write_all(completion.as_bytes()),
        )
        .await
        .is_err()
        {
            return fail("completion write failed".to_string());
        }
        match tokio::time::timeout(Duration::from_secs(5), Self::read_resp_reply(&mut slow)).await {
            Ok(Ok(r)) if r.contains(slow_val) => StageResult {
                stage: 102,
                name,
                passed: true,
                error: None,
            },
            Ok(Ok(r)) => fail(format!("slow-frame reply {r:?}, want bulk {slow_val}")),
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

        // State-independent precondition: a repair re-verification runs
        // against a durable candidate whose WAL already holds this stage's
        // seeded counter from the previous attempt (seed = hash(run_id),
        // stable per stage), so a bare INCR would answer :2 and the ticket
        // would be scored as a false regression (live: 2026-10-14 s4 round
        // 2, "Expected :1, got :2"). DEL first, like any redis test that
        // needs a fresh counter; DEL itself is stage 2's contract.
        let _ = Self::send_resp_cmd(&mut stream, &["DEL", &counter_key]).await;

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

        // A second client connects (and leaves) inside the window: the
        // connection counter must observe it. Without this the check below
        // is unpassable — the only connection predates info0, so a
        // faithful server's total_connections_received delta is zero.
        let mut second = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("second client connect: {e}")),
        };
        match Self::send_resp_cmd(&mut second, &["PING"]).await {
            Ok(r) if r.trim() == "+PONG" => {}
            Ok(r) => return fail(format!("second client PING: {r:?}")),
            Err(e) => return fail(format!("second client PING failed: {e}")),
        }
        drop(second);

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

    /// Stage 10 (campaign dashboard ticket; explicit acceptance only): an
    /// offline dashboard beside RESP. Checks, in order:
    ///   - `GET /` -> 200 text/html, non-empty, no external resources
    ///     (absolute http(s) src/href references; the sandbox has no egress);
    ///   - `GET /api/stats` -> JSON mirroring the INFO counters; the deltas
    ///     must track scripted RESP traffic (same discipline as stage 7, so
    ///     a static stats page cannot pass);
    ///   - `GET /events` -> text/event-stream emitting at least one `data:`
    ///     JSON frame inside a 2s window while RESP traffic runs;
    ///   - the RESP port keeps answering while the SSE stream is open.
    pub async fn test_stage_dashboard(&self, dashboard_port: u16) -> StageResult {
        let name = "Stage 10: dashboard (HTTP + SSE)".to_string();
        let fail = |e: String| StageResult {
            stage: 10,
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
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return fail(format!("http client: {e}")),
        };
        let base = format!("http://127.0.0.1:{dashboard_port}");

        // ---- GET / : offline HTML ----
        let root = match client.get(format!("{base}/")).send().await {
            Ok(r) => r,
            Err(e) => return fail(format!("GET / failed: {e}")),
        };
        if root.status() != reqwest::StatusCode::OK {
            return fail(format!("GET / -> {}", root.status()));
        }
        let ctype = root
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if !ctype.contains("text/html") {
            return fail(format!("GET / content-type {ctype:?}, want text/html"));
        }
        let body = match root.text().await {
            Ok(t) => t,
            Err(e) => return fail(format!("GET / body: {e}")),
        };
        if body.trim().is_empty() {
            return fail("GET / returned an empty body".to_string());
        }
        for needle in ["src=\"http", "src='http", "href=\"http", "href='http"] {
            if body.contains(needle) {
                return fail(format!(
                    "GET / references an external resource ({needle}...); the sandbox has no egress"
                ));
            }
        }

        // ---- /api/stats deltas under scripted RESP traffic ----
        let stats0 = match Self::fetch_dashboard_stats(&client, &base).await {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        for f in REQUIRED {
            if !stats0.contains_key(*f) {
                return fail(format!(
                    "/api/stats is missing '{f}' (must mirror INFO; {} fields returned)",
                    stats0.len()
                ));
            }
        }
        // Fresh RESP connection inside the window so the connection counter
        // has something to observe (same trap stage 7 originally had).
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("resp connect: {e}")),
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        self.get_seed_prefix("dash").hash(&mut h);
        "dashboard-stage-v1".hash(&mut h);
        let mut rng = h.finish() | 1;
        fn xnext(r: &mut u64) -> u64 {
            *r ^= *r << 13;
            *r ^= *r >> 7;
            *r ^= *r << 17;
            *r
        }
        let hits = 4 + (xnext(&mut rng) % 9) as usize;
        let misses = 4 + (xnext(&mut rng) % 9) as usize;
        let tag = format!("{:08x}", xnext(&mut rng));
        let mut expect_commands: i64 = 0;
        for i in 0..hits {
            let k = format!("dash_{tag}_{i}");
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
            let k = format!("dash_miss_{tag}_{i}");
            match Self::send_resp_cmd(&mut stream, &["GET", &k]).await {
                Ok(r) if r.starts_with("$-1") => {}
                Ok(r) => return fail(format!("GET missing {k}: {r:?}")),
                Err(e) => return fail(format!("GET missing {k} failed: {e}")),
            }
            expect_commands += 1;
        }
        let ek = format!("dash_exp_{tag}");
        match Self::send_resp_cmd(&mut stream, &["SET", &ek, "1", "PX", "150"]).await {
            Ok(r) if r.trim() == "+OK" => {}
            Ok(r) => return fail(format!("SET PX: {r:?}")),
            Err(e) => return fail(format!("SET PX failed: {e}")),
        }
        expect_commands += 1;
        sleep(Duration::from_millis(400)).await;
        match Self::send_resp_cmd(&mut stream, &["GET", &ek]).await {
            Ok(r) if r.starts_with("$-1") => {}
            Ok(r) => return fail(format!("expired key still present on RESP: {r:?}")),
            Err(e) => return fail(format!("GET expired failed: {e}")),
        }
        expect_commands += 1;

        let stats1 = match Self::fetch_dashboard_stats(&client, &base).await {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        let delta = |f: &str| -> Option<i64> { Some(stats1.get(f)? - stats0.get(f)?) };
        for f in REQUIRED {
            match delta(f) {
                Some(v) if v >= 0 => {}
                Some(v) => return fail(format!("/api/stats '{f}' went backwards ({v})")),
                None => {
                    return fail(format!(
                        "/api/stats '{f}' missing or non-numeric after traffic"
                    ))
                }
            }
        }
        let hits_d = delta("keyspace_hits").unwrap_or(0);
        let miss_d = delta("keyspace_misses").unwrap_or(0);
        let cmds_d = delta("total_commands_processed").unwrap_or(0);
        let conns_d = delta("total_connections_received").unwrap_or(0);
        let exp_d = delta("expired_keys").unwrap_or(0);
        if hits_d < hits as i64 {
            return fail(format!(
                "/api/stats hits delta {hits_d} < executed hits {hits}"
            ));
        }
        if miss_d < misses as i64 {
            return fail(format!(
                "/api/stats misses delta {miss_d} < executed misses {misses}"
            ));
        }
        if cmds_d < expect_commands {
            return fail(format!(
                "/api/stats commands delta {cmds_d} < executed {expect_commands}"
            ));
        }
        if conns_d < 1 {
            return fail(format!(
                "/api/stats connections delta {conns_d}: a new RESP client did not count"
            ));
        }
        if exp_d < 1 {
            return fail(format!(
                "/api/stats expired_keys delta {exp_d} < 1 after a key expired"
            ));
        }
        if stats1.get("connected_clients").copied().unwrap_or(0) < 1 {
            return fail(
                "/api/stats connected_clients < 1 while clients are connected".to_string(),
            );
        }

        // ---- /events SSE under concurrent RESP traffic (raw socket so the
        // stream stays open; reqwest buffering would hide partial frames) ----
        let mut sse = match TcpStream::connect(("127.0.0.1", dashboard_port)).await {
            Ok(s) => s,
            Err(e) => return fail(format!("SSE connect: {e}")),
        };
        let req = format!(
            "GET /events HTTP/1.1\r\nHost: 127.0.0.1:{dashboard_port}\r\nAccept: text/event-stream\r\n\r\n"
        );
        if let Err(e) = sse.write_all(req.as_bytes()).await {
            return fail(format!("SSE request write: {e}"));
        }
        let mut sse_buf: Vec<u8> = Vec::new();
        let read_sse = async {
            let mut tmp = [0u8; 2048];
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match tokio::time::timeout(left, sse.read(&mut tmp)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => break,
                    Ok(Ok(n)) => {
                        sse_buf.extend_from_slice(&tmp[..n]);
                        if let Some(frame) = Self::parse_sse_data(&sse_buf) {
                            return Some(frame);
                        }
                    }
                    Err(_) => break,
                }
            }
            None
        };
        let resp_traffic = async {
            let mut answered = 0usize;
            let mut wanted = 0usize;
            if let Ok(mut c) = self.connect().await {
                for i in 0..12 {
                    let k = format!("sse_{tag}_{i}");
                    wanted += 1;
                    if Self::send_resp_cmd(&mut c, &["SET", &k, "v"]).await.is_ok() {
                        answered += 1;
                    }
                    wanted += 1;
                    if Self::send_resp_cmd(&mut c, &["GET", &k]).await.is_ok() {
                        answered += 1;
                    }
                    sleep(Duration::from_millis(40)).await;
                }
            }
            (answered, wanted)
        };
        let (sse_frame, (answered, wanted)) = tokio::join!(read_sse, resp_traffic);
        if wanted == 0 || answered < wanted {
            return fail(format!(
                "RESP degraded while the SSE stream was open ({answered}/{wanted} answered)"
            ));
        }
        let head = String::from_utf8_lossy(&sse_buf[..sse_buf.len().min(1024)]).to_lowercase();
        if !head.starts_with("http/1.1 200") && !head.starts_with("http/1.0 200") {
            let shown = String::from_utf8_lossy(&sse_buf[..sse_buf.len().min(80)]).to_string();
            return fail(format!("GET /events status line: {shown:?}"));
        }
        if !head.contains("text/event-stream") {
            return fail("GET /events content-type is not text/event-stream".to_string());
        }
        let Some(frame) = sse_frame else {
            return fail(
                "no SSE `data:` frame within 2s (stream open, RESP traffic running)".to_string(),
            );
        };
        if serde_json::from_str::<serde_json::Value>(&frame).is_err() {
            return fail(format!("SSE data frame is not JSON: {frame:?}"));
        }
        StageResult {
            stage: 10,
            name,
            passed: true,
            error: None,
        }
    }

    /// Stage 11 (campaign security ticket; explicit acceptance only): the
    /// candidate runs with `SDB_PASSWORD` set and a 64MB memory cap.
    ///   - RESP: every command except AUTH is refused before a correct
    ///     `AUTH <password>`; a wrong password errors;
    ///   - dashboard: HTTP Basic auth is required on every route;
    ///   - oversized frames (a bulk header claiming 64MB, a multibulk
    ///     claiming 2M args) must be answered or closed WITHOUT allocating
    ///     them — under the 64MB cap, buffering would kill the process — and
    ///     a valid PING must still work afterwards;
    ///   - a malformed-frame corpus never kills or wedges the server (a
    ///     fresh AUTH+PING passes after every payload);
    ///   - a dozen half-open slow-loris connections do not starve a normal
    ///     client.
    pub async fn test_stage_security(&self, dashboard_port: u16, password: &str) -> StageResult {
        let name = "Stage 11: security (auth, caps, fuzz, slow-loris)".to_string();
        let fail = |e: String| StageResult {
            stage: 11,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };

        // ---- RESP auth gate ----
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("resp connect: {e}")),
        };
        match Self::send_resp_cmd(&mut stream, &["PING"]).await {
            Ok(r) if r.starts_with('-') && r.to_uppercase().contains("AUTH") => {}
            Ok(r) => {
                return fail(format!(
                    "PING before AUTH returned {r:?}, want a -NOAUTH error"
                ))
            }
            Err(e) => return fail(format!("PING before AUTH failed: {e}")),
        }
        match Self::send_resp_cmd(&mut stream, &["AUTH", "definitely-wrong"]).await {
            Ok(r) if r.starts_with('-') => {}
            Ok(r) => return fail(format!("AUTH with a wrong password returned {r:?}")),
            Err(e) => return fail(format!("wrong-password AUTH failed: {e}")),
        }
        match Self::send_resp_cmd(&mut stream, &["AUTH", password]).await {
            Ok(r) if r.trim() == "+OK" => {}
            Ok(r) => return fail(format!("AUTH with the right password returned {r:?}")),
            Err(e) => return fail(format!("right-password AUTH failed: {e}")),
        }
        match Self::send_resp_cmd(&mut stream, &["PING"]).await {
            Ok(r) if r.trim() == "+PONG" => {}
            Ok(r) => return fail(format!("PING after AUTH returned {r:?}")),
            Err(e) => return fail(format!("PING after AUTH failed: {e}")),
        }
        let k = format!("sec_seed_{}", self.get_seed_prefix("sec"));
        match Self::send_resp_cmd(&mut stream, &["SET", &k, "v"]).await {
            Ok(r) if r.trim() == "+OK" => {}
            Ok(r) => return fail(format!("SET after AUTH returned {r:?}")),
            Err(e) => return fail(format!("SET after AUTH failed: {e}")),
        }

        // ---- dashboard behind the same password ----
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return fail(format!("http client: {e}")),
        };
        let base = format!("http://127.0.0.1:{dashboard_port}");
        match client.get(format!("{base}/")).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {}
            Ok(r) => {
                return fail(format!(
                    "GET / without credentials -> {}, want 401",
                    r.status()
                ))
            }
            Err(e) => return fail(format!("GET / without credentials failed: {e}")),
        }
        match client
            .get(format!("{base}/api/stats"))
            .basic_auth("ops", Some(password))
            .send()
            .await
        {
            Ok(r) if r.status() == reqwest::StatusCode::OK => {}
            Ok(r) => {
                return fail(format!(
                    "GET /api/stats with Basic auth -> {}, want 200",
                    r.status()
                ))
            }
            Err(e) => return fail(format!("GET /api/stats with Basic auth failed: {e}")),
        }

        // ---- oversized frames: answer or close; never buffer ----
        let oversize: &[(&str, &[u8])] = &[
            (
                "bulk header claiming 64MB",
                b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$67108864\r\n",
            ),
            ("multibulk claiming 2M args", b"*2000000\r\n"),
        ];
        for (label, probe) in oversize {
            if let Err(e) = self.security_probe_reacts(probe, label).await {
                return fail(e);
            }
            if let Err(e) = self.security_auth_ping(password).await {
                return fail(format!("after the {label} probe: {e}"));
            }
        }

        // ---- malformed-frame corpus ----
        let corpus: Vec<Vec<u8>> = vec![
            b"\x00\x01\x02\xff\xfe".to_vec(),
            b"*3\r\n$3\r\nSET\r\n".to_vec(),
            b"$-2\r\n".to_vec(),
            b"*999999999\r\n".to_vec(),
            b"+OK\r\n".to_vec(),
            b"\r\n\r\n\r\n".to_vec(),
            b"NO SUCH COMMAND AT ALL\r\n".to_vec(),
            b"*2\r\n$3\r\nGET\r\n$100000000\r\n".to_vec(),
        ];
        for (i, payload) in corpus.iter().enumerate() {
            match self.connect().await {
                Ok(mut c) => {
                    let _ = c.write_all(payload).await;
                    let mut tmp = [0u8; 256];
                    let _ =
                        tokio::time::timeout(Duration::from_millis(200), c.read(&mut tmp)).await;
                }
                Err(e) => return fail(format!("fuzz connect #{i}: {e}")),
            }
            if let Err(e) = self.security_auth_ping(password).await {
                return fail(format!("after fuzz payload #{i}: {e}"));
            }
        }

        // ---- slow-loris: half-open senders must not starve a client ----
        let mut loris = Vec::new();
        for _ in 0..12 {
            match self.connect().await {
                Ok(mut c) => {
                    let _ = c.write_all(b"*3\r\n$3\r\nSET\r\n").await;
                    loris.push(c);
                }
                Err(e) => return fail(format!("slow-loris connect: {e}")),
            }
        }
        let starve =
            tokio::time::timeout(Duration::from_secs(2), self.security_auth_ping(password)).await;
        drop(loris);
        match starve {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return fail(format!("client starved by slow-loris: {e}")),
            Err(_) => {
                return fail(
                    "AUTH+PING timed out while 12 half-open connections idled (slow-loris not isolated)"
                        .to_string(),
                )
            }
        }
        StageResult {
            stage: 11,
            name,
            passed: true,
            error: None,
        }
    }

    /// Connect, AUTH, PING — the post-incident liveness probe.
    async fn security_auth_ping(&self, password: &str) -> std::result::Result<(), String> {
        let mut c = self.connect().await.map_err(|e| e.to_string())?;
        match Self::send_resp_cmd(&mut c, &["AUTH", password]).await {
            Ok(r) if r.trim() == "+OK" => {}
            Ok(r) => return Err(format!("AUTH returned {r:?}")),
            Err(e) => return Err(format!("AUTH failed: {e}")),
        }
        match Self::send_resp_cmd(&mut c, &["PING"]).await {
            Ok(r) if r.trim() == "+PONG" => Ok(()),
            Ok(r) => Err(format!("PING returned {r:?}")),
            Err(e) => Err(format!("PING failed: {e}")),
        }
    }

    /// An oversized frame must get a reaction (an error reply or a close)
    /// within 3s. Silence means the server is trying to buffer it — under
    /// the stage's 64MB memory cap that is a process kill, not a policy.
    async fn security_probe_reacts(
        &self,
        probe: &[u8],
        label: &str,
    ) -> std::result::Result<(), String> {
        let mut c = self.connect().await.map_err(|e| e.to_string())?;
        c.write_all(probe).await.map_err(|e| e.to_string())?;
        let mut tmp = [0u8; 512];
        match tokio::time::timeout(Duration::from_secs(3), c.read(&mut tmp)).await {
            Ok(Ok(_)) | Ok(Err(_)) => Ok(()), // bytes back or connection closed: rejected
            Err(_) => Err(format!(
                "no reaction to the {label} within 3s (buffering it? cap must reject up front)"
            )),
        }
    }

    /// Stage 12 (campaign perf ticket; explicit acceptance only): pipelined
    /// RESP correctness plus a latency sanity budget. The throughput gate
    /// itself (>= PERF_GATE_RATIO x own genesis measurement) is applied by
    /// the pipeline after the same bench method runs for this stage, so the
    /// numbers are comparable; this stage guarantees the protocol works
    /// under pipelining and that a sane GET is not pathologically slow.
    pub async fn test_stage_perf(&self) -> StageResult {
        let name = "Stage 12: pipelining (in order) + latency".to_string();
        let fail = |e: String| StageResult {
            stage: 12,
            name: name.clone(),
            passed: false,
            error: Some(e),
        };
        let tag = self.get_seed_prefix("perf");
        let batch = 100usize;

        // ---- one write, 200 pipelined commands, ordered replies ----
        let mut stream = match self.connect().await {
            Ok(s) => s,
            Err(e) => return fail(format!("connect: {e}")),
        };
        let mut payload = Vec::new();
        for i in 0..batch {
            payload.extend_from_slice(
                Self::format_resp_cmd(&["SET", &format!("perf_{tag}_{i}"), &format!("v{i}")])
                    .as_bytes(),
            );
        }
        for i in 0..batch {
            payload.extend_from_slice(
                Self::format_resp_cmd(&["GET", &format!("perf_{tag}_{i}")]).as_bytes(),
            );
        }
        if let Err(e) = stream.write_all(&payload).await {
            return fail(format!("pipelined write: {e}"));
        }
        // Pipelined replies can coalesce in the receive buffer, so read
        // exactly one RESP frame per call — never the whole buffer (the
        // shared read_resp_reply returns everything it accumulated, which
        // is fine for one-outstanding-command flows and wrong here).
        let mut rbuf: Vec<u8> = Vec::new();
        for i in 0..batch {
            match Self::read_pipelined_frame(&mut stream, &mut rbuf).await {
                Ok(r) if r.trim() == "+OK" => {}
                Ok(r) => {
                    return fail(format!(
                        "pipelined SET #{i} reply {r:?}, want +OK (replies out of order?)"
                    ))
                }
                Err(e) => return fail(format!("pipelined SET #{i} read failed: {e}")),
            }
        }
        for i in 0..batch {
            match Self::read_pipelined_frame(&mut stream, &mut rbuf).await {
                Ok(r) if r.contains(&format!("v{i}")) => {}
                Ok(r) => {
                    return fail(format!(
                        "pipelined GET #{i} reply {r:?}, want v{i} (replies out of order?)"
                    ))
                }
                Err(e) => return fail(format!("pipelined GET #{i} read failed: {e}")),
            }
        }

        // ---- sequential GET latency budget ----
        let mut lat = Vec::with_capacity(300);
        for i in 0..300usize {
            let k = format!("perf_{tag}_{}", i % batch);
            let t0 = std::time::Instant::now();
            match Self::send_resp_cmd(&mut stream, &["GET", &k]).await {
                Ok(r) if r.starts_with('$') => {}
                Ok(r) => return fail(format!("sequential GET returned {r:?}")),
                Err(e) => return fail(format!("sequential GET failed: {e}")),
            }
            lat.push(t0.elapsed());
        }
        lat.sort();
        let p50 = lat[lat.len() / 2];
        let p99 = lat[(lat.len() * 99) / 100];
        let budget = Duration::from_millis(10);
        if p99 > budget {
            return fail(format!(
                "sequential GET p99 {:.1}ms exceeds the {:?} budget (p50 {:.1}ms)",
                p99.as_secs_f64() * 1000.0,
                budget,
                p50.as_secs_f64() * 1000.0
            ));
        }
        StageResult {
            stage: 12,
            name,
            passed: true,
            error: None,
        }
    }

    /// One RESP frame from a stream that may hold several coalesced frames
    /// (pipelined replies). `buf` carries the unconsumed tail between calls.
    async fn read_pipelined_frame(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(used) = Self::parse_resp_frame(buf) {
                let frame = String::from_utf8_lossy(&buf[..used]).to_string();
                buf.drain(..used);
                return Ok(frame);
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(anyhow!("read timed out waiting for a pipelined reply"));
            }
            let mut tmp = [0u8; 8192];
            let n = tokio::time::timeout(left, stream.read(&mut tmp))
                .await
                .map_err(|_| anyhow!("read timed out waiting for a pipelined reply"))??;
            if n == 0 {
                return Err(anyhow!("server closed the connection mid-pipeline"));
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// ROLE reply of a node, trimmed + lowercased ("master" / "replica").
    /// Accepts the RESP array form (`*3\r\n$6\r\nmaster...`), a bulk reply
    /// (`$6\r\nmaster`), or a simple string (`+master`). Used by the HA
    /// stage to decide who leads and who follows.
    pub async fn role_of(port: u16) -> Result<String> {
        let v = RedisVerifier::new(port, None);
        let mut stream = v.connect().await?;
        match Self::send_resp_cmd(&mut stream, &["ROLE"]).await {
            Ok(r) => {
                let mut next_line_is_word = false;
                for line in r.lines() {
                    let line = line.trim_end_matches('\r').trim();
                    if next_line_is_word {
                        return Ok(line.to_lowercase());
                    }
                    if line.starts_with('$') {
                        next_line_is_word = true;
                    }
                }
                // Simple-string form: +master / -ERR ...
                let first = r
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches('+')
                    .trim_start_matches('-')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                Ok(first)
            }
            Err(e) => Err(anyhow!("ROLE on port {port}: {e}")),
        }
    }

    /// `n` distinct acked writes on `port`; returns the acked pairs (the
    /// caller later checks how many survive on a survivor node).
    pub async fn write_probe_batch(
        port: u16,
        tag: &str,
        n: usize,
    ) -> Result<Vec<(String, String)>> {
        let v = RedisVerifier::new(port, None);
        let mut stream = v.connect().await?;
        let mut acked = Vec::with_capacity(n);
        for i in 0..n {
            let k = format!("ha_{tag}_{i}");
            let val = format!("hv_{tag}_{i}");
            match Self::send_resp_cmd(&mut stream, &["SET", &k, &val]).await {
                Ok(r) if r.trim() == "+OK" => acked.push((k, val)),
                Ok(r) => return Err(anyhow!("SET {k} on {port}: {r:?}")),
                Err(e) => return Err(anyhow!("SET {k} on {port}: {e}")),
            }
        }
        Ok(acked)
    }

    /// Count acked pairs that read back with the right value on `port`.
    pub async fn count_present(port: u16, pairs: &[(String, String)]) -> Result<usize> {
        let v = RedisVerifier::new(port, None);
        let mut stream = v.connect().await?;
        let mut found = 0usize;
        for (k, val) in pairs {
            match Self::send_resp_cmd(&mut stream, &["GET", k]).await {
                Ok(r) if r.contains(val) => found += 1,
                Ok(_) => {}
                Err(e) => return Err(anyhow!("GET {k} on {port}: {e}")),
            }
        }
        Ok(found)
    }

    /// GET `{base}/api/stats` and flatten the JSON into lowercased
    /// key -> integer (numbers directly, numeric strings parsed). Nested
    /// objects are walked so a `{"stats": {...}}` shape still mirrors INFO.
    async fn fetch_dashboard_stats(
        client: &reqwest::Client,
        base: &str,
    ) -> std::result::Result<std::collections::HashMap<String, i64>, String> {
        let resp = client
            .get(format!("{base}/api/stats"))
            .send()
            .await
            .map_err(|e| format!("GET /api/stats failed: {e}"))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!("GET /api/stats -> {}", resp.status()));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("GET /api/stats is not JSON: {e}"))?;
        let mut out = std::collections::HashMap::new();
        fn walk(v: &serde_json::Value, out: &mut std::collections::HashMap<String, i64>) {
            if let serde_json::Value::Object(map) = v {
                for (k, val) in map {
                    match val {
                        serde_json::Value::Number(n) => {
                            if let Some(i) = n.as_i64() {
                                out.insert(k.to_lowercase(), i);
                            } else if let Some(f) = n.as_f64() {
                                out.insert(k.to_lowercase(), f as i64);
                            }
                        }
                        serde_json::Value::String(s) => {
                            if let Ok(i) = s.trim().parse::<i64>() {
                                out.insert(k.to_lowercase(), i);
                            }
                        }
                        serde_json::Value::Object(_) => walk(val, out),
                        _ => {}
                    }
                }
            }
        }
        walk(&value, &mut out);
        Ok(out)
    }

    /// First `data:` payload in a raw SSE byte stream, if any. SSE frames
    /// are `data: <payload>` lines framed by blank lines; chunked transfer
    /// size lines simply never match the prefix.
    fn parse_sse_data(buf: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(buf);
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("data:") {
                let payload = rest.trim();
                if !payload.is_empty() {
                    return Some(payload.to_string());
                }
            }
        }
        None
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
    /// Also honors the stage-102 self-seed: any SET acks, and any frame
    /// carrying the slow key answers with the seeded value (the real
    /// verifier SETs `slow_<seed>` on the parallel connection, then reads
    /// it back through the half-open frame).
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
                        } else if upper.contains("SET") {
                            Some(&b"+OK\r\n"[..])
                        } else if upper.contains("SLOW_") {
                            Some(&b"$7\r\nslow-ok\r\n"[..])
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

    /// Stateful RESP mock with INFO counters for stage 7. Tracks global
    /// connections/commands/hits/misses/expired plus per-key PX expiry and
    /// serves them as INFO lines. `freeze_expired` models a hardcoded
    /// counter (`expired_keys` never moves) for the negative case.
    async fn spawn_info_mock(freeze_expired: bool) -> u16 {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[derive(Default)]
        struct Counters {
            conns: i64,
            cmds: i64,
            hits: i64,
            misses: i64,
            expired: i64,
        }
        type State = (HashMap<String, String>, HashMap<String, Instant>, Counters);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: Arc<Mutex<State>> = Arc::new(Mutex::new((
            HashMap::new(),
            HashMap::new(),
            Counters::default(),
        )));
        let started = Instant::now();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let state = state.clone();
                state.lock().unwrap().2.conns += 1;
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
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
                        let cmd = args[0].to_uppercase();
                        let reply = {
                            let (db, exp, c) = &mut *state.lock().unwrap();
                            c.cmds += 1;
                            match cmd.as_str() {
                                "PING" => "+PONG\r\n".to_string(),
                                "SET" if args.len() == 3 => {
                                    db.insert(args[1].clone(), args[2].clone());
                                    exp.remove(&args[1]);
                                    "+OK\r\n".to_string()
                                }
                                "SET" if args.len() == 5 && args[3].eq_ignore_ascii_case("PX") => {
                                    let ms: u64 = args[4].parse().unwrap_or(0);
                                    db.insert(args[1].clone(), args[2].clone());
                                    exp.insert(
                                        args[1].clone(),
                                        Instant::now() + std::time::Duration::from_millis(ms),
                                    );
                                    "+OK\r\n".to_string()
                                }
                                "GET" if args.len() == 2 => {
                                    let mut gone = false;
                                    if let Some(dl) = exp.get(&args[1]) {
                                        if Instant::now() >= *dl {
                                            db.remove(&args[1]);
                                            exp.remove(&args[1]);
                                            c.expired += 1;
                                            gone = true;
                                        }
                                    }
                                    match if gone { None } else { db.get(&args[1]) } {
                                        Some(v) => {
                                            c.hits += 1;
                                            format!("${}\r\n{}\r\n", v.len(), v)
                                        }
                                        None => {
                                            c.misses += 1;
                                            "$-1\r\n".to_string()
                                        }
                                    }
                                }
                                "INFO" => {
                                    let up = started.elapsed().as_secs();
                                    let expired = if freeze_expired { 0 } else { c.expired };
                                    let body = format!(
                                        "# Server\r\nuptime_in_seconds:{up}\r\n\
                                         connected_clients:1\r\nrejected_connections:0\r\n\
                                         # Stats\r\ntotal_connections_received:{}\r\n\
                                         total_commands_processed:{}\r\nkeyspace_hits:{}\r\n\
                                         keyspace_misses:{}\r\nexpired_keys:{expired}\r\n",
                                        c.conns, c.cmds, c.hits, c.misses
                                    );
                                    format!("${}\r\n{}\r\n", body.len(), body)
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

    /// Stage 7 must pass against a faithful counting server and must reject
    /// a frozen counter. Guards the verifier logic itself: writing this test
    /// caught the connection-delta check being unpassable (the test never
    /// opened a second connection; stage 7 had never executed live).
    #[tokio::test]
    async fn test_stage_info_mock_pass_and_frozen_counter_fail() {
        let port = spawn_info_mock(false).await;
        let v = RedisVerifier::new(port, Some("infomock"));
        let st = v.test_stage_info().await;
        assert!(st.passed, "faithful counters must pass: {:?}", st.error);

        let port = spawn_info_mock(true).await;
        let v = RedisVerifier::new(port, Some("infomock"));
        let st = v.test_stage_info().await;
        assert!(!st.passed, "a frozen expired_keys counter must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("expired_keys"),
            "error should name expired_keys: {:?}",
            st.error
        );
    }

    /// Combined RESP + HTTP mock for stage 10: RESP serves the KV subset
    /// with counters; HTTP serves `/` (offline HTML), `/api/stats` (JSON
    /// mirror of the counters) and `/events` (a `data:` SSE frame).
    /// `freeze_stats` serves constant counters; `starve_events` sends
    /// comment-only SSE so the frame check must fail.
    async fn spawn_dashboard_mock(freeze_stats: bool, starve_events: bool) -> (u16, u16) {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[derive(Default)]
        struct Counters {
            conns: i64,
            cmds: i64,
            hits: i64,
            misses: i64,
            expired: i64,
        }
        type State = (HashMap<String, String>, HashMap<String, Instant>, Counters);
        let state: Arc<Mutex<State>> = Arc::new(Mutex::new((
            HashMap::new(),
            HashMap::new(),
            Counters::default(),
        )));

        let resp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resp_port = resp_listener.local_addr().unwrap().port();
        {
            let state = state.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = resp_listener.accept().await {
                    let state = state.clone();
                    state.lock().unwrap().2.conns += 1;
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut tmp = [0u8; 4096];
                        loop {
                            let n = match socket.read(&mut tmp).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            };
                            buf.extend_from_slice(&tmp[..n]);
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
                            let cmd = args[0].to_uppercase();
                            let reply = {
                                let (db, exp, c) = &mut *state.lock().unwrap();
                                c.cmds += 1;
                                match cmd.as_str() {
                                    "PING" => "+PONG\r\n".to_string(),
                                    "SET" if args.len() == 3 => {
                                        db.insert(args[1].clone(), args[2].clone());
                                        exp.remove(&args[1]);
                                        "+OK\r\n".to_string()
                                    }
                                    "SET"
                                        if args.len() == 5
                                            && args[3].eq_ignore_ascii_case("PX") =>
                                    {
                                        let ms: u64 = args[4].parse().unwrap_or(0);
                                        db.insert(args[1].clone(), args[2].clone());
                                        exp.insert(
                                            args[1].clone(),
                                            Instant::now() + std::time::Duration::from_millis(ms),
                                        );
                                        "+OK\r\n".to_string()
                                    }
                                    "GET" if args.len() == 2 => {
                                        let mut gone = false;
                                        if let Some(dl) = exp.get(&args[1]) {
                                            if Instant::now() >= *dl {
                                                db.remove(&args[1]);
                                                exp.remove(&args[1]);
                                                c.expired += 1;
                                                gone = true;
                                            }
                                        }
                                        match if gone { None } else { db.get(&args[1]) } {
                                            Some(v) => {
                                                c.hits += 1;
                                                format!("${}\r\n{}\r\n", v.len(), v)
                                            }
                                            None => {
                                                c.misses += 1;
                                                "$-1\r\n".to_string()
                                            }
                                        }
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
        }

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        {
            let state = state.clone();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = http_listener.accept().await {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut tmp = [0u8; 4096];
                        let head_end = loop {
                            let n = match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break pos + 4;
                            }
                            if buf.len() > 16 * 1024 {
                                return;
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                        if path.starts_with("/events") {
                            let _ = sock
                                .write_all(
                                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n",
                                )
                                .await;
                            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                            if starve_events {
                                let _ = sock.write_all(b": keep-alive\n\n").await;
                            } else {
                                let cmds = state.lock().unwrap().2.cmds;
                                let _ = sock
                                    .write_all(
                                        format!("data: {{\"commands\":{cmds}}}\n\n").as_bytes(),
                                    )
                                    .await;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            return;
                        }
                        let (ctype, body) = if path == "/" {
                            (
                                "text/html; charset=utf-8",
                                "<!DOCTYPE html><html><body><h1>sdb-kv</h1></body></html>"
                                    .to_string(),
                            )
                        } else if path.starts_with("/api/stats") {
                            let (_, _, c) = &*state.lock().unwrap();
                            let (conns, cmds, hits, misses, expired) = if freeze_stats {
                                (1, 1, 1, 1, 0)
                            } else {
                                (c.conns, c.cmds, c.hits, c.misses, c.expired)
                            };
                            (
                                "application/json",
                                format!(
                                    "{{\"total_connections_received\":{conns},\"total_commands_processed\":{cmds},\"keyspace_hits\":{hits},\"keyspace_misses\":{misses},\"expired_keys\":{expired},\"rejected_connections\":0,\"connected_clients\":1,\"uptime_in_seconds\":1}}"
                                ),
                            )
                        } else {
                            ("text/plain", "nope".to_string())
                        };
                        let status = if path == "/" || path.starts_with("/api/stats") {
                            "200 OK"
                        } else {
                            "404 Not Found"
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    });
                }
            });
        }

        (resp_port, http_port)
    }

    /// Stage 10 must pass against a faithful HTTP+SSE dashboard mock and
    /// must reject a starved event stream and frozen stats.
    #[tokio::test]
    async fn test_stage_dashboard_mock_pass_and_failures() {
        let (resp_port, http_port) = spawn_dashboard_mock(false, false).await;
        let v = RedisVerifier::new(resp_port, Some("dashmock"));
        let st = v.test_stage_dashboard(http_port).await;
        assert!(st.passed, "faithful dashboard must pass: {:?}", st.error);

        let (resp_port, http_port) = spawn_dashboard_mock(false, true).await;
        let v = RedisVerifier::new(resp_port, Some("dashmock"));
        let st = v.test_stage_dashboard(http_port).await;
        assert!(!st.passed, "a starved SSE stream must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("data:"),
            "error should name the missing data frame: {:?}",
            st.error
        );

        let (resp_port, http_port) = spawn_dashboard_mock(true, false).await;
        let v = RedisVerifier::new(resp_port, Some("dashmock"));
        let st = v.test_stage_dashboard(http_port).await;
        assert!(!st.passed, "frozen stats must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("delta"),
            "error should name the delta: {:?}",
            st.error
        );
    }

    /// Threaded RESP + HTTP mock for stage 11. RESP enforces AUTH when
    /// `require_auth` (PING before AUTH -> -NOAUTH), rejects the oversize
    /// probes up front (unless `hang_on_oversize`, which models a server
    /// trying to buffer them), ignores garbage without dying, and never
    /// replies to partial frames (slow-loris stays half-open). HTTP
    /// requires Basic auth on every route.
    async fn spawn_security_mock(
        password: &str,
        require_auth: bool,
        hang_on_oversize: bool,
    ) -> (u16, u16) {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let password = Arc::new(password.to_string());
        let resp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resp_port = resp_listener.local_addr().unwrap().port();
        {
            let password = password.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = resp_listener.accept().await {
                    let password = password.clone();
                    tokio::spawn(async move {
                        let mut buf: Vec<u8> = Vec::new();
                        let mut authed = !require_auth;
                        let mut tmp = [0u8; 4096];
                        loop {
                            let n = match socket.read(&mut tmp).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            };
                            buf.extend_from_slice(&tmp[..n]);
                            let text = String::from_utf8_lossy(&buf).to_string();
                            // Oversize probes: reject on the header.
                            if text.contains("$67108864") || text.contains("*2000000") {
                                if hang_on_oversize {
                                    continue; // model a server that tries to buffer
                                }
                                let _ = socket
                                    .write_all(b"-ERR Protocol error: frame too large\r\n")
                                    .await;
                                break;
                            }
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
                                    if len > 8 * 1024 * 1024 || count > 1024 {
                                        return None;
                                    }
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
                            let Some((args, used)) = frame else {
                                // Invalid or partial: partial frames must
                                // stay unanswered; clear complete garbage.
                                let is_partial_set =
                                    buf.windows(11).any(|w| w == b"*3\r\n$3\r\nSET");
                                if buf.ends_with(b"\r\n") && !is_partial_set {
                                    buf.clear();
                                }
                                continue;
                            };
                            buf.drain(..used);
                            let cmd = args[0].to_uppercase();
                            let reply = if cmd == "AUTH" {
                                if args.len() == 2 && args[1] == *password {
                                    authed = true;
                                    "+OK\r\n".to_string()
                                } else {
                                    "-WRONGPASS invalid password\r\n".to_string()
                                }
                            } else if !authed {
                                "-NOAUTH Authentication required.\r\n".to_string()
                            } else {
                                match cmd.as_str() {
                                    "PING" => "+PONG\r\n".to_string(),
                                    "SET" => "+OK\r\n".to_string(),
                                    "GET" => "$-1\r\n".to_string(),
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
        }

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        {
            let password = password.clone();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = http_listener.accept().await {
                    let password = password.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut tmp = [0u8; 4096];
                        let head_end = loop {
                            let n = match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break pos + 4;
                            }
                            if buf.len() > 16 * 1024 {
                                return;
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let lower = head.to_lowercase();
                        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let b64 = base64_basic("ops", &password);
                        // Header names are case-insensitive; the base64 value
                        // is NOT, so match the prefix on the lowered head and
                        // compare the credential case-sensitively.
                        let authorized = lower
                            .find("authorization: basic ")
                            .map(|i| {
                                let start = i + "authorization: basic ".len();
                                head[start..].starts_with(&b64)
                            })
                            .unwrap_or(false);
                        let (status, ctype, body) = if !authorized {
                            (
                                "401 Unauthorized",
                                "text/plain",
                                "auth required".to_string(),
                            )
                        } else if path == "/" {
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                "<html><body>ok</body></html>".to_string(),
                            )
                        } else if path.starts_with("/api/stats") {
                            (
                                "200 OK",
                                "application/json",
                                "{\"keyspace_hits\":1}".to_string(),
                            )
                        } else {
                            ("404 Not Found", "text/plain", "nope".to_string())
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    });
                }
            });
        }

        (resp_port, http_port)
    }

    /// Minimal base64 for the mock's Basic credentials (test-only).
    fn base64_basic(user: &str, password: &str) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let input = format!("{user}:{password}");
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// Stage 11 must pass against a faithful secured mock and must reject a
    /// server that skips AUTH and one that buffers oversized frames.
    #[tokio::test]
    async fn test_stage_security_mock_pass_and_failures() {
        let (resp_port, http_port) = spawn_security_mock("s3cret-probe", true, false).await;
        let v = RedisVerifier::new(resp_port, Some("secmock"));
        let st = v.test_stage_security(http_port, "s3cret-probe").await;
        assert!(
            st.passed,
            "faithful secured server must pass: {:?}",
            st.error
        );

        let (resp_port, http_port) = spawn_security_mock("s3cret-probe", false, false).await;
        let v = RedisVerifier::new(resp_port, Some("secmock"));
        let st = v.test_stage_security(http_port, "s3cret-probe").await;
        assert!(!st.passed, "a server that skips AUTH must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("before AUTH"),
            "error should name the missing auth gate: {:?}",
            st.error
        );

        let (resp_port, http_port) = spawn_security_mock("s3cret-probe", true, true).await;
        let v = RedisVerifier::new(resp_port, Some("secmock"));
        let st = v.test_stage_security(http_port, "s3cret-probe").await;
        assert!(!st.passed, "buffering an oversized frame must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("buffering"),
            "error should name the buffering: {:?}",
            st.error
        );
    }

    /// Pipelining-capable KV mock: every complete frame already in the read
    /// buffer is answered before the next read (real pipelining). With
    /// `flip_first_get` the first GET reply is poisoned, modelling a server
    /// whose pipelined replies are out of order.
    async fn spawn_perf_mock(flip_first_get: bool) -> u16 {
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
                    let mut tmp = [0u8; 8192];
                    let mut flipped = false;
                    loop {
                        let n = match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        loop {
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
                            let Some((args, used)) = frame else { break };
                            buf.drain(..used);
                            let reply = {
                                let mut db = db.lock().unwrap();
                                match args[0].to_uppercase().as_str() {
                                    "SET" if args.len() == 3 => {
                                        db.insert(args[1].clone(), args[2].clone());
                                        "+OK\r\n".to_string()
                                    }
                                    "GET" if args.len() == 2 => {
                                        if flip_first_get && !flipped {
                                            flipped = true;
                                            "$5\r\nWRONG\r\n".to_string()
                                        } else {
                                            match db.get(&args[1]) {
                                                Some(v) => format!("${}\r\n{}\r\n", v.len(), v),
                                                None => "$-1\r\n".to_string(),
                                            }
                                        }
                                    }
                                    _ => "-ERR unknown\r\n".to_string(),
                                }
                            };
                            if socket.write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        port
    }

    /// Stage 12 must pass against a faithful pipelining mock and must catch
    /// a wrong pipelined reply with the order hint.
    #[tokio::test]
    async fn test_stage_perf_mock_pass_and_misorder_fail() {
        let port = spawn_perf_mock(false).await;
        let v = RedisVerifier::new(port, Some("perfmock"));
        let st = v.test_stage_perf().await;
        assert!(st.passed, "faithful pipelining must pass: {:?}", st.error);

        let port = spawn_perf_mock(true).await;
        let v = RedisVerifier::new(port, Some("perfmock"));
        let st = v.test_stage_perf().await;
        assert!(!st.passed, "a wrong pipelined reply must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("out of order"),
            "error should hint at reply order: {:?}",
            st.error
        );
    }

    /// Ticket 10 helpers: ROLE parsing accepts the RESP array, bulk and
    /// simple-string forms; write probes + presence counting work against a
    /// real socket mock.
    #[tokio::test]
    async fn test_ha_helpers() {
        use tokio::io::AsyncWriteExt;
        // Simple string form.
        let l1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p1 = l1.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l1.accept().await {
                let _ = s.write_all(b"+master\r\n").await;
            }
        });
        assert_eq!(RedisVerifier::role_of(p1).await.unwrap(), "master");

        // RESP array form (real redis): *3 [$6 replica, :0, *0].
        let l2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p2 = l2.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l2.accept().await {
                let _ = s.write_all(b"*3\r\n$6\r\nreplica\r\n:0\r\n*0\r\n").await;
            }
        });
        assert_eq!(RedisVerifier::role_of(p2).await.unwrap(), "replica");

        // Write probes + presence counting against the KV mock.
        let port = spawn_perf_mock(false).await;
        let pairs = RedisVerifier::write_probe_batch(port, "t", 3)
            .await
            .unwrap();
        assert_eq!(pairs.len(), 3);
        assert_eq!(RedisVerifier::count_present(port, &pairs).await.unwrap(), 3);
        let missing = vec![("nope".to_string(), "x".to_string())];
        assert_eq!(
            RedisVerifier::count_present(port, &missing).await.unwrap(),
            0
        );
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
