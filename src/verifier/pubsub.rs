//! Pub/Sub verifier: raw-TCP WebSocket (RFC 6455) chat server.
//! Upgrade on `GET /chat`, then masked client text frames `SUB <chan>` /
//! `PUB <chan> <msg>`; server broadcasts `PUB <chan> <msg>` and pongs pings.
//! Self-contained: hand-rolled SHA-1, base64, WS framing (no new crates).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

use super::StageResult;

/// RFC 6455 Section 1.3 example key (fixed handshake test vector).
const TEST_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
/// Expected `Sec-WebSocket-Accept` for [`TEST_KEY`].
const TEST_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Fresh client nonce without new crates: nanos + pid + counter folded
/// into 16 bytes. Uniqueness per handshake is all the stub check needs.
fn fresh_nonce() -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::process::id() as u64;
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(t ^ ((p as u128) << 32)).to_le_bytes()[..8]);
    b[8..].copy_from_slice(&(c.wrapping_mul(0x9E3779B97F4A7C15)).to_le_bytes());
    b
}
const SOAK_SUBS: usize = 200;
const SOAK_MSGS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubsubTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
    pub soak_p99_ms: Option<f64>,
    pub soak_msgs_per_sec: Option<f64>,
}

pub struct PubsubVerifier {
    pub target_port: u16,
    pub run_seed: Option<String>,
}

/// Minimal SHA-1 (FIPS 180-4) returning the 20-byte digest.
pub fn sha1(message: &[u8]) -> [u8; 20] {
    let mut h = [
        0x67452301u32,
        0xEFCDAB89,
        0x98BADCFE,
        0x10325476,
        0xC3D2E1F0,
    ];
    let mut msg = message.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((message.len() as u64).wrapping_mul(8)).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, c) in chunk.chunks(4).enumerate().take(16) {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        // `i` selects the round's (f, k); `*wi` is this round's word. The
        // expansion loop above keeps its index form because it writes `w[i]`
        // while reading `w[i-3]` and friends.
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        for (hv, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *hv = hv.wrapping_add(v);
        }
    }
    let mut out = [0u8; 20];
    for (i, hv) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&hv.to_be_bytes());
    }
    out
}

/// Minimal base64 encoder (RFC 4648, padded).
pub fn base64_encode(input: &[u8]) -> String {
    const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((if c.len() > 1 { c[1] as u32 } else { 0 }) << 8)
            | (if c.len() > 2 { c[2] as u32 } else { 0 });
        s.push(ALPH[((n >> 18) & 63) as usize] as char);
        s.push(ALPH[((n >> 12) & 63) as usize] as char);
        s.push(if c.len() > 1 {
            ALPH[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 {
            ALPH[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    s
}

/// `Sec-WebSocket-Accept` for a client key: `base64(sha1(key + GUID))`.
pub fn ws_accept_key(client_key: &str) -> String {
    base64_encode(&sha1(
        format!("{}{}", client_key.trim(), WS_GUID).as_bytes(),
    ))
}

/// Encodes a masked client->server frame (payloads up to 65535 via len 126).
pub fn encode_masked_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut f = Vec::with_capacity(10 + payload.len());
    f.push(0x80 | (opcode & 0x0F));
    if payload.len() < 126 {
        f.push(0x80 | payload.len() as u8);
    } else {
        f.push(0x80 | 126);
        f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        f.push(b ^ mask[i % 4]);
    }
    f
}

/// Decodes one frame (masked or unmasked) from the head of `buf`; `None` if
/// incomplete. 64-bit lengths above 65535 are rejected (tests stay in 126-range).
pub fn decode_frame(buf: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let (opcode, masked) = (buf[0] & 0x0F, buf[1] & 0x80 != 0);
    let mut len = (buf[1] & 0x7F) as u64;
    let mut off = 2usize;
    if len == 126 {
        if buf.len() < 4 {
            return None;
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as u64;
        off = 4;
    } else if len == 127 {
        if buf.len() < 10 || u64::from_be_bytes(buf[2..10].try_into().ok()?) > 65535 {
            return None;
        }
        len = u64::from_be_bytes(buf[2..10].try_into().ok()?);
        off = 10;
    }
    let mlen = if masked { 4 } else { 0 };
    if buf.len() < off + mlen + len as usize {
        return None;
    }
    let payload = if masked {
        let m = &buf[off..off + 4];
        buf[off + 4..off + 4 + len as usize]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ m[i % 4])
            .collect()
    } else {
        buf[off..off + len as usize].to_vec()
    };
    Some((opcode, payload, off + mlen + len as usize))
}

/// Case-insensitive header lookup in a raw HTTP header block.
pub fn header_lookup(headers: &str, name: &str) -> Option<String> {
    headers.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
}

fn stage_result(stage: u32, name: &str, error: Option<String>) -> StageResult {
    StageResult {
        stage,
        name: name.to_string(),
        passed: error.is_none(),
        error,
    }
}

/// Unwraps a step or returns the failed `StageResult` immediately.
macro_rules! step {
    ($n:expr, $name:expr, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return stage_result($n, $name, Some(e.to_string())),
        }
    };
}

/// Handshake-completed connection with a read buffer so coalesced frames survive across reads.
struct WsConn {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl WsConn {
    async fn read_frame(&mut self, timeout: Duration) -> Result<(u8, Vec<u8>)> {
        let start = Instant::now();
        loop {
            if let Some((op, payload, used)) = decode_frame(&self.buf) {
                self.buf.drain(..used);
                return Ok((op, payload));
            }
            if start.elapsed() >= timeout {
                return Err(anyhow!("WS read timed out"));
            }
            let mut tmp = [0u8; 4096];
            let n = tokio::time::timeout(timeout - start.elapsed(), self.stream.read(&mut tmp))
                .await
                .map_err(|_| anyhow!("WS read timed out"))??;
            if n == 0 {
                return Err(anyhow!("Server closed WS connection"));
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
    async fn send(&mut self, opcode: u8, data: &[u8]) -> Result<()> {
        let mask = if opcode == 0x9 {
            [0x21, 0x43, 0x65, 0x87]
        } else {
            [0x12, 0x34, 0x56, 0x78]
        };
        self.stream
            .write_all(&encode_masked_frame(opcode, data, mask))
            .await?;
        self.stream.flush().await?;
        Ok(())
    }
}

impl PubsubVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        Self {
            target_port,
            run_seed: seed.map(|s| s.to_string()),
        }
    }
    pub fn get_seed_prefix<'a>(&'a self, default_val: &'a str) -> &'a str {
        super::get_seed_prefix(self.run_seed.as_deref(), default_val)
    }
    async fn connect_raw(&self) -> Result<TcpStream> {
        let addr = format!("127.0.0.1:{}", self.target_port);
        tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("Connection timed out to {}", addr))?
            .map_err(|e| anyhow!("Failed to connect to {}: {}", addr, e))
    }
    /// Writes the Upgrade request and reads the header block; returns `(header_text, leftover)`.
    async fn handshake(&self, stream: &mut TcpStream, key: &str) -> Result<(String, Vec<u8>)> {
        stream.write_all(format!("GET /chat HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n", self.target_port, key).as_bytes()).await?;
        stream.flush().await?;
        let mut head: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 1024];
        let res = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    return Err::<Vec<u8>, anyhow::Error>(anyhow!(
                        "Server closed connection during handshake"
                    ));
                }
                head.extend_from_slice(&tmp[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if head.len() > 8192 {
                    return Err(anyhow!("Handshake response header too large"));
                }
            }
            Ok::<Vec<u8>, anyhow::Error>(head)
        })
        .await
        .map_err(|_| anyhow!("Handshake read timed out"))??;
        let end = res
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(res.len());
        Ok((
            String::from_utf8_lossy(&res[..end]).to_string(),
            res[end..].to_vec(),
        ))
    }
    async fn ws_connect(&self, key: &str) -> Result<WsConn> {
        let mut stream = self.connect_raw().await?;
        let (headers, leftover) = self.handshake(&mut stream, key).await?;
        if !headers.lines().next().unwrap_or("").contains("101") {
            return Err(anyhow!(
                "Expected 101 Switching Protocols, got {:?}",
                headers.lines().next()
            ));
        }
        let expected = ws_accept_key(key);
        match header_lookup(&headers, "Sec-WebSocket-Accept") {
            Some(a) if a == expected => Ok(WsConn {
                stream,
                buf: leftover,
            }),
            other => Err(anyhow!(
                "Bad Sec-WebSocket-Accept: got {:?}, want {:?}",
                other,
                expected
            )),
        }
    }
    /// Reads text frames until one starting with `PUB <channel> ` arrives.
    async fn read_pub(
        &self,
        conn: &mut WsConn,
        channel: &str,
        timeout: Duration,
    ) -> Result<String> {
        let prefix = format!("PUB {} ", channel);
        let start = Instant::now();
        loop {
            if start.elapsed() >= timeout {
                return Err(anyhow!("Timed out waiting for PUB on '{}'", channel));
            }
            let (op, payload) = conn.read_frame(timeout - start.elapsed()).await?;
            if op == 0x8 {
                return Err(anyhow!("Server sent close frame"));
            }
            if op == 0x1 {
                let text = String::from_utf8_lossy(&payload).to_string();
                if text.starts_with(&prefix) {
                    return Ok(text);
                }
            }
        }
    }
    pub async fn test_stage1_handshake(&self) -> StageResult {
        // Two handshakes: the RFC vector (fixed key) AND a fresh random
        // key whose accept must equal ws_accept_key(key). A server that
        // echoes a hard-coded `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=` passes the
        // first and fails the second — exactly the stub this catches.
        let name = "Stage 1: WebSocket Handshake (101 + Accept, dynamic key)";
        let mut stream = step!(1, name, self.connect_raw().await);
        let (headers, _) = step!(
            1,
            name,
            self.handshake(&mut stream, TEST_KEY)
                .await
                .map_err(|e| anyhow!("Handshake failed: {}", e))
        );
        if !headers.lines().next().unwrap_or("").contains("101") {
            return stage_result(
                1,
                name,
                Some(format!("Expected 101, got {:?}", headers.lines().next())),
            );
        }
        match header_lookup(&headers, "Sec-WebSocket-Accept") {
            Some(a) if a == TEST_ACCEPT => {}
            other => {
                return stage_result(
                    1,
                    name,
                    Some(format!(
                        "Bad Accept: got {:?}, want {:?}",
                        other, TEST_ACCEPT
                    )),
                )
            }
        }
        // Fresh key: must NOT echo the vector answer.
        let fresh_key = base64_encode(&fresh_nonce());
        let want = ws_accept_key(&fresh_key);
        let mut stream2 = step!(1, name, self.connect_raw().await);
        let (headers2, _) = step!(
            1,
            name,
            self.handshake(&mut stream2, &fresh_key)
                .await
                .map_err(|e| anyhow!("Handshake (fresh key) failed: {}", e))
        );
        match header_lookup(&headers2, "Sec-WebSocket-Accept") {
            Some(a) if a == want => stage_result(1, name, None),
            other => stage_result(
                1,
                name,
                Some(format!(
                    "Static Accept (hard-coded vector?): got {:?}, want {:?} for fresh key",
                    other, want
                )),
            ),
        }
    }
    pub async fn test_stage2_broadcast(&self) -> StageResult {
        let name = "Stage 2: Pub/Sub Broadcast (2 subscribers)";
        let seed = self.get_seed_prefix("bench");
        let channel = format!("chan_{}", seed);
        let mut subs = Vec::with_capacity(2);
        for i in 1..=2 {
            let mut sub = step!(
                2,
                name,
                self.ws_connect(TEST_KEY)
                    .await
                    .map_err(|e| anyhow!("sub{} connect: {}", i, e))
            );
            step!(
                2,
                name,
                sub.send(0x1, format!("SUB {}", channel).as_bytes())
                    .await
                    .map_err(|e| anyhow!("sub{} SUB: {}", i, e))
            );
            subs.push(sub);
        }
        sleep(Duration::from_millis(150)).await;
        let mut publ = step!(
            2,
            name,
            self.ws_connect(TEST_KEY)
                .await
                .map_err(|e| anyhow!("publisher connect: {}", e))
        );
        let want = format!("PUB {} hello-{}", channel, seed);
        step!(
            2,
            name,
            publ.send(0x1, want.as_bytes())
                .await
                .map_err(|e| anyhow!("PUB: {}", e))
        );
        for (i, sub) in subs.iter_mut().enumerate() {
            match self.read_pub(sub, &channel, Duration::from_secs(5)).await {
                Ok(got) if got == want => {}
                Ok(got) => {
                    return stage_result(
                        2,
                        name,
                        Some(format!("sub{} got {:?}, want {:?}", i + 1, got, want)),
                    )
                }
                Err(e) => {
                    return stage_result(2, name, Some(format!("sub{} missed PUB: {}", i + 1, e)))
                }
            }
        }
        stage_result(2, name, None)
    }
    pub async fn test_stage3_isolation_pong(&self) -> StageResult {
        let name = "Stage 3: Channel Isolation + Ping/Pong";
        let seed = self.get_seed_prefix("bench");
        let (channel, other) = (format!("chan_{}", seed), format!("other_{}", seed));
        let mut sub = step!(
            3,
            name,
            self.ws_connect(TEST_KEY)
                .await
                .map_err(|e| anyhow!("subscriber connect: {}", e))
        );
        step!(
            3,
            name,
            sub.send(0x1, format!("SUB {}", channel).as_bytes())
                .await
                .map_err(|e| anyhow!("SUB: {}", e))
        );
        sleep(Duration::from_millis(150)).await;
        let mut publ = step!(
            3,
            name,
            self.ws_connect(TEST_KEY)
                .await
                .map_err(|e| anyhow!("publisher connect: {}", e))
        );
        step!(
            3,
            name,
            publ.send(0x1, format!("PUB {} isolated-{}", other, seed).as_bytes())
                .await
                .map_err(|e| anyhow!("probe PUB: {}", e))
        );
        match sub.read_frame(Duration::from_millis(500)).await {
            Err(e) if e.to_string().contains("timed out") => {}
            Ok((op, payload)) => {
                return stage_result(
                    3,
                    name,
                    Some(format!(
                        "Isolation violated: opcode {:#x} {:?} after PUB on '{}'",
                        op,
                        String::from_utf8_lossy(&payload),
                        other
                    )),
                )
            }
            Err(e) => return stage_result(3, name, Some(format!("Isolation check: {}", e))),
        }
        step!(
            3,
            name,
            sub.send(0x9, format!("hb-{}", seed).as_bytes())
                .await
                .map_err(|e| anyhow!("ping send: {}", e))
        );
        let start = Instant::now();
        loop {
            if start.elapsed() >= Duration::from_secs(3) {
                return stage_result(3, name, Some("No pong reply to ping within 3s".to_string()));
            }
            match sub
                .read_frame(Duration::from_secs(3) - start.elapsed())
                .await
            {
                Ok((0xA, _)) => break,
                Ok((0x8, _)) => {
                    return stage_result(
                        3,
                        name,
                        Some("Server closed connection on ping".to_string()),
                    )
                }
                Ok(_) => continue,
                Err(e) => return stage_result(3, name, Some(format!("pong wait: {}", e))),
            }
        }
        stage_result(3, name, None)
    }
    pub async fn test_stage4_soak(&self) -> (StageResult, Option<f64>, Option<f64>) {
        let name = "Stage 4: Soak (200 subs x 20 msgs)";
        let fail = |msg: String| (stage_result(4, name, Some(msg)), None, None);
        let seed = self.get_seed_prefix("bench").to_string();
        let channel = format!("soak_{}", seed);
        let mut streams = Vec::with_capacity(SOAK_SUBS);
        for i in 0..SOAK_SUBS {
            let mut c = match self.ws_connect(TEST_KEY).await {
                Ok(c) => c,
                Err(e) => return fail(format!("sub {} connect: {}", i, e)),
            };
            if let Err(e) = c.send(0x1, format!("SUB {}", channel).as_bytes()).await {
                return fail(format!("sub {} SUB: {}", i, e));
            }
            streams.push(c.stream);
        }
        sleep(Duration::from_millis(200)).await;
        let start = Instant::now();
        let prefix = format!("PUB {} ", channel);
        let mut readers = Vec::with_capacity(SOAK_SUBS);
        for stream in streams {
            let prefix = prefix.clone();
            readers.push(tokio::spawn(async move {
                let mut c = WsConn {
                    stream,
                    buf: Vec::new(),
                };
                let (mut got, t0, deadline): (Vec<(usize, Duration)>, Instant, Duration) =
                    (Vec::new(), Instant::now(), Duration::from_secs(15));
                while got.len() < SOAK_MSGS && t0.elapsed() < deadline {
                    match c.read_frame(deadline - t0.elapsed()).await {
                        Ok((0x1, payload)) => {
                            let text = String::from_utf8_lossy(&payload).to_string();
                            if let Some(idx) = text
                                .strip_prefix(prefix.as_str())
                                .and_then(|b| b.strip_prefix("soakmsg-"))
                                .and_then(|r| r.split('-').next())
                                .and_then(|s| s.parse::<usize>().ok())
                            {
                                got.push((idx, start.elapsed()));
                            }
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                got
            }));
        }
        let mut publ = match self.ws_connect(TEST_KEY).await {
            Ok(c) => c,
            Err(e) => return fail(format!("publisher connect: {}", e)),
        };
        let mut pub_at = Vec::with_capacity(SOAK_MSGS);
        for i in 0..SOAK_MSGS {
            if let Err(e) = publ
                .send(
                    0x1,
                    format!("PUB {} soakmsg-{}-{}", channel, i, seed).as_bytes(),
                )
                .await
            {
                return fail(format!("PUB {}: {}", i, e));
            }
            pub_at.push(start.elapsed());
        }
        // Distinctness: duplicates of one index must not count as deliveries
        // of twenty. Per-subscriber, dedupe by index; only in-range indices
        // count, and latency needs the matching publish timestamp.
        let (mut delivered, mut lat) = (0u64, Vec::with_capacity(SOAK_SUBS * SOAK_MSGS));
        for h in readers {
            if let Ok(got) = h.await {
                let mut seen = std::collections::HashSet::with_capacity(got.len());
                for (idx, arr) in got {
                    if idx < pub_at.len() && seen.insert(idx) {
                        delivered += 1;
                        lat.push(arr.saturating_sub(pub_at[idx]));
                    }
                }
            }
        }
        let elapsed = start.elapsed();
        let mps = if elapsed.as_secs_f64() > 0.0 {
            Some(delivered as f64 / elapsed.as_secs_f64())
        } else {
            None
        };
        let p99 = if lat.is_empty() {
            None
        } else {
            lat.sort();
            Some(lat[lat.len() * 99 / 100].as_secs_f64() * 1000.0)
        };
        let expected = (SOAK_SUBS * SOAK_MSGS) as u64;
        if delivered < expected {
            return (
                stage_result(
                    4,
                    name,
                    Some(format!(
                        "Delivery {}/{}, p99 {:.1}ms, {:.0} msgs/s",
                        delivered,
                        expected,
                        p99.unwrap_or(-1.0),
                        mps.unwrap_or(0.0)
                    )),
                ),
                p99,
                mps,
            );
        }
        match p99 {
            Some(p) if p >= 100.0 => (
                stage_result(
                    4,
                    name,
                    Some(format!(
                        "p99 {:.1}ms >= 100ms budget ({}/{}, {:.0} msgs/s)",
                        p,
                        delivered,
                        expected,
                        mps.unwrap_or(0.0)
                    )),
                ),
                p99,
                mps,
            ),
            _ => (stage_result(4, name, None), p99, mps),
        }
    }
    pub async fn run_all(&self) -> PubsubTestSummary {
        let mut stages = Vec::new();
        stages.push(self.test_stage1_handshake().await);
        stages.push(self.test_stage2_broadcast().await);
        stages.push(self.test_stage3_isolation_pong().await);
        let (s4, p99, mps) = self.test_stage4_soak().await;
        stages.push(s4);
        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        PubsubTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate: passed_count as f64 / total_stages as f64 * 100.0,
            soak_p99_ms: p99,
            soak_msgs_per_sec: mps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    fn hex(d: &[u8]) -> String {
        d.iter().map(|b| format!("{:02x}", b)).collect()
    }
    #[test]
    fn test_crypto_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // The two vectors above are both a single partial block, so they never
        // run the 16..80 expansion. These do: one 56-byte message (one full
        // block after padding) and the canonical 1,000,000-'a' case, which
        // runs the per-chunk loop 15,625 times.
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&sha1(&vec![b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(&sha1(b"abc")), "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=");
        assert_eq!(ws_accept_key(TEST_KEY), TEST_ACCEPT);
        assert_eq!(ws_accept_key(TEST_KEY), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        // Fresh keys must NOT map to the vector answer: a stub that
        // hard-codes the accept string fails any key but the vector's.
        for k in ["AAAAAAAAAAAAAAAAAAAAAA==", "AQIDBAUGBwgJCgsMDQ4PEA=="] {
            assert_ne!(ws_accept_key(k), TEST_ACCEPT, "static accept for {k}");
            assert_eq!(ws_accept_key(k), ws_accept_key(k), "deterministic for {k}");
        }
        assert_ne!(fresh_nonce(), fresh_nonce(), "nonces must vary");
    }
    #[test]
    fn test_frame_roundtrip() {
        let frame = encode_masked_frame(0x1, b"SUB chan_abc", [0xAA, 0xBB, 0xCC, 0xDD]);
        assert!(frame[1] & 0x80 != 0, "client frames must be masked");
        let (op, out, used) = decode_frame(&frame).expect("complete frame");
        assert_eq!((op, used), (0x1, frame.len()));
        assert_eq!(out, b"SUB chan_abc");
        let payload = vec![b'x'; 300];
        let big = encode_masked_frame(0x1, &payload, [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(big[1] & 0x7F, 126);
        let (op, out, used) = decode_frame(&big).expect("complete frame");
        assert_eq!((op, used), (0x1, big.len()));
        assert_eq!(out, payload);
        let mut srv = vec![0x81u8, 126];
        srv.extend_from_slice(&300u16.to_be_bytes());
        srv.extend_from_slice(&payload);
        let (op, out, _) = decode_frame(&srv).expect("complete frame");
        assert_eq!(op, 0x1);
        assert_eq!(out, payload);
        assert!(decode_frame(&big[..3]).is_none());
        let mut huge = vec![0x81u8, 127];
        huge.extend_from_slice(&70000u64.to_be_bytes());
        assert!(decode_frame(&huge).is_none());
    }
    #[test]
    fn test_header_lookup() {
        let h = "HTTP/1.1 101 Switching Protocols\r\nSEC-WEBSOCKET-ACCEPT: abc==\r\n\r\n";
        assert_eq!(
            header_lookup(h, "Sec-WebSocket-Accept"),
            Some("abc==".to_string())
        );
        assert_eq!(header_lookup(h, "missing"), None);
    }

    /// Server->client WS frame (unmasked; RFC 6455 forbids server masking).
    fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(10 + payload.len());
        f.push(0x80 | (opcode & 0x0F));
        if payload.len() < 126 {
            f.push(payload.len() as u8);
        } else {
            f.push(126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        f.extend_from_slice(payload);
        f
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum AcceptMode {
        /// RFC-correct accept for whatever key the client sent.
        Correct,
        /// Always answer with the RFC 6455 test vector (hard-coded stub).
        Static,
        /// 101 with a bogus accept value.
        Wrong,
        /// 200 OK instead of 101 Switching Protocols.
        No101,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum BroadcastMode {
        /// Fan out to subscribers of the published channel only.
        Matching,
        /// Fan out to every subscribed connection (isolation violation).
        AllChannels,
        /// Fan out a corrupted payload.
        Tamper,
        /// Send subscribers a close frame instead of the message.
        CloseSubscribers,
        /// Never fan out anything.
        Silent,
    }

    /// Knobs for the mock pub/sub server; the defaults model a faithful one.
    #[derive(Clone, Debug)]
    struct MockCfg {
        accept: AcceptMode,
        broadcast: BroadcastMode,
        pong: bool,
        /// Send an unrelated text frame before the pong reply.
        ping_noise: bool,
        close_on_ping: bool,
        ack_sub: bool,
        close_after_upgrade: bool,
        drop_sub_at: Option<usize>,
        delay_first_pub: Duration,
        greet: Option<Vec<u8>>,
    }

    impl Default for MockCfg {
        fn default() -> Self {
            Self {
                accept: AcceptMode::Correct,
                broadcast: BroadcastMode::Matching,
                pong: true,
                ping_noise: false,
                close_on_ping: false,
                ack_sub: false,
                close_after_upgrade: false,
                drop_sub_at: None,
                delay_first_pub: Duration::ZERO,
                greet: None,
            }
        }
    }

    type Subscribers = Arc<Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Vec<u8>>>>>>;

    /// Spawns a mock chat server on its own loopback port: any GET upgrades
    /// (per `accept`), then masked `SUB <chan>` / `PUB <chan> <msg>` text
    /// frames are pumped with the configured fan-out and ping->pong.
    async fn spawn_ws_mock(cfg: MockCfg) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let subs: Subscribers = Arc::new(Mutex::new(HashMap::new()));
        let sub_seq = Arc::new(AtomicUsize::new(0));
        let delay_left = Arc::new(AtomicUsize::new(if cfg.delay_first_pub.is_zero() {
            0
        } else {
            1
        }));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let cfg = cfg.clone();
                let subs = subs.clone();
                let sub_seq = sub_seq.clone();
                let delay_left = delay_left.clone();
                tokio::spawn(async move {
                    let _ = serve_ws(stream, cfg, subs, sub_seq, delay_left).await;
                });
            }
        });
        port
    }

    /// HTTP upgrade plus frame pump for one mock connection.
    async fn serve_ws(
        mut stream: TcpStream,
        cfg: MockCfg,
        subs: Subscribers,
        sub_seq: Arc<AtomicUsize>,
        delay_left: Arc<AtomicUsize>,
    ) -> std::io::Result<()> {
        let mut req: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            req.extend_from_slice(&tmp[..n]);
            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let req = String::from_utf8_lossy(&req).to_string();
        let key = header_lookup(&req, "Sec-WebSocket-Key").unwrap_or_default();
        let resp = match cfg.accept {
            AcceptMode::Correct => format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                ws_accept_key(&key)
            ),
            AcceptMode::Static => format!(
                "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: {TEST_ACCEPT}\r\n\r\n"
            ),
            AcceptMode::Wrong => {
                "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: nope\r\n\r\n".to_string()
            }
            AcceptMode::No101 => "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_string(),
        };
        let mut first_write = resp.into_bytes();
        if let Some(g) = &cfg.greet {
            first_write.extend_from_slice(g);
        }
        stream.write_all(&first_write).await?;
        if cfg.close_after_upgrade || cfg.accept == AcceptMode::No101 {
            return Ok(());
        }

        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if wr.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        let mut buf: Vec<u8> = Vec::new();
        loop {
            while let Some((op, payload, used)) = decode_frame(&buf) {
                buf.drain(..used);
                match op {
                    0x1 => {
                        let text = String::from_utf8_lossy(&payload).to_string();
                        if let Some(chan) = text.strip_prefix("SUB ") {
                            let chan = chan.trim().to_string();
                            if cfg.drop_sub_at == Some(sub_seq.fetch_add(1, Ordering::SeqCst)) {
                                return Ok(());
                            }
                            subs.lock()
                                .unwrap()
                                .entry(chan.clone())
                                .or_default()
                                .push(tx.clone());
                            if cfg.ack_sub {
                                let _ =
                                    tx.send(server_frame(0x1, format!("OK SUB {chan}").as_bytes()));
                            }
                        } else if let Some(rest) = text.strip_prefix("PUB ") {
                            if let Some((chan, msg)) = rest.split_once(' ') {
                                if delay_left.fetch_sub(1, Ordering::SeqCst) == 1 {
                                    sleep(cfg.delay_first_pub).await;
                                }
                                match cfg.broadcast {
                                    BroadcastMode::Silent => {}
                                    BroadcastMode::AllChannels => {
                                        let frame = format!("PUB {chan} {msg}");
                                        let targets: Vec<_> = subs
                                            .lock()
                                            .unwrap()
                                            .values()
                                            .flatten()
                                            .cloned()
                                            .collect();
                                        for t in targets {
                                            let _ = t.send(server_frame(0x1, frame.as_bytes()));
                                        }
                                    }
                                    mode => {
                                        let frame = match mode {
                                            BroadcastMode::Tamper => server_frame(
                                                0x1,
                                                format!("PUB {chan} {msg}-tampered").as_bytes(),
                                            ),
                                            BroadcastMode::CloseSubscribers => {
                                                server_frame(0x8, b"")
                                            }
                                            _ => server_frame(
                                                0x1,
                                                format!("PUB {chan} {msg}").as_bytes(),
                                            ),
                                        };
                                        let targets: Vec<_> = subs
                                            .lock()
                                            .unwrap()
                                            .get(chan)
                                            .cloned()
                                            .unwrap_or_default();
                                        for t in targets {
                                            let _ = t.send(frame.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    0x9 if cfg.close_on_ping => {
                        let _ = tx.send(server_frame(0x8, b""));
                    }
                    0x9 if cfg.pong => {
                        if cfg.ping_noise {
                            let _ = tx.send(server_frame(0x1, b"NOTICE pong pending"));
                        }
                        let _ = tx.send(server_frame(0xA, &payload));
                    }
                    0x8 => return Ok(()),
                    _ => {}
                }
            }
            let n = rd.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    #[test]
    fn test_frame_decode_edge_cases() {
        // A 126-length header whose extended length has not arrived yet.
        assert!(decode_frame(&[0x81, 126]).is_none());
        assert!(decode_frame(&[0x81, 126, 0]).is_none());
        // The 64-bit length form is accepted while the value fits 16 bits.
        let mut extended = vec![0x81, 127];
        extended.extend_from_slice(&5u64.to_be_bytes());
        extended.extend_from_slice(b"hello");
        assert_eq!(
            decode_frame(&extended),
            Some((0x1, b"hello".to_vec(), extended.len()))
        );
        // A truncated payload of a 126-length masked frame yields no frame.
        let frame = encode_masked_frame(0x1, &[b'y'; 130], [1, 2, 3, 4]);
        assert!(decode_frame(&frame[..frame.len() - 1]).is_none());
        // Trailing bytes belong to the next frame and are not consumed.
        let mut two = frame.clone();
        two.extend_from_slice(&server_frame(0x1, b"next"));
        let (op, payload, used) = decode_frame(&two).unwrap();
        assert_eq!((op, used), (0x1, frame.len()));
        assert_eq!(payload, vec![b'y'; 130]);
        // A 126-length unmasked server frame decodes through the same path.
        let big = server_frame(0x1, &[b'z'; 130]);
        assert_eq!(big[1], 126);
        assert_eq!(decode_frame(&big), Some((0x1, vec![b'z'; 130], big.len())));
    }

    #[tokio::test]
    async fn test_stage1_handshake_passes_against_faithful_mock() {
        let port = spawn_ws_mock(MockCfg::default()).await;
        let v = PubsubVerifier::new(port, Some("hsmock"));
        let st = v.test_stage1_handshake().await;
        assert!(st.passed, "faithful handshake must pass: {:?}", st.error);
        assert_eq!(st.stage, 1);
        assert!(st.error.is_none());
    }

    #[tokio::test]
    async fn test_stage1_rejects_static_and_wrong_accept() {
        let port = spawn_ws_mock(MockCfg {
            accept: AcceptMode::Static,
            ..Default::default()
        })
        .await;
        let st = PubsubVerifier::new(port, None)
            .test_stage1_handshake()
            .await;
        assert!(!st.passed, "a hard-coded vector accept must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("Static Accept"),
            "error must call out the static accept: {:?}",
            st.error
        );

        let port = spawn_ws_mock(MockCfg {
            accept: AcceptMode::Wrong,
            ..Default::default()
        })
        .await;
        let st = PubsubVerifier::new(port, None)
            .test_stage1_handshake()
            .await;
        assert!(!st.passed, "a bogus accept must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("Bad Accept"),
            "error must name the accept check: {:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_stage1_rejects_non_101() {
        let port = spawn_ws_mock(MockCfg {
            accept: AcceptMode::No101,
            ..Default::default()
        })
        .await;
        let st = PubsubVerifier::new(port, None)
            .test_stage1_handshake()
            .await;
        assert!(!st.passed, "a 200 OK response must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("Expected 101"),
            "error must name the status check: {:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_stage1_reports_connect_refused() {
        // Reserve then drop a port so nothing listens there.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        let st = PubsubVerifier::new(port, None)
            .test_stage1_handshake()
            .await;
        assert!(!st.passed);
        assert!(
            st.error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to connect"),
            "error must name the failed connect: {:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_stage1_reports_handshake_abort_and_oversize() {
        // The server accepts, reads the request, then closes: the handshake
        // read must surface the EOF instead of hanging.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut tmp = [0u8; 1024];
                let _ = s.read(&mut tmp).await;
            }
        });
        let st = PubsubVerifier::new(port, None)
            .test_stage1_handshake()
            .await;
        assert!(!st.passed, "an aborted handshake must fail");
        assert!(
            st.error
                .as_deref()
                .unwrap_or("")
                .contains("closed connection during handshake"),
            "error must name the EOF: {:?}",
            st.error
        );

        // A header block that never terminates and overflows the 8KiB cap.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut tmp = [0u8; 1024];
                let _ = s.read(&mut tmp).await;
                let _ = s.write_all(&[b'x'; 9000]).await;
                sleep(Duration::from_millis(200)).await;
            }
        });
        let err = match PubsubVerifier::new(port, None).ws_connect(TEST_KEY).await {
            Ok(_) => panic!("an endless header block must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("Handshake response header too large"), "{err}");
    }

    #[tokio::test]
    async fn test_stage1_reports_fresh_key_handshake_failure() {
        // The first handshake succeeds; the server then dies before answering
        // the fresh-key probe, whose failure must be attributed to that step.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut tmp = [0u8; 1024];
            if let Ok((mut s, _)) = listener.accept().await {
                let mut req = Vec::new();
                loop {
                    let n = s.read(&mut tmp).await.unwrap();
                    req.extend_from_slice(&tmp[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&req).to_string();
                let key = header_lookup(&req, "Sec-WebSocket-Key").unwrap_or_default();
                let resp = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                    ws_accept_key(&key)
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
            // Second connection: read the request, then vanish mid-handshake.
            if let Ok((mut s, _)) = listener.accept().await {
                let _ = s.read(&mut tmp).await;
            }
        });
        let st = PubsubVerifier::new(port, None)
            .test_stage1_handshake()
            .await;
        assert!(!st.passed, "a dead fresh-key probe must fail");
        assert!(
            st.error
                .as_deref()
                .unwrap_or("")
                .contains("Handshake (fresh key) failed"),
            "error must blame the fresh-key step: {:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_ws_connect_rejects_bad_upgrade() {
        let port = spawn_ws_mock(MockCfg {
            accept: AcceptMode::Wrong,
            ..Default::default()
        })
        .await;
        let err = match PubsubVerifier::new(port, None).ws_connect(TEST_KEY).await {
            Ok(_) => panic!("a bogus accept must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("Bad Sec-WebSocket-Accept"), "{err}");

        let port = spawn_ws_mock(MockCfg {
            accept: AcceptMode::No101,
            ..Default::default()
        })
        .await;
        let err = match PubsubVerifier::new(port, None).ws_connect(TEST_KEY).await {
            Ok(_) => panic!("a 200 OK response must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("Expected 101 Switching Protocols"), "{err}");
    }

    #[tokio::test]
    async fn test_ws_connect_keeps_leftover_frame_bytes() {
        // Coalesce the 101 header and a text frame in one server write: the
        // surplus bytes must survive inside the connection read buffer.
        let greet = server_frame(0x1, b"PUB chan_x early");
        let port = spawn_ws_mock(MockCfg {
            greet: Some(greet),
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, None);
        let mut c = v.ws_connect(TEST_KEY).await.unwrap();
        let (op, payload) = c.read_frame(Duration::from_millis(500)).await.unwrap();
        assert_eq!((op, payload.as_slice()), (0x1, &b"PUB chan_x early"[..]));
    }

    #[tokio::test]
    async fn test_read_frame_times_out_on_silent_server() {
        let port = spawn_ws_mock(MockCfg::default()).await;
        let mut c = PubsubVerifier::new(port, None)
            .ws_connect(TEST_KEY)
            .await
            .unwrap();
        let err = c
            .read_frame(Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn test_read_frame_reports_server_close() {
        let port = spawn_ws_mock(MockCfg {
            close_after_upgrade: true,
            ..Default::default()
        })
        .await;
        let mut c = PubsubVerifier::new(port, None)
            .ws_connect(TEST_KEY)
            .await
            .unwrap();
        let err = c
            .read_frame(Duration::from_millis(500))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Server closed WS connection"), "{err}");
    }

    #[tokio::test]
    async fn test_read_frame_reassembles_split_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut tmp = [0u8; 1024];
            let mut req = Vec::new();
            loop {
                let n = s.read(&mut tmp).await.unwrap();
                req.extend_from_slice(&tmp[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&req).to_string();
            let key = header_lookup(&req, "Sec-WebSocket-Key").unwrap_or_default();
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                ws_accept_key(&key)
            );
            s.write_all(resp.as_bytes()).await.unwrap();
            // Deliver one frame in two TCP segments.
            let frame = server_frame(0x1, b"PUB split hello");
            s.write_all(&frame[..3]).await.unwrap();
            s.flush().await.unwrap();
            sleep(Duration::from_millis(40)).await;
            s.write_all(&frame[3..]).await.unwrap();
            sleep(Duration::from_millis(300)).await;
        });
        let mut c = PubsubVerifier::new(port, None)
            .ws_connect(TEST_KEY)
            .await
            .unwrap();
        let (op, payload) = c.read_frame(Duration::from_millis(1000)).await.unwrap();
        assert_eq!((op, payload.as_slice()), (0x1, &b"PUB split hello"[..]));
    }

    #[tokio::test]
    async fn test_read_pub_skips_non_pub_text_and_matches_channel() {
        let greet = server_frame(0x1, b"NOTICE welcome");
        let port = spawn_ws_mock(MockCfg {
            greet: Some(greet),
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, None);
        let mut sub = v.ws_connect(TEST_KEY).await.unwrap();
        sub.send(0x1, b"SUB chan_a").await.unwrap();
        sleep(Duration::from_millis(100)).await;
        let mut publ = v.ws_connect(TEST_KEY).await.unwrap();
        publ.send(0x1, b"PUB chan_a hello").await.unwrap();
        let got = v
            .read_pub(&mut sub, "chan_a", Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(got, "PUB chan_a hello");
    }

    #[tokio::test]
    async fn test_read_pub_times_out_on_missing_message() {
        let port = spawn_ws_mock(MockCfg {
            broadcast: BroadcastMode::Silent,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, None);
        let mut sub = v.ws_connect(TEST_KEY).await.unwrap();
        sub.send(0x1, b"SUB chan_a").await.unwrap();
        sleep(Duration::from_millis(50)).await;
        let mut publ = v.ws_connect(TEST_KEY).await.unwrap();
        publ.send(0x1, b"PUB chan_a hello").await.unwrap();
        let err = v
            .read_pub(&mut sub, "chan_a", Duration::from_millis(100))
            .await
            .unwrap_err()
            .to_string()
            .to_lowercase();
        // The inner read_frame fires its own deadline a hair before the
        // outer "Timed out waiting for PUB" check, so both messages are
        // legitimate; what matters is a bounded timeout, not a hang.
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn test_read_pub_rejects_server_close() {
        let greet = server_frame(0x8, b"");
        let port = spawn_ws_mock(MockCfg {
            greet: Some(greet),
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, None);
        let mut sub = v.ws_connect(TEST_KEY).await.unwrap();
        let err = v
            .read_pub(&mut sub, "chan_a", Duration::from_millis(500))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Server sent close frame"), "{err}");
    }

    #[tokio::test]
    async fn test_stage2_broadcast_passes_with_sub_ack_noise() {
        // `ack_sub` adds "OK SUB ..." text frames the verifier must skip.
        let port = spawn_ws_mock(MockCfg {
            ack_sub: true,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage2_broadcast().await;
        assert!(
            st.passed,
            "both subscribers must get the PUB: {:?}",
            st.error
        );
        assert_eq!(st.stage, 2);
    }

    #[tokio::test]
    async fn test_stage2_broadcast_flags_tampered_payload() {
        let port = spawn_ws_mock(MockCfg {
            broadcast: BroadcastMode::Tamper,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage2_broadcast().await;
        assert!(!st.passed, "a corrupted payload must fail");
        let err = st.error.as_deref().unwrap_or("");
        assert!(
            err.contains("sub1 got"),
            "error must name the subscriber: {err}"
        );
        assert!(
            err.contains("tampered"),
            "error must show the payload: {err}"
        );
    }

    #[tokio::test]
    async fn test_stage2_flags_subscriber_disconnect() {
        let port = spawn_ws_mock(MockCfg {
            broadcast: BroadcastMode::CloseSubscribers,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage2_broadcast().await;
        assert!(!st.passed, "a closed subscriber must be reported as a miss");
        let err = st.error.as_deref().unwrap_or("");
        assert!(err.contains("sub1 missed PUB"), "{err}");
        assert!(err.contains("close frame"), "{err}");
    }

    #[tokio::test]
    async fn test_stage3_isolation_and_pong_pass() {
        let port = spawn_ws_mock(MockCfg::default()).await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage3_isolation_pong().await;
        assert!(st.passed, "isolation + pong must pass: {:?}", st.error);
        assert_eq!(st.stage, 3);
    }

    #[tokio::test]
    async fn test_stage3_flags_isolation_violation() {
        let port = spawn_ws_mock(MockCfg {
            broadcast: BroadcastMode::AllChannels,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage3_isolation_pong().await;
        assert!(!st.passed, "cross-channel delivery must fail");
        let err = st.error.as_deref().unwrap_or("");
        assert!(err.contains("Isolation violated"), "{err}");
        assert!(err.contains("other_mockrun"), "{err}");
    }

    #[tokio::test]
    async fn test_stage3_flags_missing_pong() {
        let port = spawn_ws_mock(MockCfg {
            pong: false,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage3_isolation_pong().await;
        assert!(!st.passed, "silence instead of a pong must fail");
        let err = st.error.as_deref().unwrap_or("");
        assert!(err.contains("pong wait"), "{err}");
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn test_stage3_skips_noise_before_pong() {
        let port = spawn_ws_mock(MockCfg {
            ping_noise: true,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage3_isolation_pong().await;
        assert!(
            st.passed,
            "noise before the pong must be skipped: {:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_stage3_flags_close_on_ping() {
        let port = spawn_ws_mock(MockCfg {
            close_on_ping: true,
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let st = v.test_stage3_isolation_pong().await;
        assert!(!st.passed, "closing on ping must fail");
        assert!(
            st.error
                .as_deref()
                .unwrap_or("")
                .contains("Server closed connection on ping"),
            "{:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_stage4_soak_passes_against_faithful_mock() {
        let port = spawn_ws_mock(MockCfg::default()).await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let (st, p99, mps) = v.test_stage4_soak().await;
        assert!(st.passed, "200x20 fan-out must pass: {:?}", st.error);
        let p99 = p99.expect("a passing soak must report p99");
        assert!(
            p99 < 100.0,
            "loopback p99 {p99}ms must be inside the budget"
        );
        assert!(
            mps.unwrap_or(0.0) > 0.0,
            "a passing soak must report a positive rate"
        );
    }

    #[tokio::test]
    async fn test_stage4_soak_flags_missing_deliveries() {
        // One subscriber (the 200th) is dropped before it can subscribe:
        // 199 x 20 = 3980 deliveries, not 4000.
        let port = spawn_ws_mock(MockCfg {
            drop_sub_at: Some(SOAK_SUBS - 1),
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let (st, _, _) = v.test_stage4_soak().await;
        assert!(!st.passed, "missing deliveries must fail");
        let expected = SOAK_SUBS * SOAK_MSGS;
        let got = (SOAK_SUBS - 1) * SOAK_MSGS;
        assert!(
            st.error
                .as_deref()
                .unwrap_or("")
                .contains(&format!("Delivery {got}/{expected}")),
            "{:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_stage4_soak_flags_p99_over_budget() {
        // The first broadcast is delayed past the 100ms budget; every later
        // PUB queues behind it, so the measured p99 must exceed the budget.
        let port = spawn_ws_mock(MockCfg {
            delay_first_pub: Duration::from_millis(120),
            ..Default::default()
        })
        .await;
        let v = PubsubVerifier::new(port, Some("mockrun"));
        let (st, p99, _) = v.test_stage4_soak().await;
        assert!(!st.passed, "a 120ms stall must break the latency budget");
        assert!(
            p99.unwrap_or(0.0) >= 100.0,
            "p99 {p99:?} must be over budget"
        );
        assert!(
            st.error.as_deref().unwrap_or("").contains("p99"),
            "{:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn test_run_all_reports_total_failure_on_dead_port() {
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        let summary = PubsubVerifier::new(port, None).run_all().await;
        assert_eq!(summary.total_stages, 4);
        assert_eq!(summary.passed_count, 0);
        assert_eq!(summary.pass_rate, 0.0);
        assert_eq!(summary.soak_p99_ms, None);
        assert_eq!(summary.soak_msgs_per_sec, None);
        assert!(summary.stages.iter().all(|s| !s.passed));
    }

    #[tokio::test]
    async fn test_run_all_passes_against_faithful_mock() {
        let port = spawn_ws_mock(MockCfg::default()).await;
        let summary = PubsubVerifier::new(port, Some("mockrun")).run_all().await;
        assert_eq!(summary.passed_count, 4, "stages: {:?}", summary.stages);
        assert_eq!(summary.total_stages, 4);
        assert_eq!(summary.pass_rate, 100.0);
        assert!(summary.soak_p99_ms.is_some());
        assert!(summary.soak_msgs_per_sec.unwrap_or(0.0) > 0.0);
    }
}
