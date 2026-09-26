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
