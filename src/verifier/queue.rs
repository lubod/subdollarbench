//! Job-queue verifier (sdb-queue campaign): line protocol on TCP port 6385.
//!
//! Contract (LF-terminated lines, `[A-Za-z0-9._-]` names, payloads <= 8KiB):
//! - `ENQ <queue> <payload>` -> `+OK <id>`; delayed form
//!   `ENQ <queue> <payload> IN <delay_ms>`.
//! - `LEASE <queue> <lease_ms>` -> `+JOB <id> <attempts> <payload>` | `+NONE`.
//!   A leased job is invisible to other consumers until acked/nacked or the
//!   lease expires; a lease is owned by the connection that took it.
//! - `ACK <id>` / `NACK <id> [delay_ms]` -> `+OK`; `-ERR not-leased` for
//!   unknown/expired ids, `-ERR not-owner` when another connection holds the
//!   lease. NACK returns the job for redelivery (optionally delayed).
//! - After `MAX_ATTEMPTS` failures (a nack or expiry at attempts ==
//!   MAX_ATTEMPTS) the job moves to `<queue>.dead` and is never redelivered
//!   to the main queue. Leasing from a `.dead` queue is an inspection: the
//!   reported attempts stay at the failed count (reads do not increment it).
//! - `REDRIVE <queue> <id>` -> `+OK` moves a dead job back (attempts reset).
//! - `STATS <queue>` -> `+STATS enqueued=<n> acked=<n> nacked=<n>
//!   inflight=<n> available=<n> dead=<n>`.
//! - `PING` -> `+PONG`. Malformed input -> `-ERR ...` (never a crash).
//! - Durability: acknowledged state survives `kill -9`; persistent state
//!   lives under `./data`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::StageResult;

/// Attempts after which a failing job is dead-lettered.
pub const MAX_ATTEMPTS: u32 = 3;
/// Payload cap the contract advertises (the hardening stage sends 4x this).
pub const MAX_PAYLOAD: usize = 8 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct QueueVerifier {
    pub target_port: u16,
    pub run_seed: Option<String>,
}

fn stage_fail(stage: u32, name: &str, err: String) -> StageResult {
    StageResult {
        stage,
        name: name.to_string(),
        passed: false,
        error: Some(err),
    }
}

fn stage_ok(stage: u32, name: &str) -> StageResult {
    StageResult {
        stage,
        name: name.to_string(),
        passed: true,
        error: None,
    }
}

/// A leased job as reported by `LEASE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedJob {
    pub id: u64,
    pub attempts: u32,
    pub payload: String,
}

/// Parse `+JOB <id> <attempts> <payload>`; None for `+NONE`.
fn parse_job(reply: &str) -> Result<Option<LeasedJob>, String> {
    let reply = reply.trim_end();
    if reply == "+NONE" {
        return Ok(None);
    }
    let mut parts = reply.splitn(4, ' ');
    let tag = parts.next().unwrap_or("");
    if tag != "+JOB" {
        return Err(format!("expected +JOB/+NONE, got {reply:?}"));
    }
    let id = parts
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| format!("bad job id in {reply:?}"))?;
    let attempts = parts
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .ok_or_else(|| format!("bad attempts in {reply:?}"))?;
    let payload = parts
        .next()
        .ok_or_else(|| format!("missing payload in {reply:?}"))?
        .to_string();
    Ok(Some(LeasedJob {
        id,
        attempts,
        payload,
    }))
}

/// Parse `+OK <id>`.
fn parse_ok_id(reply: &str) -> Result<u64, String> {
    let reply = reply.trim_end();
    let rest = reply
        .strip_prefix("+OK ")
        .ok_or_else(|| format!("expected +OK <id>, got {reply:?}"))?;
    rest.trim()
        .parse::<u64>()
        .map_err(|_| format!("bad id in {reply:?}"))
}

/// Parse `+STATS k=v ...` into a map.
fn parse_stats(reply: &str) -> Result<HashMap<String, u64>, String> {
    let reply = reply.trim_end();
    let rest = reply
        .strip_prefix("+STATS ")
        .ok_or_else(|| format!("expected +STATS ..., got {reply:?}"))?;
    let mut out = HashMap::new();
    for pair in rest.split_whitespace() {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("bad stats pair {pair:?}"))?;
        let n = v
            .parse::<u64>()
            .map_err(|_| format!("bad stats value {pair:?}"))?;
        out.insert(k.to_string(), n);
    }
    Ok(out)
}

/// One line-oriented connection to the broker.
pub struct QueueConn {
    stream: BufReader<TcpStream>,
}

impl QueueConn {
    pub async fn connect(port: u16) -> Result<Self, String> {
        let addr = format!("127.0.0.1:{port}");
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("connect to {addr} timed out"))?
            .map_err(|e| format!("connect to {addr}: {e}"))?;
        Ok(Self {
            stream: BufReader::new(conn),
        })
    }

    /// Send a command and read exactly one reply line.
    pub async fn cmd(&mut self, line: &str) -> Result<String, String> {
        self.stream
            .get_mut()
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("write {line:?}: {e}"))?;
        self.read_line().await
    }

    /// Read one LF-terminated reply line (CR trimmed).
    pub async fn read_line(&mut self) -> Result<String, String> {
        let mut buf = String::new();
        let n = tokio::time::timeout(READ_TIMEOUT, self.stream.read_line(&mut buf))
            .await
            .map_err(|_| "read timed out".to_string())?
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed".to_string());
        }
        Ok(buf.trim_end_matches(['\n', '\r']).to_string())
    }

    /// Raw socket access for hardening probes.
    pub fn into_inner(self) -> TcpStream {
        self.stream.into_inner()
    }
}

impl QueueVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        Self {
            target_port,
            run_seed: seed.map(|s| s.to_string()),
        }
    }

    fn tag(&self) -> &str {
        super::get_seed_prefix(self.run_seed.as_deref(), "q")
    }

    fn queue(&self, name: &str) -> String {
        format!("{name}.{}", self.tag())
    }

    /// All stages (single-shot order); campaign runs gate by acceptance.
    pub async fn run_all(&self) -> QueueTestSummary {
        let stages = vec![
            self.test_stage1_basic().await,
            self.test_stage2_leases().await,
            self.test_stage3_nack().await,
            self.test_stage4_dlq().await,
            // Stage 5 (durability) needs a process restart and is
            // orchestrated by the pipeline; the single-shot suite reports
            // the prepare half and a restart note.
            self.test_stage5_durability_prepare().await,
            self.test_stage6_concurrency().await,
            self.test_stage7_stats().await,
            self.test_stage8_delay_redrive().await,
            self.test_stage9_hardening().await,
        ];
        let passed = stages.iter().filter(|s| s.passed).count() as u32;
        let total = stages.len() as u32;
        QueueTestSummary {
            stages,
            passed_count: passed,
            total_stages: total,
            pass_rate: if total == 0 {
                0.0
            } else {
                passed as f64 / total as f64 * 100.0
            },
        }
    }

    /// Stage 1: enqueue -> lease (FIFO) -> ack; ids monotonic; payload
    /// byte-exact; an empty queue leases `+NONE`.
    pub async fn test_stage1_basic(&self) -> StageResult {
        let name = "Stage 1: ENQ/LEASE/ACK round trip (FIFO)";
        let q = self.queue("basic");
        match self.stage1_inner(&q).await {
            Ok(()) => stage_ok(1, name),
            Err(e) => stage_fail(1, name, e),
        }
    }

    async fn stage1_inner(&self, q: &str) -> Result<(), String> {
        let mut c = QueueConn::connect(self.target_port).await?;
        let mut ids = Vec::new();
        for i in 0..3u32 {
            let payload = format!("job-{i}");
            let reply = c.cmd(&format!("ENQ {q} {payload}")).await?;
            ids.push(parse_ok_id(&reply)?);
        }
        if !(ids[0] < ids[1] && ids[1] < ids[2]) {
            return Err(format!("ids not increasing: {ids:?}"));
        }
        // FIFO: leases come back in enqueue order with attempts 1.
        for i in 0..3u32 {
            let reply = c.cmd(&format!("LEASE {q} 5000")).await?;
            let job = parse_job(&reply)?
                .ok_or_else(|| format!("queue empty at lease {i} (expected FIFO order)"))?;
            if job.payload != format!("job-{i}") {
                return Err(format!(
                    "FIFO violated: expected job-{i}, got {:?}",
                    job.payload
                ));
            }
            if job.attempts != 1 {
                return Err(format!("first lease attempts = {} (want 1)", job.attempts));
            }
            let ack = c.cmd(&format!("ACK {}", job.id)).await?;
            if ack != "+OK" {
                return Err(format!("ACK reply {ack:?}"));
            }
        }
        let reply = c.cmd(&format!("LEASE {q} 1000")).await?;
        if parse_job(&reply)?.is_some() {
            return Err("queue still holds jobs after acking all three".to_string());
        }
        Ok(())
    }

    /// Stage 2: visibility + lease expiry + ownership.
    pub async fn test_stage2_leases(&self) -> StageResult {
        let name = "Stage 2: Visibility leases (expiry + ownership)";
        let q = self.queue("lease");
        match self.stage2_inner(&q).await {
            Ok(()) => stage_ok(2, name),
            Err(e) => stage_fail(2, name, e),
        }
    }

    async fn stage2_inner(&self, q: &str) -> Result<(), String> {
        let mut owner = QueueConn::connect(self.target_port).await?;
        let mut other = QueueConn::connect(self.target_port).await?;
        owner.cmd(&format!("ENQ {q} invis")).await?;
        let job = parse_job(&owner.cmd(&format!("LEASE {q} 1500")).await?)?
            .ok_or("first lease returned +NONE")?;
        // Invisible to another consumer while leased.
        if parse_job(&other.cmd(&format!("LEASE {q} 1000")).await?)?.is_some() {
            return Err("a leased job was delivered to a second consumer".to_string());
        }
        // Expiry: after the visibility window the job returns, attempts+1.
        tokio::time::sleep(Duration::from_millis(2100)).await;
        let again = parse_job(&other.cmd(&format!("LEASE {q} 5000")).await?)?
            .ok_or("job did not return after lease expiry")?;
        if again.id != job.id || again.attempts != 2 {
            return Err(format!(
                "post-expiry lease: id {} attempts {} (want id {} attempts 2)",
                again.id, again.attempts, job.id
            ));
        }
        // Ownership: the original (now stale) connection cannot ack it.
        let reply = owner.cmd(&format!("ACK {}", job.id)).await?;
        if !reply.starts_with("-ERR") {
            return Err(format!("ACK from a non-owner must fail, got {reply:?}"));
        }
        // The owner of record can ack.
        let ack = other.cmd(&format!("ACK {}", job.id)).await?;
        if ack != "+OK" {
            return Err(format!("owner ACK reply {ack:?}"));
        }
        Ok(())
    }

    /// Stage 3: NACK (immediate + delayed) and attempt accounting.
    pub async fn test_stage3_nack(&self) -> StageResult {
        let name = "Stage 3: NACK and redelivery (delayed + attempts)";
        let q = self.queue("nack");
        match self.stage3_inner(&q).await {
            Ok(()) => stage_ok(3, name),
            Err(e) => stage_fail(3, name, e),
        }
    }

    async fn stage3_inner(&self, q: &str) -> Result<(), String> {
        let mut c = QueueConn::connect(self.target_port).await?;
        let mut c2 = QueueConn::connect(self.target_port).await?;
        let id = parse_ok_id(&c.cmd(&format!("ENQ {q} retryme")).await?)?;
        let first = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?.ok_or("no job")?;
        if first.id != id || first.attempts != 1 {
            return Err(format!("first lease {first:?}"));
        }
        // Immediate NACK: available right away, attempts+1 on redelivery.
        let nack = c.cmd(&format!("NACK {}", id)).await?;
        if nack != "+OK" {
            return Err(format!("NACK reply {nack:?}"));
        }
        let second = parse_job(&c2.cmd(&format!("LEASE {q} 5000")).await?)?
            .ok_or("job not available immediately after NACK")?;
        if second.attempts != 2 {
            return Err(format!(
                "attempts after NACK = {} (want 2)",
                second.attempts
            ));
        }
        // Delayed NACK: not available until the delay passes.
        let nack = c2.cmd(&format!("NACK {} 1500", second.id)).await?;
        if nack != "+OK" {
            return Err(format!("delayed NACK reply {nack:?}"));
        }
        if parse_job(&c.cmd(&format!("LEASE {q} 1000")).await?)?.is_some() {
            return Err("delayed NACK returned the job immediately".to_string());
        }
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let third = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?
            .ok_or("job missing after the NACK delay")?;
        if third.attempts != 3 {
            return Err(format!(
                "attempts after delay = {} (want 3)",
                third.attempts
            ));
        }
        // Unknown id is an error, not a crash.
        let reply = c.cmd("ACK 99999999").await?;
        if !reply.starts_with("-ERR") {
            return Err(format!("ACK of an unknown id must error, got {reply:?}"));
        }
        let ack = c.cmd(&format!("ACK {}", third.id)).await?;
        if ack != "+OK" {
            return Err(format!("final ACK reply {ack:?}"));
        }
        Ok(())
    }

    /// Stage 4: max attempts -> dead-letter queue.
    pub async fn test_stage4_dlq(&self) -> StageResult {
        let name = "Stage 4: Retries exhaust -> dead-letter queue";
        let q = self.queue("dlq");
        match self.stage4_inner(&q).await {
            Ok(()) => stage_ok(4, name),
            Err(e) => stage_fail(4, name, e),
        }
    }

    async fn stage4_inner(&self, q: &str) -> Result<(), String> {
        let mut c = QueueConn::connect(self.target_port).await?;
        let id = parse_ok_id(&c.cmd(&format!("ENQ {q} poison")).await?)?;
        let mut attempts_seen = Vec::new();
        for _ in 0..MAX_ATTEMPTS {
            let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?
                .ok_or("job unavailable before attempts were exhausted")?;
            attempts_seen.push(job.attempts);
            let nack = c.cmd(&format!("NACK {}", job.id)).await?;
            if nack != "+OK" {
                return Err(format!("NACK reply {nack:?}"));
            }
        }
        if attempts_seen != vec![1, 2, 3] {
            return Err(format!(
                "attempts sequence {attempts_seen:?} (want [1,2,3])"
            ));
        }
        // Fourth lease: the job must be gone from the live queue.
        if parse_job(&c.cmd(&format!("LEASE {q} 1000")).await?)?.is_some() {
            return Err(format!(
                "job redelivered after {} attempts (want DLQ move)",
                MAX_ATTEMPTS
            ));
        }
        // It lives in <queue>.dead with its payload and attempt count.
        let dead_q = format!("{q}.dead");
        let dead = parse_job(&c.cmd(&format!("LEASE {dead_q} 5000")).await?)?
            .ok_or("job not found in the dead-letter queue")?;
        if dead.id != id || dead.payload != "poison" || dead.attempts != MAX_ATTEMPTS {
            return Err(format!(
                "dead-letter entry {dead:?} (want id {id}, attempts {MAX_ATTEMPTS})"
            ));
        }
        // Dead-letter jobs are acked like any other queue.
        let ack = c.cmd(&format!("ACK {}", dead.id)).await?;
        if ack != "+OK" {
            return Err(format!("dead-letter ACK reply {ack:?}"));
        }
        // The live queue still works after all of that.
        let id2 = parse_ok_id(&c.cmd(&format!("ENQ {q} fresh")).await?)?;
        if id2 <= id {
            return Err(format!("ids not increasing after DLQ ({id2} <= {id})"));
        }
        Ok(())
    }

    /// Label for the seeded durability queues (shared by prepare/verify).
    fn durability_queue(&self) -> String {
        self.queue("durable")
    }

    /// Stage 5a: establish a durable state the pipeline can carry across a
    /// SIGKILL + restart: one acked job (must never return), one available
    /// job, one in-flight job with attempts=1 (must return), one dead-letter
    /// job.
    pub async fn test_stage5_durability_prepare(&self) -> StageResult {
        let name = "Stage 5: Durability across kill -9";
        let q = self.durability_queue();
        match self.stage5_prepare_inner(&q).await {
            Ok(()) => stage_ok(5, name),
            Err(e) => stage_fail(5, name, e),
        }
    }

    async fn stage5_prepare_inner(&self, q: &str) -> Result<(), String> {
        let mut c = QueueConn::connect(self.target_port).await?;
        // acked: lease + ack.
        let acked = parse_ok_id(&c.cmd(&format!("ENQ {q} acked")).await?)?;
        let job = parse_job(&c.cmd(&format!("LEASE {q} 60000")).await?)?.ok_or("no acked job")?;
        if c.cmd(&format!("ACK {}", job.id)).await? != "+OK" {
            return Err("prepare: ack failed".to_string());
        }
        // available: enqueue only.
        let _available = parse_ok_id(&c.cmd(&format!("ENQ {q} waiting")).await?)?;
        // in-flight: lease and leave it (long lease so it is still held at kill).
        let inflight = parse_ok_id(&c.cmd(&format!("ENQ {q} inflight")).await?)?;
        let job =
            parse_job(&c.cmd(&format!("LEASE {q} 60000")).await?)?.ok_or("no inflight job")?;
        if job.id != inflight || job.attempts != 1 {
            return Err(format!("prepare: inflight lease {job:?}"));
        }
        // dead: exhaust attempts.
        let dead = parse_ok_id(&c.cmd(&format!("ENQ {q} deadjob")).await?)?;
        for _ in 0..MAX_ATTEMPTS {
            let job =
                parse_job(&c.cmd(&format!("LEASE {q} 60000")).await?)?.ok_or("no dead job")?;
            if c.cmd(&format!("NACK {}", job.id)).await? != "+OK" {
                return Err("prepare: nack failed".to_string());
            }
        }
        if dead == acked {
            return Err("prepare: duplicate ids".to_string());
        }
        Ok(())
    }

    /// Stage 5b: after the pipeline SIGKILLs and restarts the broker: the
    /// acked job must never return, the available job is leasable (attempts
    /// 1), the in-flight job returns with attempts preserved (next lease
    /// shows 2), and the dead-letter queue is intact.
    pub async fn test_stage5_durability_verify(&self) -> StageResult {
        let name = "Stage 5: Durability across kill -9";
        let q = self.durability_queue();
        match self.stage5_verify_inner(&q).await {
            Ok(()) => stage_ok(5, name),
            Err(e) => stage_fail(5, name, e),
        }
    }

    async fn stage5_verify_inner(&self, q: &str) -> Result<(), String> {
        let mut c = QueueConn::connect(self.target_port).await?;
        // Drain the live queue: exactly the available + in-flight jobs.
        let mut seen: Vec<(u64, u32, String)> = Vec::new();
        for _ in 0..8 {
            match parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)? {
                Some(job) => {
                    seen.push((job.id, job.attempts, job.payload.clone()));
                    if c.cmd(&format!("ACK {}", job.id)).await? != "+OK" {
                        return Err("verify: ack failed".to_string());
                    }
                }
                None => break,
            }
        }
        let payloads: Vec<&str> = seen.iter().map(|(_, _, p)| p.as_str()).collect();
        if payloads.contains(&"acked") {
            return Err("an acked job came back after restart (lost ack)".to_string());
        }
        if !payloads.contains(&"waiting") {
            return Err(format!(
                "the available job did not survive restart (saw {payloads:?})"
            ));
        }
        let inflight = seen
            .iter()
            .find(|(_, _, p)| p == "inflight")
            .ok_or_else(|| format!("the in-flight job did not return (saw {payloads:?})"))?;
        if inflight.1 != 2 {
            return Err(format!(
                "in-flight job attempts after restart = {} (want 2)",
                inflight.1
            ));
        }
        if seen.len() != 2 {
            return Err(format!(
                "expected exactly 2 live jobs after restart, saw {payloads:?}"
            ));
        }
        // Dead-letter entry survived with its attempts.
        let dead_q = format!("{q}.dead");
        let dead = parse_job(&c.cmd(&format!("LEASE {dead_q} 5000")).await?)?
            .ok_or("dead-letter job did not survive restart")?;
        if dead.payload != "deadjob" || dead.attempts != MAX_ATTEMPTS {
            return Err(format!("dead-letter after restart {dead:?}"));
        }
        let _ = c.cmd(&format!("ACK {}", dead.id)).await?;
        Ok(())
    }

    /// Stage 6: concurrent consumers never see the same job twice.
    pub async fn test_stage6_concurrency(&self) -> StageResult {
        let name = "Stage 6: No duplicate delivery under contention";
        let q = self.queue("race");
        match self.stage6_inner(&q).await {
            Ok(()) => stage_ok(6, name),
            Err(e) => stage_fail(6, name, e),
        }
    }

    async fn stage6_inner(&self, q: &str) -> Result<(), String> {
        const JOBS: u64 = 200;
        const CONSUMERS: usize = 6;
        let mut producer = QueueConn::connect(self.target_port).await?;
        for i in 0..JOBS {
            producer.cmd(&format!("ENQ {q} m{i:04}")).await?;
        }
        let acked = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let duplicates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..CONSUMERS {
            let acked = acked.clone();
            let duplicates = duplicates.clone();
            let q = q.to_string();
            let port = self.target_port;
            handles.push(tokio::spawn(async move {
                let mut c = QueueConn::connect(port).await?;
                loop {
                    let reply = c.cmd(&format!("LEASE {q} 10000")).await?;
                    let Some(job) = parse_job(&reply)? else {
                        return Ok::<(), String>(());
                    };
                    if !acked.lock().unwrap().insert(job.id) {
                        duplicates.lock().unwrap().push(job.id);
                    }
                    if c.cmd(&format!("ACK {}", job.id)).await? != "+OK" {
                        return Err(format!("ack failed for id {}", job.id));
                    }
                }
            }));
        }
        for h in handles {
            h.await.map_err(|e| format!("consumer task: {e}"))??;
        }
        let acked = acked.lock().unwrap().clone();
        if !duplicates.lock().unwrap().is_empty() {
            return Err(format!(
                "the same job was leased to two consumers: {:?}",
                duplicates.lock().unwrap()
            ));
        }
        if acked.len() != JOBS as usize {
            return Err(format!(
                "{} of {JOBS} jobs were acked (loss under contention)",
                acked.len()
            ));
        }
        Ok(())
    }

    /// Stage 7: STATS reflects a known workload.
    pub async fn test_stage7_stats(&self) -> StageResult {
        let name = "Stage 7: Observability (STATS counters)";
        let q = self.queue("stats");
        match self.stage7_inner(&q).await {
            Ok(()) => stage_ok(7, name),
            Err(e) => stage_fail(7, name, e),
        }
    }

    async fn stage7_inner(&self, q: &str) -> Result<(), String> {
        let mut c = QueueConn::connect(self.target_port).await?;
        for i in 0..10u32 {
            c.cmd(&format!("ENQ {q} s{i}")).await?;
        }
        // 4 acked.
        for _ in 0..4 {
            let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?.ok_or("no job")?;
            c.cmd(&format!("ACK {}", job.id)).await?;
        }
        // 2 nacked (return to available).
        for _ in 0..2 {
            let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?.ok_or("no job")?;
            c.cmd(&format!("NACK {}", job.id)).await?;
        }
        // 1 leased and left in flight.
        let job = parse_job(&c.cmd(&format!("LEASE {q} 60000")).await?)?.ok_or("no job")?;
        let _inflight = job.id;
        let stats = parse_stats(&c.cmd(&format!("STATS {q}")).await?)?;
        let get = |k: &str| -> Result<u64, String> {
            stats
                .get(k)
                .copied()
                .ok_or_else(|| format!("STATS missing {k}: {stats:?}"))
        };
        let (enq, acked, nacked, inflight, available, dead) = (
            get("enqueued")?,
            get("acked")?,
            get("nacked")?,
            get("inflight")?,
            get("available")?,
            get("dead")?,
        );
        if (enq, acked, nacked, inflight, available, dead) != (10, 4, 2, 1, 5, 0) {
            return Err(format!(
                "STATS mismatch: enqueued={enq} acked={acked} nacked={nacked} inflight={inflight} available={available} dead={dead} (want 10/4/2/1/5/0)"
            ));
        }
        Ok(())
    }

    /// Stage 8: delayed enqueue + dead-letter redrive.
    pub async fn test_stage8_delay_redrive(&self) -> StageResult {
        let name = "Stage 8: Delayed delivery + DLQ redrive";
        let q = self.queue("defer");
        match self.stage8_inner(&q).await {
            Ok(()) => stage_ok(8, name),
            Err(e) => stage_fail(8, name, e),
        }
    }

    async fn stage8_inner(&self, q: &str) -> Result<(), String> {
        let mut c = QueueConn::connect(self.target_port).await?;
        // Delayed ENQ: not leasable before the delay, leasable after.
        let id = parse_ok_id(&c.cmd(&format!("ENQ {q} later IN 1500")).await?)?;
        if parse_job(&c.cmd(&format!("LEASE {q} 1000")).await?)?.is_some() {
            return Err("a delayed job was leasable before its delay".to_string());
        }
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?
            .ok_or("delayed job never became leasable")?;
        if job.id != id || job.payload != "later" || job.attempts != 1 {
            return Err(format!("delayed job {job:?} (want id {id}, attempts 1)"));
        }
        c.cmd(&format!("ACK {}", job.id)).await?;
        // Redrive: poison a job into the DLQ, then move it back.
        let dead_id = parse_ok_id(&c.cmd(&format!("ENQ {q} revivable")).await?)?;
        for _ in 0..MAX_ATTEMPTS {
            let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?.ok_or("no job")?;
            c.cmd(&format!("NACK {}", job.id)).await?;
        }
        let dead = parse_job(&c.cmd(&format!("LEASE {q}.dead 5000")).await?)?
            .ok_or("job did not reach the DLQ")?;
        // Consume it so the DLQ entry is acked, then redrive by id.
        c.cmd(&format!("ACK {}", dead.id)).await?;
        let again = parse_ok_id(&c.cmd(&format!("ENQ {q} revivable2")).await?)?;
        for _ in 0..MAX_ATTEMPTS {
            let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?.ok_or("no job")?;
            c.cmd(&format!("NACK {}", job.id)).await?;
        }
        let reply = c.cmd(&format!("REDRIVE {q} {again}")).await?;
        if reply != "+OK" {
            return Err(format!("REDRIVE reply {reply:?}"));
        }
        // Back in the live queue with attempts reset: the next lease is
        // attempts=1 again.
        let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?
            .ok_or("redriven job not in the live queue")?;
        if job.id != again || job.attempts != 1 {
            return Err(format!(
                "redriven job {job:?} (want id {again}, attempts reset to 1)"
            ));
        }
        c.cmd(&format!("ACK {}", job.id)).await?;
        // Unknown redrive id errors.
        let reply = c.cmd(&format!("REDRIVE {q} 99999999")).await?;
        if !reply.starts_with("-ERR") {
            return Err(format!(
                "REDRIVE of an unknown id must error, got {reply:?}"
            ));
        }
        let _ = dead_id;
        Ok(())
    }

    /// Stage 9: hardening (oversized payload, malformed lines, stalled
    /// partial line) with normal traffic still flowing.
    pub async fn test_stage9_hardening(&self) -> StageResult {
        let name = "Stage 9: Limits and hostile input";
        let q = self.queue("hard");
        match self.stage9_inner(&q).await {
            Ok(()) => stage_ok(9, name),
            Err(e) => stage_fail(9, name, e),
        }
    }

    async fn stage9_inner(&self, q: &str) -> Result<(), String> {
        // Oversized payload: -ERR (or a close), server survives.
        let big = "x".repeat(MAX_PAYLOAD * 4);
        let mut c = QueueConn::connect(self.target_port).await?;
        match c.cmd(&format!("ENQ {q} {big}")).await {
            Ok(reply) if reply.starts_with("-ERR") => {}
            Ok(reply) => {
                return Err(format!(
                    "a {}-byte payload (cap {MAX_PAYLOAD}) must be rejected, got {reply:?}",
                    big.len()
                ))
            }
            Err(_) => {}
        }
        // Malformed lines: errors, no crash, connection still usable.
        let mut c = QueueConn::connect(self.target_port).await?;
        for bad in ["GARBAGE", "ENQ", "LEASE", "ACK", "ENQ onlyqueue"] {
            let reply = c.cmd(bad).await?;
            if !reply.starts_with("-ERR") {
                return Err(format!("malformed {bad:?} must answer -ERR, got {reply:?}"));
            }
        }
        // Normal traffic on the same connection after the abuse.
        let id = parse_ok_id(&c.cmd(&format!("ENQ {q} alive")).await?)?;
        let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await?)?.ok_or("no job")?;
        if job.id != id {
            return Err(format!("post-abuse lease {job:?} (want id {id})"));
        }
        c.cmd(&format!("ACK {}", job.id)).await?;
        // Slow-loris: a stalled partial line must be reclaimed within 12s
        // while other clients keep working.
        let mut raw = TcpStream::connect(("127.0.0.1", self.target_port))
            .await
            .map_err(|e| format!("stall connect: {e}"))?;
        raw.write_all(b"ENQ stall.partial")
            .await
            .map_err(|e| format!("stall write: {e}"))?;
        let mut probe = QueueConn::connect(self.target_port).await?;
        let reply = probe.cmd("PING").await?;
        if reply != "+PONG" {
            return Err(format!("PING while a slow client idled: {reply:?}"));
        }
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut buf = [0u8; 256];
        let mut reclaimed = false;
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), raw.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => {
                    reclaimed = true;
                    break;
                }
                Ok(Ok(_)) => {
                    // A 4xx-style error line also counts as reclaimed.
                    reclaimed = true;
                    break;
                }
                Err(_) => {}
            }
        }
        if !reclaimed {
            return Err(
                "a stalled partial line stayed open for 12s: enforce an idle/read timeout"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Soak measurement for the bench (jobs/sec: ENQ+LEASE+ACK cycles).
    pub async fn measure_throughput(&self) -> Option<f64> {
        let q = self.queue("throughput");
        let mut c = QueueConn::connect(self.target_port).await.ok()?;
        let start = Instant::now();
        let mut done = 0u32;
        for i in 0..300u32 {
            c.cmd(&format!("ENQ {q} t{i}")).await.ok()?;
            let job = parse_job(&c.cmd(&format!("LEASE {q} 5000")).await.ok()?).ok()??;
            c.cmd(&format!("ACK {}", job.id)).await.ok()?;
            done += 1;
        }
        if done < 250 {
            return None;
        }
        let secs = start.elapsed().as_secs_f64();
        (secs > 0.0).then_some(done as f64 / secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[test]
    fn phrase_parsers_are_strict() {
        assert_eq!(
            parse_job("+JOB 7 2 hello-world").unwrap(),
            Some(LeasedJob {
                id: 7,
                attempts: 2,
                payload: "hello-world".to_string()
            })
        );
        assert_eq!(parse_job("+NONE").unwrap(), None);
        assert!(parse_job("+OK 1").is_err());
        assert!(parse_job("+JOB xx 2 p").is_err());
        assert_eq!(parse_ok_id("+OK 42").unwrap(), 42);
        assert!(parse_ok_id("+OK").is_err());
        let stats =
            parse_stats("+STATS enqueued=10 acked=4 nacked=2 inflight=1 available=5 dead=0")
                .unwrap();
        assert_eq!(stats.get("acked"), Some(&4));
        assert!(parse_stats("+STATS enqueued=x").is_err());
    }

    /// Defect switches for the mock broker.
    #[derive(Clone, Copy, PartialEq)]
    enum QueueFail {
        None,
        /// Leased jobs never expire.
        NoExpiry,
        /// Any connection may ack any lease.
        NoOwnership,
        /// Jobs retry forever; the DLQ never fills.
        NoDlq,
        /// A leased job stays leasable by others (duplicate delivery).
        DuplicateDelivery,
        /// Stats are always zero.
        FrozenStats,
        /// Delayed jobs are available immediately.
        DelayVoid,
        /// Oversized payloads are accepted.
        OversizedOk,
        /// Stalled partial lines are never reclaimed.
        NeverReclaim,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum JobState {
        Available,
        Leased,
        Acked,
        Dead,
    }

    #[derive(Clone)]
    struct Job {
        id: u64,
        payload: String,
        /// Queue the job was enqueued to (survives DLQ moves).
        origin: String,
        /// Queue the job currently lives in (`<origin>` or `<origin>.dead`).
        queue: String,
        attempts: u32,
        nacks: u32,
        state: JobState,
        owner: u64,
        until: f64,
        not_before: f64,
    }

    struct Mock {
        jobs: Vec<Job>,
        next_id: u64,
        conns: u64,
        fail: QueueFail,
    }

    impl Mock {
        fn expire(&mut self, now: f64) {
            if self.fail == QueueFail::NoExpiry {
                return;
            }
            let fail = self.fail;
            for job in self.jobs.iter_mut() {
                if job.state == JobState::Leased && job.until <= now {
                    if job.attempts >= MAX_ATTEMPTS && fail != QueueFail::NoDlq {
                        job.queue = format!("{}.dead", job.queue);
                        job.state = JobState::Dead;
                    } else {
                        job.state = JobState::Available;
                        job.not_before = now;
                    }
                    job.owner = 0;
                }
            }
        }

        fn stats(&self, q: &str) -> String {
            if self.fail == QueueFail::FrozenStats {
                return "+STATS enqueued=0 acked=0 nacked=0 inflight=0 available=0 dead=0".into();
            }
            // Per-queue counters (origin survives DLQ moves and redrives).
            let enqueued = self.jobs.iter().filter(|j| j.origin == q).count();
            let acked = self
                .jobs
                .iter()
                .filter(|j| j.origin == q && j.state == JobState::Acked)
                .count();
            let nacked: u32 = self
                .jobs
                .iter()
                .filter(|j| j.origin == q)
                .map(|j| j.nacks)
                .sum();
            let inflight = self
                .jobs
                .iter()
                .filter(|j| j.origin == q && j.state == JobState::Leased)
                .count();
            let available = self
                .jobs
                .iter()
                .filter(|j| j.origin == q && j.state == JobState::Available)
                .count();
            let dead = self
                .jobs
                .iter()
                .filter(|j| j.origin == q && j.state == JobState::Dead)
                .count();
            format!(
                "+STATS enqueued={enqueued} acked={acked} nacked={nacked} inflight={inflight} available={available} dead={dead}"
            )
        }
    }

    fn now() -> f64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    /// Spawn a mock queue broker; returns its port.
    async fn spawn_queue_mock(fail: QueueFail) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(Mock {
            jobs: Vec::new(),
            next_id: 1,
            conns: 0,
            fail,
        }));
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let conn_id = {
                        let mut st = state.lock().unwrap();
                        st.conns += 1;
                        st.conns
                    };
                    serve_queue_conn(sock, conn_id, state).await;
                });
            }
        });
        port
    }

    async fn serve_queue_conn(sock: TcpStream, conn_id: u64, state: Arc<Mutex<Mock>>) {
        let mut reader = BufReader::new(sock);
        let mut line = String::new();
        loop {
            line.clear();
            let never = state.lock().unwrap().fail == QueueFail::NeverReclaim;
            let read = if never {
                reader.read_line(&mut line).await
            } else {
                match tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
                    .await
                {
                    Ok(r) => r,
                    Err(_) => return, // reclaim a stalled partial line
                }
            };
            match read {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let text = line.trim_end_matches(['\n', '\r']).to_string();
            let reply = handle_line(&text, conn_id, &state);
            if reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    }

    fn handle_line(line: &str, conn_id: u64, state: &Arc<Mutex<Mock>>) -> String {
        let parts: Vec<&str> = line.split(' ').collect();
        let mut st = state.lock().unwrap();
        let t = now();
        st.expire(t);
        match parts.as_slice() {
            ["PING"] => "+PONG".to_string(),
            ["ENQ", q, payload, "IN", delay] => {
                let ms: u64 = delay.parse().unwrap_or(0);
                let id = st.next_id;
                st.next_id += 1;
                let not_before = if st.fail == QueueFail::DelayVoid {
                    0.0
                } else {
                    t + ms as f64 / 1000.0
                };
                st.jobs.push(Job {
                    id,
                    payload: (*payload).to_string(),
                    origin: (*q).to_string(),
                    queue: (*q).to_string(),
                    attempts: 0,
                    nacks: 0,
                    state: JobState::Available,
                    owner: 0,
                    until: 0.0,
                    not_before,
                });
                format!("+OK {id}")
            }
            ["ENQ", q, payload]
                if payload.len() <= MAX_PAYLOAD || st.fail == QueueFail::OversizedOk =>
            {
                let id = st.next_id;
                st.next_id += 1;
                st.jobs.push(Job {
                    id,
                    payload: (*payload).to_string(),
                    origin: (*q).to_string(),
                    queue: (*q).to_string(),
                    attempts: 0,
                    nacks: 0,
                    state: JobState::Available,
                    owner: 0,
                    until: 0.0,
                    not_before: t,
                });
                format!("+OK {id}")
            }
            ["LEASE", q, lease] => {
                let lease_ms: u64 = lease.parse().unwrap_or(0);
                let dup = st.fail == QueueFail::DuplicateDelivery;
                let pick = st.jobs.iter().position(|j| {
                    j.queue == *q
                        && j.not_before <= t
                        && (j.state == JobState::Available
                            || j.state == JobState::Dead
                            || (dup && j.state == JobState::Leased))
                });
                match pick {
                    Some(i) => {
                        let job = &mut st.jobs[i];
                        // Reading a dead-letter entry is an inspection, not
                        // a delivery attempt: attempts keeps the failed count.
                        if !job.queue.ends_with(".dead") {
                            job.attempts += 1;
                        }
                        job.owner = conn_id;
                        job.until = t + lease_ms as f64 / 1000.0;
                        job.state = JobState::Leased;
                        format!("+JOB {} {} {}", job.id, job.attempts, job.payload)
                    }
                    None => "+NONE".to_string(),
                }
            }
            ["ACK", id] => {
                let Ok(id) = id.parse::<u64>() else {
                    return "-ERR bad id".to_string();
                };
                let dup = st.fail == QueueFail::DuplicateDelivery;
                let no_owner = st.fail == QueueFail::NoOwnership;
                let found = if dup {
                    st.jobs.iter_mut().find(|j| j.id == id)
                } else {
                    st.jobs
                        .iter_mut()
                        .find(|j| j.id == id && j.state == JobState::Leased)
                };
                if let Some(job) = found {
                    if !dup && job.owner != conn_id && !no_owner {
                        return "-ERR not-owner".to_string();
                    }
                    job.state = JobState::Acked;
                    "+OK".to_string()
                } else {
                    "-ERR not-leased".to_string()
                }
            }
            ["NACK", id, ..] => {
                let Ok(id) = id.parse::<u64>() else {
                    return "-ERR bad id".to_string();
                };
                let delay_ms: f64 = match parts.as_slice() {
                    ["NACK", _, d] => d.parse().unwrap_or(0.0),
                    _ => 0.0,
                };
                let no_owner = st.fail == QueueFail::NoOwnership;
                let dlq = st.fail != QueueFail::NoDlq;
                if let Some(job) = st
                    .jobs
                    .iter_mut()
                    .find(|j| j.id == id && j.state == JobState::Leased)
                {
                    if job.owner != conn_id && !no_owner {
                        return "-ERR not-owner".to_string();
                    }
                    if job.attempts >= MAX_ATTEMPTS && dlq && !job.queue.ends_with(".dead") {
                        job.queue = format!("{}.dead", job.queue);
                        job.state = JobState::Dead;
                    } else if job.attempts >= MAX_ATTEMPTS && dlq {
                        job.state = JobState::Dead;
                    } else {
                        job.state = JobState::Available;
                        job.not_before = t + delay_ms / 1000.0;
                    }
                    job.owner = 0;
                    job.nacks += 1;
                    "+OK".to_string()
                } else {
                    "-ERR not-leased".to_string()
                }
            }
            ["REDRIVE", q, id] => {
                let Ok(id) = id.parse::<u64>() else {
                    return "-ERR bad id".to_string();
                };
                let dead_q = format!("{q}.dead");
                if let Some(job) = st
                    .jobs
                    .iter_mut()
                    .find(|j| j.id == id && j.queue == dead_q && j.state == JobState::Dead)
                {
                    job.queue = (*q).to_string();
                    job.state = JobState::Available;
                    job.attempts = 0;
                    job.not_before = t;
                    "+OK".to_string()
                } else {
                    "-ERR unknown".to_string()
                }
            }
            ["STATS", q] => st.stats(q),
            _ => "-ERR syntax".to_string(),
        }
    }

    #[tokio::test]
    async fn mock_queue_passes_all_self_contained_stages() {
        let port = spawn_queue_mock(QueueFail::None).await;
        let v = QueueVerifier::new(port, Some("mockq"));
        let checks = vec![
            v.test_stage1_basic().await,
            v.test_stage2_leases().await,
            v.test_stage3_nack().await,
            v.test_stage4_dlq().await,
            v.test_stage6_concurrency().await,
            v.test_stage7_stats().await,
            v.test_stage8_delay_redrive().await,
            v.test_stage9_hardening().await,
        ];
        for st in &checks {
            assert!(st.passed, "stage {} failed: {:?}", st.stage, st.error);
        }
    }

    #[tokio::test]
    async fn mock_queue_defects_are_caught() {
        for (fail, stage, needle) in [
            (QueueFail::NoExpiry, 2u32, "did not return"),
            (QueueFail::NoOwnership, 2, "non-owner"),
            (QueueFail::NoDlq, 4, "redelivered"),
            (QueueFail::DuplicateDelivery, 6, "leased to two consumers"),
            (QueueFail::FrozenStats, 7, "STATS mismatch"),
            (QueueFail::DelayVoid, 8, "leasable before its delay"),
            (QueueFail::OversizedOk, 9, "must be rejected"),
            (QueueFail::NeverReclaim, 9, "stayed open for 12s"),
        ] {
            let port = spawn_queue_mock(fail).await;
            let v = QueueVerifier::new(port, Some("mockq"));
            let st = match stage {
                2 => v.test_stage2_leases().await,
                4 => v.test_stage4_dlq().await,
                6 => v.test_stage6_concurrency().await,
                7 => v.test_stage7_stats().await,
                8 => v.test_stage8_delay_redrive().await,
                _ => v.test_stage9_hardening().await,
            };
            assert!(
                !st.passed,
                "stage {stage} must fail for the injected defect"
            );
            let err = st.error.unwrap_or_default();
            assert!(
                err.contains(needle),
                "stage {stage} error {err:?} lacks {needle:?}"
            );
        }
    }
}
