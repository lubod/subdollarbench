//! Custom binary RPC ("SDB1") benchmark verifier.
//!
//! Wire protocol (TCP, big-endian, byte-exact):
//! - 16-byte header: magic `SDB1`, u16 seq, u8 opcode, u8 status,
//!   u32 payload len N, u32 IEEE CRC32 of payload.
//! - Requests send status 0x00 (reserved); only responses carry status.
//! - Opcodes: PING(01) ECHO(02) PUT(03) GET(04) STREAM(05).
//! - Wrong magic -> silent close; CRC mismatch -> ERR_BAD_CHECKSUM (open);
//!   unknown opcode / missing key -> ERR_UNKNOWN_OP (open).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::StageResult;

pub const MAGIC: &[u8; 4] = b"SDB1";
pub const OP_PING: u8 = 0x01;
pub const OP_ECHO: u8 = 0x02;
pub const OP_PUT: u8 = 0x03;
pub const OP_GET: u8 = 0x04;
pub const OP_STREAM: u8 = 0x05;
/// SDB1 v2 admin extension: STATS mirrors the INFO counters as a UTF-8
/// `field:value`-per-line payload.
pub const OP_STATS: u8 = 0x06;
pub const ST_OK: u8 = 0x00;
pub const ST_BAD_CHECKSUM: u8 = 0x01;
pub const ST_UNKNOWN_OP: u8 = 0x02;

pub const MAX_PAYLOAD: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const PIPELINE_FRAMES: usize = 50;
const PIPELINE_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinprotoTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct BinprotoVerifier {
    pub target_port: u16,
    pub run_seed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub seq: u16,
    pub opcode: u8,
    pub status: u8,
    pub payload: Vec<u8>,
}

/// CRC32/IEEE of payload bytes (empty -> 0).
pub fn crc_of(payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(payload);
    h.finalize()
}

/// Encode one request frame (status byte always 0x00 reserved).
pub fn encode_frame(seq: u16, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&seq.to_be_bytes());
    out.push(opcode);
    out.push(0x00);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc_of(payload).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode one frame from the head of `buf`; `None` when incomplete.
/// Errors on bad magic / oversize (caller decides close vs fail).
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
    if buf.len() < 16 {
        return Ok(None);
    }
    if &buf[0..4] != MAGIC {
        return Err(anyhow!("bad magic {:02x?}", &buf[0..4.min(buf.len())]));
    }
    let seq = u16::from_be_bytes([buf[4], buf[5]]);
    let opcode = buf[6];
    let status = buf[7];
    let len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(anyhow!("payload length {len} exceeds 1 MiB cap"));
    }
    if buf.len() < 16 + len {
        return Ok(None);
    }
    let crc_want = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let payload = buf[16..16 + len].to_vec();
    if crc_of(&payload) != crc_want {
        return Err(anyhow!(
            "frame crc mismatch: want {crc_want:08x}, got {:08x}",
            crc_of(&payload)
        ));
    }
    Ok(Some((
        Frame {
            seq,
            opcode,
            status,
            payload,
        },
        16 + len,
    )))
}

struct Conn {
    stream: TcpStream,
    read_buf: Vec<u8>,
}

impl Conn {
    async fn connect(port: u16) -> Result<Self> {
        let addr = format!("127.0.0.1:{port}");
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("Connection timed out to {addr}"))?
            .map_err(|e| anyhow!("Failed to connect to {addr}: {e}"))?;
        Ok(Self {
            stream,
            read_buf: Vec::new(),
        })
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream
            .write_all(bytes)
            .await
            .map_err(|e| anyhow!("send failed: {e}"))?;
        Ok(())
    }

    /// Read frames until `want` responses arrive or the deadline hits.
    /// Responses may arrive in any order; matched by sequence upstream.
    async fn read_frames(&mut self, want: usize, deadline: Duration) -> Result<Vec<Frame>> {
        let start = Instant::now();
        let mut out = Vec::new();
        // Heap-allocated: a 64KiB stack array inside this future multiplies
        // through every nested await and overflows 2MiB test-thread stacks
        // in debug builds (the admin stage's await nesting showed this).
        let mut tmp = vec![0u8; 65536];
        while out.len() < want {
            if start.elapsed() > deadline {
                return Err(anyhow!(
                    "deadline: got {} of {want} frames in {:?}",
                    out.len(),
                    start.elapsed()
                ));
            }
            // Drain buffered frames first.
            while let Some((f, used)) = decode_frame(&self.read_buf)? {
                self.read_buf.drain(..used);
                out.push(f);
                if out.len() >= want {
                    return Ok(out);
                }
            }
            let remain = deadline.saturating_sub(start.elapsed());
            let n = tokio::time::timeout(remain.min(READ_TIMEOUT), self.stream.read(&mut tmp))
                .await
                .map_err(|_| anyhow!("read timed out waiting for frame"))?
                .map_err(|e| anyhow!("read failed: {e}"))?;
            if n == 0 {
                return Err(anyhow!(
                    "server closed connection (have {} of {want})",
                    out.len()
                ));
            }
            self.read_buf.extend_from_slice(&tmp[..n]);
        }
        Ok(out)
    }

    async fn roundtrip(&mut self, seq: u16, opcode: u8, payload: &[u8]) -> Result<Frame> {
        self.send(&encode_frame(seq, opcode, payload)).await?;
        let frames = self.read_frames(1, READ_TIMEOUT).await?;
        Ok(frames.into_iter().next().unwrap())
    }
}

fn corrupt_crc(frame: &[u8]) -> Vec<u8> {
    let mut bad = frame.to_vec();
    // Flip a payload byte when present, else the CRC word itself.
    if bad.len() > 16 {
        let i = 16 + (bad.len() - 17) / 2;
        bad[i] ^= 0xFF;
    } else {
        bad[15] ^= 0xFF;
    }
    bad
}

/// Minimal RESP client used by the admin stage's cross-protocol checks.
struct RespMini {
    stream: TcpStream,
}

impl RespMini {
    async fn connect(port: u16) -> Result<Self> {
        let addr = format!("127.0.0.1:{port}");
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("Connection timed out to {addr}"))?
            .map_err(|e| anyhow!("Failed to connect to {addr}: {e}"))?;
        Ok(Self { stream })
    }

    /// Send one RESP command; return the first complete reply frame as text.
    async fn cmd(&mut self, args: &[&str]) -> Result<String> {
        let mut out = format!("*{}\r\n", args.len());
        for a in args {
            out.push_str(&format!("${}\r\n{}\r\n", a.len(), a));
        }
        self.stream
            .write_all(out.as_bytes())
            .await
            .map_err(|e| anyhow!("resp send: {e}"))?;
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(need) = resp_frame_len(&buf)? {
                if buf.len() >= need {
                    return Ok(String::from_utf8_lossy(&buf[..need]).to_string());
                }
            }
            let n = tokio::time::timeout(READ_TIMEOUT, self.stream.read(&mut tmp))
                .await
                .map_err(|_| anyhow!("resp read timed out"))?
                .map_err(|e| anyhow!("resp read: {e}"))?;
            if n == 0 {
                return Err(anyhow!("resp connection closed mid-reply"));
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }
}

/// Byte length of the first complete RESP reply in `buf`, if known.
fn resp_frame_len(buf: &[u8]) -> Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }
    let head_end = match buf.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p,
        None => return Ok(None),
    };
    let total = match buf[0] {
        b'+' | b'-' | b':' => head_end + 2,
        b'$' => {
            let hd =
                std::str::from_utf8(&buf[1..head_end]).map_err(|_| anyhow!("bad bulk header"))?;
            let len: i64 = hd.parse().map_err(|_| anyhow!("bad bulk length {hd:?}"))?;
            if len < 0 {
                head_end + 2
            } else {
                head_end + 2 + len as usize + 2
            }
        }
        other => return Err(anyhow!("unexpected RESP type byte {other:02x}")),
    };
    Ok(Some(total))
}

/// Campaign stage 16 (admin binary protocol, SDB1 v2; explicit acceptance
/// only): the admin channel is served beside RESP on its own port. Checks
/// framing fidelity (bad magic closes, PING/ECHO byte fidelity, corrupt
/// CRC -> ERR_BAD_CHECKSUM with the connection kept open, unknown opcode
/// -> ERR_UNKNOWN_OP, 50-frame pipeline with matched sequence ids) plus
/// the v2 additions: the SAME keyspace as RESP in both directions
/// (SDB1 PUT -> RESP GET, RESP SET -> SDB1 GET) and STATS mirroring the
/// INFO counter fields in a `field:value` payload.
pub async fn admin_stage(admin_port: u16, resp_port: u16, seed: Option<&str>) -> StageResult {
    let name = "Stage 16: admin binary protocol (SDB1 v2)".to_string();
    let fail = |e: String| StageResult {
        stage: 16,
        name: name.clone(),
        passed: false,
        error: Some(e),
    };
    let tag = super::get_seed_prefix(seed, "adm").to_string();
    match admin_stage_inner(admin_port, resp_port, &tag).await {
        Ok(()) => StageResult {
            stage: 16,
            name,
            passed: true,
            error: None,
        },
        Err(e) => fail(e.to_string()),
    }
}

/// Sectioned body of the admin stage; split so each async state machine
/// stays small (a single ~9-step async fn overflowed 2MB test-thread
/// stacks in debug builds).
async fn admin_stage_inner(admin_port: u16, resp_port: u16, tag: &str) -> Result<()> {
    admin_framing(admin_port).await?;
    admin_shared_keyspace(admin_port, resp_port, tag).await?;
    admin_stats(admin_port).await?;
    admin_pipeline(admin_port).await
}

async fn admin_framing(admin_port: u16) -> Result<()> {
    // 1. Wrong magic must close silently (service survives).
    let mut c = Conn::connect(admin_port).await?;
    let mut bad = encode_frame(1, OP_PING, b"");
    bad[0] = b'X';
    c.send(&bad).await?;
    if c.read_frames(1, Duration::from_secs(2)).await.is_ok() {
        return Err(anyhow!(
            "a bad-magic frame was answered; contract says close"
        ));
    }

    // 2-3. PING / ECHO fidelity (incl. binary bytes and 64 KiB).
    let mut c = Conn::connect(admin_port).await?;
    let r = c.roundtrip(7, OP_PING, b"").await?;
    if r.opcode != OP_PING || r.status != ST_OK || r.seq != 7 || !r.payload.is_empty() {
        return Err(anyhow!("PING reply wrong: {r:?}"));
    }
    let small: &[u8] = b"hello-\x00\xff-binary";
    let r = c.roundtrip(8, OP_ECHO, small).await?;
    if r.opcode != OP_ECHO || r.status != ST_OK || r.seq != 8 || r.payload != small {
        return Err(anyhow!("ECHO small payload mismatch"));
    }
    let big = vec![0xA5u8; 64 * 1024];
    let r = c.roundtrip(9, OP_ECHO, &big).await?;
    if r.status != ST_OK || r.payload != big {
        return Err(anyhow!("ECHO 64 KiB payload mismatch"));
    }

    // 4. Corrupt CRC -> ERR_BAD_CHECKSUM, connection stays open.
    let bad = corrupt_crc(&encode_frame(10, OP_ECHO, b"x"));
    c.send(&bad).await?;
    let frames = c.read_frames(1, READ_TIMEOUT).await?;
    if frames[0].status != ST_BAD_CHECKSUM || frames[0].seq != 10 {
        return Err(anyhow!("corrupt CRC reply wrong: {:?}", frames[0]));
    }
    let r = c.roundtrip(11, OP_PING, b"").await?;
    if r.status != ST_OK {
        return Err(anyhow!("connection not usable after a checksum error"));
    }

    // 5. Unknown opcode -> ERR_UNKNOWN_OP.
    let r = c.roundtrip(12, 0x7F, b"").await?;
    if r.status != ST_UNKNOWN_OP {
        return Err(anyhow!("unknown opcode reply wrong: {:?}", r));
    }
    Ok(())
}

async fn admin_shared_keyspace(admin_port: u16, resp_port: u16, tag: &str) -> Result<()> {
    let mut c = Conn::connect(admin_port).await?;
    // 6. PUT/GET over SDB1 (shared keyspace, binary-safe values).
    let k = format!("adm_{tag}_k1");
    let v: &[u8] = b"adm-value-\x01\x02\xfe";
    let mut put = k.as_bytes().to_vec();
    put.push(0x00);
    put.extend_from_slice(v);
    let r = c.roundtrip(13, OP_PUT, &put).await?;
    if r.opcode != OP_PUT || r.status != ST_OK || !r.payload.is_empty() {
        return Err(anyhow!("SDB1 PUT reply wrong: {r:?}"));
    }
    let r = c.roundtrip(14, OP_GET, k.as_bytes()).await?;
    if r.status != ST_OK || r.payload != v {
        return Err(anyhow!("SDB1 GET mismatch (status {:02x})", r.status));
    }
    let r = c
        .roundtrip(15, OP_GET, format!("adm_{tag}_absent").as_bytes())
        .await?;
    if r.status != ST_UNKNOWN_OP {
        return Err(anyhow!("SDB1 GET missing key reply wrong: {r:?}"));
    }

    // 7. Cross-protocol coherence: one store behind both protocols.
    let mut resp = RespMini::connect(resp_port).await?;
    let k2 = format!("adm_{tag}_k2");
    let mut put2 = k2.as_bytes().to_vec();
    put2.push(0x00);
    put2.extend_from_slice(b"xval");
    let r = c.roundtrip(16, OP_PUT, &put2).await?;
    if r.status != ST_OK {
        return Err(anyhow!("SDB1 PUT (cross) failed: {r:?}"));
    }
    let got = resp.cmd(&["GET", &k2]).await?;
    if !got.contains("xval") {
        return Err(anyhow!(
            "RESP GET of an SDB1-written key returned {got:?} (protocols share one store?)"
        ));
    }
    let k3 = format!("adm_{tag}_k3");
    let set = resp.cmd(&["SET", &k3, "rval"]).await?;
    if !set.starts_with("+OK") {
        return Err(anyhow!("RESP SET failed: {set:?}"));
    }
    let r = c.roundtrip(17, OP_GET, k3.as_bytes()).await?;
    if r.status != ST_OK || r.payload != b"rval" {
        return Err(anyhow!(
            "SDB1 GET of an RESP-written key mismatch (status {:02x})",
            r.status
        ));
    }
    Ok(())
}

async fn admin_stats(admin_port: u16) -> Result<()> {
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
    // 8. STATS mirrors the INFO counters.
    let mut c = Conn::connect(admin_port).await?;
    let r = c.roundtrip(18, OP_STATS, b"").await?;
    if r.opcode != OP_STATS || r.status != ST_OK {
        return Err(anyhow!("STATS reply wrong: {r:?}"));
    }
    let text = String::from_utf8_lossy(&r.payload).to_lowercase();
    for f in REQUIRED {
        if !text.contains(&format!("{f}:")) {
            return Err(anyhow!(
                "STATS payload is missing '{f}' (mirror the INFO counters)"
            ));
        }
    }
    Ok(())
}

async fn admin_pipeline(admin_port: u16) -> Result<()> {
    // 9. 50-frame pipeline with matched sequence ids and payloads.
    let mut c = Conn::connect(admin_port).await?;
    let mut burst = Vec::new();
    for i in 0..50u16 {
        let payload = format!("pipe{i}");
        burst.extend_from_slice(&encode_frame(100 + i, OP_ECHO, payload.as_bytes()));
    }
    c.send(&burst).await?;
    let frames = c.read_frames(50, PIPELINE_DEADLINE).await?;
    let mut seqs = std::collections::HashSet::new();
    for f in &frames {
        if f.opcode != OP_ECHO || f.status != ST_OK {
            return Err(anyhow!("pipelined reply wrong: {:?}", f));
        }
        let want = format!("pipe{}", f.seq.saturating_sub(100));
        if f.payload != want.as_bytes() {
            return Err(anyhow!("pipeline frame {} corrupt: {:?}", f.seq, f));
        }
        seqs.insert(f.seq);
    }
    if seqs.len() != 50 {
        return Err(anyhow!(
            "pipelined replies not matched: {} unique seq ids of 50",
            seqs.len()
        ));
    }
    Ok(())
}

impl BinprotoVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        Self {
            target_port,
            run_seed: seed.map(|s| s.to_string()),
        }
    }

    fn tag<'a>(&'a self, default_val: &'a str) -> &'a str {
        super::get_seed_prefix(self.run_seed.as_deref(), default_val)
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

    /// All 5 stages: handshake, ping/echo, integrity, KV, pipeline.
    /// Throughput probe: 200 pipelined ECHO frames, ops/sec.
    /// Same shape as stage5 (validates framing under burst); unscored
    /// until a reference harness exists (throughput_score stays None).
    pub async fn measure_throughput(&self) -> Option<f64> {
        use std::time::Instant;
        let mut c = Conn::connect(self.target_port).await.ok()?;
        let mut burst = Vec::new();
        for i in 0..200u16 {
            let payload = format!("tput{i:03}");
            burst.extend_from_slice(&encode_frame(2000 + i, OP_ECHO, payload.as_bytes()));
        }
        let start = Instant::now();
        c.send(&burst).await.ok()?;
        let frames = c.read_frames(200, PIPELINE_DEADLINE).await.ok()?;
        let elapsed = start.elapsed().as_secs_f64();
        if frames.len() != 200 || elapsed <= 0.0 {
            return None;
        }
        Some(200.0 / elapsed)
    }

    pub async fn run_all(&self) -> BinprotoTestSummary {
        let stages = vec![
            self.stage1_handshake().await,
            self.stage2_roundtrip().await,
            self.stage3_integrity().await,
            self.stage4_kv().await,
            self.stage5_pipeline().await,
        ];
        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;
        BinprotoTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    async fn stage1_handshake(&self) -> StageResult {
        let name = "Stage 1: SDB1 handshake, bad magic closes".to_string();
        match self.stage1_inner().await {
            Ok(()) => Self::pass(1, name),
            Err(e) => Self::fail(1, name, e),
        }
    }

    async fn stage1_inner(&self) -> Result<()> {
        // Valid PING proves the handshake; then garbage must close.
        let mut c = Conn::connect(self.target_port).await?;
        let r = c.roundtrip(1, OP_PING, &[]).await?;
        if r.opcode != OP_PING || r.status != ST_OK || r.seq != 1 || !r.payload.is_empty() {
            return Err(anyhow!("PING handshake wrong: {r:?}"));
        }
        // A full 16-byte header with corrupt magic: servers that frame on
        // 16-byte headers reject it deterministically; buffered readers
        // that wait for more bytes stay silent. Either close or silence
        // within the window passes — what fails is answering garbage.
        let mut c2 = Conn::connect(self.target_port).await?;
        let mut garbage = vec![b'X', b'Y', b'Z', b'!'];
        garbage.extend_from_slice(&[0u8; 12]);
        c2.send(&garbage).await?;
        let mut tmp = [0u8; 64];
        match tokio::time::timeout(READ_TIMEOUT, c2.stream.read(&mut tmp)).await {
            Ok(Ok(0)) => Ok(()),
            Ok(Ok(n)) => Err(anyhow!(
                "bad magic drew {n} response bytes, want close/silence"
            )),
            Ok(Err(e)) => Err(anyhow!("read after bad magic failed: {e}")),
            // Silence: no bytes within 3 s. The peer stays half-open but
            // answered nothing, which is the observable requirement.
            Err(_) => Ok(()),
        }
    }

    async fn stage2_roundtrip(&self) -> StageResult {
        let name = "Stage 2: PING/ECHO fidelity incl 64 KiB".to_string();
        match self.stage2_inner().await {
            Ok(()) => Self::pass(2, name),
            Err(e) => Self::fail(2, name, e),
        }
    }

    async fn stage2_inner(&self) -> Result<()> {
        let mut c = Conn::connect(self.target_port).await?;
        let r = c.roundtrip(10, OP_ECHO, &[]).await?;
        if r.opcode != OP_ECHO || r.status != ST_OK || r.seq != 10 || !r.payload.is_empty() {
            return Err(anyhow!("empty ECHO wrong: {r:?}"));
        }
        let big: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
        let r = c.roundtrip(11, OP_ECHO, &big).await?;
        if r.opcode != OP_ECHO || r.status != ST_OK || r.seq != 11 || r.payload != big {
            return Err(anyhow!(
                "64 KiB ECHO mismatch (got {} bytes, status {:02x})",
                r.payload.len(),
                r.status
            ));
        }
        Ok(())
    }

    async fn stage3_integrity(&self) -> StageResult {
        let name = "Stage 3: corrupt CRC -> ERR_BAD_CHECKSUM, stays open".to_string();
        match self.stage3_inner().await {
            Ok(()) => Self::pass(3, name),
            Err(e) => Self::fail(3, name, e),
        }
    }

    async fn stage3_inner(&self) -> Result<()> {
        let mut c = Conn::connect(self.target_port).await?;
        let bad = corrupt_crc(&encode_frame(20, OP_ECHO, b"hello"));
        c.send(&bad).await?;
        let frames = c.read_frames(1, READ_TIMEOUT).await?;
        let r = &frames[0];
        if r.opcode != OP_ECHO || r.status != ST_BAD_CHECKSUM || r.seq != 20 {
            return Err(anyhow!("CRC-corrupt reply wrong: {r:?}"));
        }
        // Connection must survive: a clean PING right after.
        let r = c.roundtrip(21, OP_PING, &[]).await?;
        if r.status != ST_OK || r.seq != 21 {
            return Err(anyhow!("connection died after CRC error: {r:?}"));
        }
        // Unknown opcode likewise stays open.
        let r = c.roundtrip(22, 0x7F, b"x").await?;
        if r.status != ST_UNKNOWN_OP || r.seq != 22 {
            return Err(anyhow!("unknown-op reply wrong: {r:?}"));
        }
        Ok(())
    }

    async fn stage4_kv(&self) -> StageResult {
        let name = "Stage 4: binary PUT/GET, missing-key error".to_string();
        match self.stage4_inner().await {
            Ok(()) => Self::pass(4, name),
            Err(e) => Self::fail(4, name, e),
        }
    }

    async fn stage4_inner(&self) -> Result<()> {
        let tag = self.tag("bench");
        let key = format!("binproto_{tag}_k1");
        let mut value = vec![0x00, 0xFF, 0x01, 0x00];
        value.extend_from_slice(format!("bin_{tag}").as_bytes());
        let mut put = key.as_bytes().to_vec();
        put.push(0x00);
        put.extend_from_slice(&value);
        let mut c = Conn::connect(self.target_port).await?;
        let r = c.roundtrip(30, OP_PUT, &put).await?;
        if r.opcode != OP_PUT || r.status != ST_OK || r.seq != 30 || !r.payload.is_empty() {
            return Err(anyhow!("PUT reply wrong: {r:?}"));
        }
        let r = c.roundtrip(31, OP_GET, key.as_bytes()).await?;
        if r.opcode != OP_GET || r.status != ST_OK || r.seq != 31 || r.payload != value {
            return Err(anyhow!(
                "GET mismatch (got {} bytes, status {:02x})",
                r.payload.len(),
                r.status
            ));
        }
        let r = c
            .roundtrip(32, OP_GET, format!("binproto_{tag}_absent").as_bytes())
            .await?;
        if r.opcode != OP_GET || r.status != ST_UNKNOWN_OP || r.seq != 32 {
            return Err(anyhow!("missing-key reply wrong: {r:?}"));
        }
        Ok(())
    }

    async fn stage5_pipeline(&self) -> StageResult {
        let name = "Stage 5: 50-frame pipeline in 3 s, IDs matched".to_string();
        match self.stage5_inner().await {
            Ok(()) => Self::pass(5, name),
            Err(e) => Self::fail(5, name, e),
        }
    }

    async fn stage5_inner(&self) -> Result<()> {
        let mut c = Conn::connect(self.target_port).await?;
        // One writev-style burst: all frames back-to-back on one connection.
        let mut burst = Vec::new();
        for i in 0..PIPELINE_FRAMES as u16 {
            let payload = format!("pipe{i:03}");
            burst.extend_from_slice(&encode_frame(1000 + i, OP_ECHO, payload.as_bytes()));
        }
        let start = Instant::now();
        c.send(&burst).await?;
        let frames = c.read_frames(PIPELINE_FRAMES, PIPELINE_DEADLINE).await?;
        let elapsed = start.elapsed();
        let mut seqs: Vec<u16> = frames.iter().map(|f| f.seq).collect();
        seqs.sort_unstable();
        let want: Vec<u16> = (0..PIPELINE_FRAMES as u16).map(|i| 1000 + i).collect();
        if seqs != want {
            return Err(anyhow!(
                "pipeline IDs unmatched ({}/{} in {:?})",
                frames.len(),
                PIPELINE_FRAMES,
                elapsed
            ));
        }
        for f in &frames {
            let want_payload = format!("pipe{:03}", f.seq - 1000);
            if f.opcode != OP_ECHO || f.status != ST_OK || f.payload != want_payload.as_bytes() {
                return Err(anyhow!("pipeline frame {} corrupt: {f:?}", f.seq));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codec_roundtrip() {
        let payload = b"\x00\xffbinary";
        let bytes = encode_frame(0x1234, OP_ECHO, payload);
        assert_eq!(&bytes[0..4], MAGIC);
        assert_eq!(&bytes[4..6], &[0x12, 0x34]);
        assert_eq!(bytes[6], OP_ECHO);
        assert_eq!(bytes[7], 0x00);
        assert_eq!(&bytes[8..12], &(payload.len() as u32).to_be_bytes());
        let (f, used) = decode_frame(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(f.seq, 0x1234);
        assert_eq!(f.payload, payload);
    }

    #[test]
    fn test_decode_partial_and_glued() {
        let a = encode_frame(1, OP_PING, &[]);
        let b = encode_frame(2, OP_ECHO, b"xy");
        let mut glued = a.clone();
        glued.extend_from_slice(&b);
        // Partial head: incomplete.
        assert!(decode_frame(&a[..10]).unwrap().is_none());
        // Glued: first decodes with exact consumption.
        let (f, used) = decode_frame(&glued).unwrap().unwrap();
        assert_eq!(f.seq, 1);
        assert_eq!(used, a.len());
        // Remainder decodes.
        let (g, used2) = decode_frame(&glued[used..]).unwrap().unwrap();
        assert_eq!(g.seq, 2);
        assert_eq!(used2, b.len());
    }

    #[test]
    fn test_decode_rejects_bad_magic_and_oversize() {
        let mut bad = encode_frame(1, OP_PING, &[]);
        bad[0] = b'X';
        assert!(decode_frame(&bad).is_err());
        let mut hdr = encode_frame(1, OP_PING, &[]);
        hdr[8..12].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        assert!(decode_frame(&hdr).is_err());
    }

    #[test]
    fn test_corrupt_crc_detectable() {
        let good = encode_frame(5, OP_ECHO, b"hello");
        let bad = corrupt_crc(&good);
        assert_ne!(good, bad);
        // Flipped payload byte breaks the stored CRC.
        assert!(decode_frame(&bad).is_err());
    }

    #[test]
    fn test_empty_crc_is_zero() {
        assert_eq!(crc_of(&[]), 0);
        let bytes = encode_frame(9, OP_PING, &[]);
        assert_eq!(&bytes[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn test_corrupt_crc_empty_payload_flips_crc_word() {
        let good = encode_frame(7, OP_PING, &[]);
        let bad = corrupt_crc(&good);
        assert_ne!(good, bad);
        // With no payload byte to flip the CRC word itself is corrupted.
        assert!(decode_frame(&bad).is_err());
    }

    #[test]
    fn test_decode_accepts_payload_at_cap() {
        let payload = vec![0xAB; MAX_PAYLOAD];
        let frame = encode_frame(3, OP_ECHO, &payload);
        let (f, used) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(used, frame.len());
        assert_eq!(f.seq, 3);
        assert_eq!(f.payload, payload);
        // A complete header whose payload is truncated waits for more bytes.
        assert!(decode_frame(&frame[..16 + 3]).unwrap().is_none());
    }

    // ---- In-process mock-server tests: drive the verifier over real TCP ----

    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum BinFail {
        None,
        BadMagicReplies,
        SilentOnGarbage,
        EchoFlipValue,
        EchoWrongSeq,
        EchoBadStatus,
        CrcAccepts,
        CrcCloses,
        ReplyBadCrc,
        UnknownOpOk,
        KvMissing,
        KvMissingOk,
        PutWrongStatus,
        PipelineShiftId,
        PipelineWrongPayload,
        SilenceAll,
        CloseOnAccept,
    }

    /// A response frame: the real encoder with the status byte overwritten.
    fn response(seq: u16, opcode: u8, status: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = encode_frame(seq, opcode, payload);
        f[7] = status;
        f
    }

    async fn spawn_binproto_mock(fail: BinFail) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    serve_binproto_conn(sock, fail).await;
                });
            }
        });
        port
    }

    /// Minimal SDB1 server: stateful PUT/GET, ECHO, CRC checking, and
    /// deterministic close on bad magic. `fail` selects one defect.
    async fn serve_binproto_conn(mut sock: TcpStream, fail: BinFail) {
        if fail == BinFail::CloseOnAccept {
            return;
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut kv: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut tmp = [0u8; 65536];
        loop {
            // Answer every complete frame currently buffered (real pipelining).
            loop {
                if buf.len() < 16 {
                    break;
                }
                if &buf[0..4] != MAGIC {
                    if fail == BinFail::BadMagicReplies {
                        let _ = sock.write_all(b"garbage").await;
                    }
                    if fail == BinFail::SilentOnGarbage {
                        // Stay open and mute, modeling a buffered reader.
                        break;
                    }
                    return; // deterministic rejection: close
                }
                let seq = u16::from_be_bytes([buf[4], buf[5]]);
                let opcode = buf[6];
                let len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
                if len > MAX_PAYLOAD || buf.len() < 16 + len {
                    break;
                }
                let crc_want = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
                let payload = buf[16..16 + len].to_vec();
                buf.drain(..16 + len);
                if fail == BinFail::SilenceAll {
                    continue;
                }
                if crc_of(&payload) != crc_want {
                    match fail {
                        BinFail::CrcAccepts => {
                            let _ = sock.write_all(&response(seq, opcode, ST_OK, &[])).await;
                        }
                        BinFail::CrcCloses => return,
                        _ => {
                            let _ = sock
                                .write_all(&response(seq, opcode, ST_BAD_CHECKSUM, &[]))
                                .await;
                        }
                    }
                    continue;
                }
                let mut reply = match opcode {
                    OP_PING => response(seq, OP_PING, ST_OK, &[]),
                    OP_ECHO if fail == BinFail::EchoFlipValue && !payload.is_empty() => {
                        let mut flipped = payload.clone();
                        flipped[0] ^= 0xFF;
                        response(seq, OP_ECHO, ST_OK, &flipped)
                    }
                    OP_ECHO if fail == BinFail::EchoWrongSeq => {
                        response(seq.wrapping_add(1), OP_ECHO, ST_OK, &payload)
                    }
                    OP_ECHO if fail == BinFail::EchoBadStatus => {
                        response(seq, OP_ECHO, ST_UNKNOWN_OP, &payload)
                    }
                    OP_ECHO if fail == BinFail::PipelineShiftId && seq == 1005 => {
                        // Same frame count but one ID wrong: the delivered
                        // IDs (not a timeout) must fail the pipeline stage.
                        response(1099, OP_ECHO, ST_OK, &payload)
                    }
                    OP_ECHO if fail == BinFail::PipelineWrongPayload && seq >= 1000 => {
                        response(seq, OP_ECHO, ST_OK, b"CORRUPT")
                    }
                    OP_ECHO => response(seq, OP_ECHO, ST_OK, &payload),
                    OP_PUT => {
                        if let Some(i) = payload.iter().position(|b| *b == 0) {
                            kv.insert(payload[..i].to_vec(), payload[i + 1..].to_vec());
                        }
                        if fail == BinFail::PutWrongStatus {
                            response(seq, OP_PUT, ST_UNKNOWN_OP, &[])
                        } else {
                            response(seq, OP_PUT, ST_OK, &[])
                        }
                    }
                    OP_GET => match kv.get(&payload) {
                        Some(v) if fail != BinFail::KvMissing => response(seq, OP_GET, ST_OK, v),
                        Some(_) | None if fail == BinFail::KvMissingOk => {
                            response(seq, OP_GET, ST_OK, &[])
                        }
                        Some(_) | None => response(seq, OP_GET, ST_UNKNOWN_OP, &[]),
                    },
                    _ if fail == BinFail::UnknownOpOk => response(seq, opcode, ST_OK, &[]),
                    _ => response(seq, opcode, ST_UNKNOWN_OP, &[]),
                };
                if fail == BinFail::ReplyBadCrc {
                    // Corrupt the stored CRC word of a valid response.
                    reply[15] ^= 0xFF;
                }
                if sock.write_all(&reply).await.is_err() {
                    return;
                }
            }
            match sock.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    /// Minimal RESP array parser for the admin-stage RESP mock.
    fn parse_resp_array(buf: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
        let line = |buf: &[u8], pos: usize| -> Option<(Vec<u8>, usize)> {
            let end = buf.get(pos..)?.windows(2).position(|w| w == b"\r\n")? + pos;
            Some((buf[pos..end].to_vec(), end + 2))
        };
        let (head, mut pos) = line(buf, 0)?;
        if head.first() != Some(&b'*') {
            return None;
        }
        let n: usize = std::str::from_utf8(&head[1..]).ok()?.parse().ok()?;
        let mut args = Vec::new();
        for _ in 0..n {
            let (h, p) = line(buf, pos)?;
            if h.first() != Some(&b'$') {
                return None;
            }
            let l: usize = std::str::from_utf8(&h[1..]).ok()?.parse().ok()?;
            if buf.len() < p + l + 2 {
                return None;
            }
            args.push(buf[p..p + l].to_vec());
            pos = p + l + 2;
        }
        Some((args, pos))
    }

    /// SDB1 + RESP mocks sharing one byte keyspace (admin stage 16).
    /// `stats_no_fields` drops the counter lines from STATS; `split_stores`
    /// gives the two protocols independent maps (cross-protocol failure).
    async fn spawn_admin_mocks(stats_no_fields: bool, split_stores: bool) -> (u16, u16) {
        use std::sync::{Arc, Mutex};
        type Store = Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>;
        let store: Store = Arc::new(Mutex::new(HashMap::new()));

        let sdb1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin_port = sdb1.local_addr().unwrap().port();
        {
            let store = store.clone();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = sdb1.accept().await {
                    let store = store.clone();
                    tokio::spawn(async move {
                        let mut buf: Vec<u8> = Vec::new();
                        let mut tmp = [0u8; 65536];
                        loop {
                            loop {
                                match decode_frame(&buf) {
                                    Ok(None) => break,
                                    Ok(Some((f, used))) => {
                                        buf.drain(..used);
                                        let reply = match f.opcode {
                                            OP_PING => response(f.seq, f.opcode, ST_OK, &[]),
                                            OP_ECHO => response(f.seq, f.opcode, ST_OK, &f.payload),
                                            OP_PUT => {
                                                if let Some(i) =
                                                    f.payload.iter().position(|b| *b == 0)
                                                {
                                                    store.lock().unwrap().insert(
                                                        f.payload[..i].to_vec(),
                                                        f.payload[i + 1..].to_vec(),
                                                    );
                                                    response(f.seq, f.opcode, ST_OK, &[])
                                                } else {
                                                    response(f.seq, f.opcode, ST_UNKNOWN_OP, &[])
                                                }
                                            }
                                            OP_GET => {
                                                match store.lock().unwrap().get(&f.payload).cloned()
                                                {
                                                    Some(v) => response(f.seq, f.opcode, ST_OK, &v),
                                                    None => response(
                                                        f.seq,
                                                        f.opcode,
                                                        ST_UNKNOWN_OP,
                                                        &[],
                                                    ),
                                                }
                                            }
                                            OP_STATS => {
                                                let body: &str = if stats_no_fields {
                                                    "uptime:1\n"
                                                } else {
                                                    "total_connections_received:1\ntotal_commands_processed:2\nkeyspace_hits:0\nkeyspace_misses:0\nexpired_keys:0\nrejected_connections:0\nconnected_clients:1\nuptime_in_seconds:1\n"
                                                };
                                                response(f.seq, OP_STATS, ST_OK, body.as_bytes())
                                            }
                                            _ => response(f.seq, f.opcode, ST_UNKNOWN_OP, &[]),
                                        };
                                        if sock.write_all(&reply).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(_) => {
                                        if buf.len() >= 16 && &buf[0..4] != MAGIC {
                                            return; // wrong magic closes
                                        }
                                        if buf.len() >= 16 {
                                            let seq = u16::from_be_bytes([buf[4], buf[5]]);
                                            let op = buf[6];
                                            let len = u32::from_be_bytes([
                                                buf[8], buf[9], buf[10], buf[11],
                                            ])
                                                as usize;
                                            let _ = sock
                                                .write_all(&response(seq, op, ST_BAD_CHECKSUM, &[]))
                                                .await;
                                            if len <= MAX_PAYLOAD {
                                                buf.drain(..16 + len);
                                            } else {
                                                buf.clear();
                                            }
                                            continue;
                                        }
                                        break;
                                    }
                                }
                            }
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        }
                    });
                }
            });
        }

        let resp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resp_port = resp.local_addr().unwrap().port();
        let resp_store: Store = if split_stores {
            Arc::new(Mutex::new(HashMap::new()))
        } else {
            store.clone()
        };
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = resp.accept().await {
                let store = resp_store.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        while let Some((args, used)) = parse_resp_array(&buf) {
                            buf.drain(..used);
                            if args.is_empty() {
                                continue;
                            }
                            let cmd = String::from_utf8_lossy(&args[0]).to_uppercase();
                            let reply = match cmd.as_str() {
                                "PING" => b"+PONG\r\n".to_vec(),
                                "SET" if args.len() == 3 => {
                                    store
                                        .lock()
                                        .unwrap()
                                        .insert(args[1].clone(), args[2].clone());
                                    b"+OK\r\n".to_vec()
                                }
                                "GET" if args.len() == 2 => {
                                    match store.lock().unwrap().get(&args[1]) {
                                        Some(v) => {
                                            let mut r = format!("${}\r\n", v.len()).into_bytes();
                                            r.extend_from_slice(v);
                                            r.extend_from_slice(b"\r\n");
                                            r
                                        }
                                        None => b"$-1\r\n".to_vec(),
                                    }
                                }
                                _ => b"-ERR unknown\r\n".to_vec(),
                            };
                            if sock.write_all(&reply).await.is_err() {
                                return;
                            }
                        }
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                });
            }
        });
        (admin_port, resp_port)
    }

    /// Stage 16 must pass against faithful SDB1 v2 + RESP mocks and reject
    /// a STATS payload without the counter fields and split stores.
    #[tokio::test]
    async fn mock_admin_stage_pass_and_failure_modes() {
        let (a, r) = spawn_admin_mocks(false, false).await;
        let st = admin_stage(a, r, Some("adm")).await;
        assert!(st.passed, "faithful SDB1 v2 must pass: {:?}", st.error);

        let (a, r) = spawn_admin_mocks(true, false).await;
        let st = admin_stage(a, r, Some("adm")).await;
        assert!(!st.passed, "STATS without the counter fields must fail");
        assert!(
            st.error.as_deref().unwrap_or("").contains("missing"),
            "error should name the missing field: {:?}",
            st.error
        );

        let (a, r) = spawn_admin_mocks(false, true).await;
        let st = admin_stage(a, r, Some("adm")).await;
        assert!(
            !st.passed,
            "split stores must fail the cross-protocol check"
        );
        assert!(
            st.error
                .as_deref()
                .unwrap_or("")
                .contains("share one store"),
            "error should name the store split: {:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn mock_binproto_run_all_passes_on_faithful_server() {
        let port = spawn_binproto_mock(BinFail::None).await;
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        let summary = v.run_all().await;
        assert_eq!(summary.total_stages, 5);
        assert_eq!(summary.passed_count, 5, "stages: {:?}", summary.stages);
        assert_eq!(summary.pass_rate, 100.0);
        assert_eq!(
            summary.stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert!(summary.stages.iter().all(|s| s.error.is_none()));
    }

    #[tokio::test]
    async fn mock_binproto_bad_magic_handling() {
        // Answering garbage fails the handshake.
        let port = spawn_binproto_mock(BinFail::BadMagicReplies).await;
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        let st = v.stage1_handshake().await;
        assert!(!st.passed);
        assert!(st.error.as_deref().unwrap_or("").contains("bad magic drew"));
        assert_eq!(st.stage, 1);

        // Mute-on-garbage is accepted: silence within the window passes.
        let port = spawn_binproto_mock(BinFail::SilentOnGarbage).await;
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        let st = tokio::time::timeout(Duration::from_secs(10), v.stage1_handshake())
            .await
            .expect("silence check must finish inside its own 3s read window");
        assert!(st.passed, "silent rejection must pass: {:?}", st.error);
    }

    #[tokio::test]
    async fn mock_binproto_stage2_failure_modes() {
        let cases: &[(BinFail, &str)] = &[
            (BinFail::EchoFlipValue, "ECHO mismatch"),
            (BinFail::EchoWrongSeq, "empty ECHO wrong"),
            (BinFail::EchoBadStatus, "empty ECHO wrong"),
        ];
        for (fail, needle) in cases {
            let port = spawn_binproto_mock(*fail).await;
            let v = BinprotoVerifier::new(port, Some("mockseed"));
            let st = v.stage2_roundtrip().await;
            assert!(!st.passed, "{fail:?} must fail stage 2");
            let err = st.error.as_deref().unwrap_or("");
            assert!(err.contains(needle), "{fail:?}: {err:?} missing {needle:?}");
            assert_eq!(st.stage, 2);
        }
    }

    #[tokio::test]
    async fn mock_binproto_stage3_failure_modes() {
        let cases: &[(BinFail, &str)] = &[
            (BinFail::CrcAccepts, "CRC-corrupt reply wrong"),
            (BinFail::CrcCloses, "server closed connection"),
            (BinFail::UnknownOpOk, "unknown-op reply wrong"),
        ];
        for (fail, needle) in cases {
            let port = spawn_binproto_mock(*fail).await;
            let v = BinprotoVerifier::new(port, Some("mockseed"));
            let st = v.stage3_integrity().await;
            assert!(!st.passed, "{fail:?} must fail stage 3");
            let err = st.error.as_deref().unwrap_or("");
            assert!(err.contains(needle), "{fail:?}: {err:?} missing {needle:?}");
            assert_eq!(st.stage, 3);
        }
    }

    #[tokio::test]
    async fn mock_binproto_stage4_failure_modes() {
        let cases: &[(BinFail, &str)] = &[
            (BinFail::PutWrongStatus, "PUT reply wrong"),
            (BinFail::KvMissing, "GET mismatch"),
            (BinFail::KvMissingOk, "missing-key reply wrong"),
        ];
        for (fail, needle) in cases {
            let port = spawn_binproto_mock(*fail).await;
            let v = BinprotoVerifier::new(port, Some("mockseed"));
            let st = v.stage4_kv().await;
            assert!(!st.passed, "{fail:?} must fail stage 4");
            let err = st.error.as_deref().unwrap_or("");
            assert!(err.contains(needle), "{fail:?}: {err:?} missing {needle:?}");
            assert_eq!(st.stage, 4);
        }
    }

    #[tokio::test]
    async fn mock_binproto_stage5_failure_modes() {
        let cases: &[(BinFail, &str)] = &[
            (BinFail::PipelineShiftId, "IDs unmatched"),
            (BinFail::PipelineWrongPayload, "pipeline frame 1000 corrupt"),
        ];
        for (fail, needle) in cases {
            let port = spawn_binproto_mock(*fail).await;
            let v = BinprotoVerifier::new(port, Some("mockseed"));
            let st = v.stage5_pipeline().await;
            assert!(!st.passed, "{fail:?} must fail stage 5");
            let err = st.error.as_deref().unwrap_or("");
            assert!(err.contains(needle), "{fail:?}: {err:?} missing {needle:?}");
            assert_eq!(st.stage, 5);
        }
    }

    #[tokio::test]
    async fn mock_binproto_measure_throughput_and_closed_peer() {
        let port = spawn_binproto_mock(BinFail::None).await;
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        let rate = v
            .measure_throughput()
            .await
            .expect("200 pipelined ECHOs must be answered");
        assert!(rate.is_finite() && rate > 0.0, "rate {rate}");

        // A peer that closes immediately yields None, not a hang or panic.
        let port = spawn_binproto_mock(BinFail::CloseOnAccept).await;
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        assert!(v.measure_throughput().await.is_none());
    }

    #[tokio::test]
    async fn mock_binproto_corrupt_response_crc_fails_framing() {
        // Responses with a bad CRC word must be rejected by the frame
        // decoder, not silently accepted.
        let port = spawn_binproto_mock(BinFail::ReplyBadCrc).await;
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        assert!(v.measure_throughput().await.is_none());
        let st = v.stage5_pipeline().await;
        assert!(!st.passed);
        assert!(st.error.as_deref().unwrap_or("").contains("crc mismatch"));
    }

    #[tokio::test]
    async fn mock_binproto_silent_server_times_out_to_none() {
        let port = spawn_binproto_mock(BinFail::SilenceAll).await;
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        let out = tokio::time::timeout(Duration::from_secs(10), v.measure_throughput()).await;
        assert!(
            out.expect("must obey its own 3s read window").is_none(),
            "a mute server yields no throughput"
        );
    }

    #[tokio::test]
    async fn mock_binproto_unreachable_target_is_reported() {
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        let v = BinprotoVerifier::new(port, Some("mockseed"));
        let st = v.stage1_handshake().await;
        assert!(!st.passed);
        assert!(st
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Failed to connect"));
        assert!(v.measure_throughput().await.is_none());
    }
}
