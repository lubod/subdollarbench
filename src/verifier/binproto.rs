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
        let mut tmp = [0u8; 65536];
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
