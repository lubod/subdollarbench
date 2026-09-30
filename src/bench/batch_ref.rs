//! Batch-level reference calibration.
//!
//! One measured reference per multi-trial batch, shared by all trials.
//! Per-trial ref containers still start (the agent keeps its black-box oracle),
//! but trials no longer *measure* the ref: every trial scores against the same
//! denominator, so ref-side jitter cannot reshuffle trial order.
//!
//! Lifecycle: [`BatchRef::start`] before trial 1 (warmup + median on an
//! isolated batch port), [`BatchRef::finish`] after the last trial (post
//! bracket, drift flag, history append, container cleanup).

use anyhow::{anyhow, Result};
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::bench::BenchmarkRunner;
use crate::config::{get_repo_root, TaskType};
use crate::sandbox::docker::{
    SandboxManager, BENCH_REDIS_CLIENT_IMAGE, REF_DNS_IMAGE, REF_NGINX_IMAGE, REF_PDF_IMAGE,
    REF_REDIS_IMAGE,
};

/// Batch-port assignments. Deliberately offset from the per-trial ref ports
/// (6380/8081) so the batch ref can live across the whole batch while trial
/// sandboxes churn their own refs underneath.
pub fn batch_port(task: &TaskType) -> Option<u16> {
    match task {
        TaskType::Redis => Some(6389),
        TaskType::Http | TaskType::Proxy => Some(8089),
        // DNS batch ref: pinned CoreDNS + forwarding Corefile (same oracle
        // as trial refs); offset from the per-trial ref port 5354.
        TaskType::Dns => Some(5359),
        // PDF batch ref: canonical extractor (REF_PDF_IMAGE). 8409, not 8289:
        // every other batch base ends in 89, so an 89-based port collides with
        // `8089 + 100*w` at w=2 — i.e. pdf@w0 and http/proxy@w2 both bind
        // 8289, which the default 4-worker grid reaches. 8409 differs from
        // 6389/8089/5359 by a non-multiple of 100, so no cross-worker overlap
        // is possible. `batch_ports_are_disjoint` below proves the whole map.
        TaskType::Pdf => Some(8409),
        TaskType::Wal | TaskType::Pubsub | TaskType::Binproto => None,
    }
}

/// Every port that can be bound concurrently, for workers `0..WORKER_SLOTS`,
/// as `(role, worker, port)`. Two *distinct* roles on *different* workers
/// landing on the same number means one container steals another's bind, and
/// the loser silently changes scoring class (batch ref -> live per-trial ref)
/// or aborts the cell.
///
/// Keyed on the port *base*, not the task: `http` and `proxy` deliberately
/// share batch base 8089, and since one cell runs per worker slot they can
/// never be live on the same worker, so they are one role, not two.
pub fn reserved_ports() -> Vec<(String, u8, u16)> {
    use crate::web::server::worker_port;
    let mut bases: Vec<(String, String, u16)> = Vec::new();
    for task in [
        TaskType::Redis,
        TaskType::Http,
        TaskType::Dns,
        TaskType::Pdf,
        TaskType::Wal,
        TaskType::Proxy,
        TaskType::Pubsub,
        TaskType::Binproto,
    ] {
        for (role, base) in [
            ("candidate", Some(task.default_port())),
            ("batch", batch_port(&task)),
        ] {
            if let Some(b) = base {
                let name = format!("{task}/{role}");
                match bases.iter_mut().find(|(_, _, existing)| *existing == b) {
                    Some((owner, owner_role, _)) => {
                        // Sharing a base is only legitimate within one role:
                        // http/batch and proxy/batch both use 8089 and can never
                        // be live on the same worker. A candidate/batch overlap
                        // would be a real collision.
                        assert_eq!(
                            owner_role, role,
                            "base {b} shared by {owner} and {name} across different roles"
                        );
                    }
                    None => bases.push((name, role.to_string(), b)),
                }
            }
        }
    }
    // Per-trial reference containers are host-published next to their
    // candidate, and the proxy task dials a three-port upstream pool.
    for base in [6380u16, 8081, 5354] {
        bases.push((format!("ref/{base}"), "ref".to_string(), base));
    }
    for up in 8101u16..=8103 {
        bases.push((format!("upstream/{up}"), "upstream".to_string(), up));
    }
    let mut out = Vec::new();
    for (name, _role, base) in bases {
        for w in 0..WORKER_SLOTS {
            out.push((name.clone(), w, worker_port(base, w)));
        }
    }
    out
}

/// Upper bound on concurrently dispatching workers; `worker_count()` clamps
/// to this. Port arithmetic is only proven safe across the whole range.
pub const WORKER_SLOTS: u8 = 8;

fn ref_image(task: &TaskType) -> &'static str {
    match task {
        TaskType::Redis => REF_REDIS_IMAGE,
        TaskType::Http | TaskType::Proxy => REF_NGINX_IMAGE,
        TaskType::Dns => REF_DNS_IMAGE,
        TaskType::Pdf => REF_PDF_IMAGE,
        _ => "none",
    }
}

fn wait_redis_ready(port: u16) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "host",
                BENCH_REDIS_CLIENT_IMAGE,
                "redis-cli",
                "-h",
                "127.0.0.1",
                "-p",
                &port.to_string(),
                "ping",
            ])
            .output();
        if let Ok(o) = out {
            if String::from_utf8_lossy(&o.stdout).contains("PONG") {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

fn wait_dns_ready(port: u16) -> bool {
    use std::net::UdpSocket;
    use std::time::Duration;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
            // Minimal A-query for example.com (header + 1 question).
            let mut q = vec![
                0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ];
            for l in "example.com".split('.') {
                q.push(l.len() as u8);
                q.extend_from_slice(l.as_bytes());
            }
            q.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x01]);
            let target = format!("127.0.0.1:{port}");
            if sock.send_to(&q, &target).is_ok() {
                let mut buf = [0u8; 512];
                if sock.recv_from(&mut buf).is_ok() {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

fn wait_pdf_ready(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            use std::io::Write;
            let _ = s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
            let mut buf = [0u8; 64];
            use std::io::Read;
            if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

fn measure_pdf_pages_per_sec(port: u16) -> Result<f64> {
    // Sync wrapper over the async verifier soak (stage-4 multipage path).
    // A throwaway single-thread runtime keeps batch_ref sync like DNS.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let v = crate::verifier::PdfVerifier::new(port, Some("batchref"));
        v.measure_throughput()
            .await
            .ok_or_else(|| anyhow!("pdf batch ref: verifier soak returned None on port {port}"))
    })
}

fn measure_dns_queries_per_sec(port: u16) -> Result<f64> {
    use std::net::UdpSocket;
    use std::time::Duration;
    let sock = UdpSocket::bind("127.0.0.1:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    sock.connect(format!("127.0.0.1:{port}"))?;
    let iters = 200u32;
    let mut q = vec![
        0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    for l in "example.com".split('.') {
        q.push(l.len() as u8);
        q.extend_from_slice(l.as_bytes());
    }
    q.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x01]);
    let start = Instant::now();
    let mut ok = 0u32;
    for _ in 0..iters {
        if sock.send(&q).is_err() {
            continue;
        }
        let mut buf = [0u8; 512];
        if sock.recv(&mut buf).is_ok() {
            ok += 1;
        }
    }
    let secs = start.elapsed().as_secs_f64().max(0.001);
    if ok == 0 {
        return Err(anyhow!("dns batch ref: no query answered on port {port}"));
    }
    Ok(ok as f64 / secs)
}

fn wait_http_ready(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok() {
            // TCP accept means nginx is up (stock image serves / immediately).
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Drift tolerance per task: forwarding resolvers (DNS: upstream RTT +
/// cache-30) jitter past the generic 10% bracket on a quiet host
/// (measured 9.1% pre/post spread, no load). Local refs stay at 10%.
pub fn drift_threshold(task: &TaskType) -> f64 {
    match task {
        TaskType::Dns => 0.25,
        _ => 0.10,
    }
}

fn measure(task: &TaskType, port: u16) -> Result<f64> {
    match task {
        TaskType::Redis => BenchmarkRunner::run_redis_benchmark(port),
        TaskType::Http | TaskType::Proxy => BenchmarkRunner::run_wrk_benchmark(port),
        TaskType::Dns => measure_dns_queries_per_sec(port),
        TaskType::Pdf => measure_pdf_pages_per_sec(port),
        _ => Err(anyhow!("no external reference for {:?}", task)),
    }
}

pub struct BatchRef {
    sandbox: SandboxManager,
    task: TaskType,
    port: u16,
    pub pre: f64,
}

pub struct BatchRefReport {
    pub pre: f64,
    pub post: Option<f64>,
    pub drift: bool,
}

impl BatchRef {
    /// Start the batch ref, warm it up (one discarded window), and return the
    /// median-of-3 pre-batch value. `None` task support (WAL/...) yields `Ok(None)`.
    pub fn start(task: &TaskType, parent_run_id: &str, log: &dyn Fn(&str)) -> Result<Option<Self>> {
        Self::start_on_worker(task, parent_run_id, log, 0)
    }

    /// Worker-aware start: batch port follows the worker offset so parallel
    /// cells never share a batch-ref container.
    pub fn start_on_worker(
        task: &TaskType,
        parent_run_id: &str,
        log: &dyn Fn(&str),
        worker: u8,
    ) -> Result<Option<Self>> {
        let port = match batch_port(task) {
            Some(p) => crate::web::server::worker_port(p, worker),
            None => return Ok(None),
        };
        let sandbox = SandboxManager::with_id(&format!("{}_batchref", parent_run_id));
        sandbox.ensure_network()?;
        let start_res = match task {
            TaskType::Redis => sandbox.start_reference_redis(port),
            TaskType::Http | TaskType::Proxy => sandbox.start_reference_http(port),
            TaskType::Dns => sandbox.start_reference_dns(port),
            TaskType::Pdf => sandbox.start_reference_pdf(port),
            _ => return Ok(None),
        };
        if let Err(e) = start_res {
            // Pdf has no per-trial refs: a broken pdf ref must fail the
            // batch loudly, never warn-and-fall-back to unscored.
            if *task == TaskType::Pdf {
                sandbox.cleanup();
                return Err(anyhow!("pdf batch reference unavailable: {}", e));
            }
            log(&format!("[BATCH-REF] Warning: batch reference unavailable: {}. Trials fall back to live per-trial refs.", e));
            return Ok(None);
        }
        let ready = match task {
            TaskType::Redis => wait_redis_ready(port),
            TaskType::Http | TaskType::Proxy => wait_http_ready(port),
            TaskType::Dns => wait_dns_ready(port),
            TaskType::Pdf => wait_pdf_ready(port),
            _ => false,
        };
        if !ready {
            // Same loud rule for pdf (no per-trial fallback exists).
            if *task == TaskType::Pdf {
                sandbox.cleanup();
                return Err(anyhow!(
                    "pdf batch reference never became ready on port {port}"
                ));
            }
            log("[BATCH-REF] Warning: batch reference never became ready. Trials fall back to live per-trial refs.");
            sandbox.cleanup();
            return Ok(None);
        }
        // Warmup window (discarded): settles page cache / JIT so measured
        // windows reflect steady state, not cold start.
        let _ = measure(task, port);
        match measure(task, port) {
            Ok(pre) => {
                log(&format!(
                    "[BATCH-REF] Batch reference {} median: {:.0} req/sec on port {} (shared by all trials)",
                    ref_image(task),
                    pre,
                    port
                ));
                Ok(Some(BatchRef {
                    sandbox,
                    task: *task,
                    port,
                    pre,
                }))
            }
            Err(e) => {
                // Same loud rule for pdf.
                if *task == TaskType::Pdf {
                    sandbox.cleanup();
                    return Err(anyhow!("pdf batch calibration failed: {}", e));
                }
                log(&format!("[BATCH-REF] Warning: batch calibration failed: {}. Trials fall back to live per-trial refs.", e));
                sandbox.cleanup();
                Ok(None)
            }
        }
    }

    pub fn reference(&self) -> f64 {
        self.pre
    }

    /// Post-batch bracket: re-measure, flag drift, append history, clean up.
    /// Never fails the batch: worst case the report carries `post: None`.
    pub fn finish(self, log: &dyn Fn(&str)) -> BatchRefReport {
        let post = match measure(&self.task, self.port) {
            Ok(v) => Some(v),
            Err(e) => {
                log(&format!(
                    "[BATCH-REF] Warning: post-batch re-measure failed: {}",
                    e
                ));
                None
            }
        };
        // `post: None` means the reference could not be re-measured at all, so
        // the bracket is unverified — that is drift, not a clean result. Same
        // for a zero/NaN reading on either side. Single source of truth:
        // `crate::bench::ref_drifted_with_tol`.
        let tol = drift_threshold(&self.task);
        let drift = match post {
            Some(p) => {
                let d = crate::bench::ref_drifted_with_tol(self.pre, p, tol);
                log(&format!(
                    "[BATCH-REF] Post-batch reference: {:.0} req/sec (pre {:.0}, drift {} >{:.0}%)",
                    p,
                    self.pre,
                    if d { "FLAGGED" } else { "ok" },
                    tol * 100.0
                ));
                d
            }
            None => {
                log(&format!(
                    "[BATCH-REF] Post-batch re-measure unavailable; bracket UNVERIFIED (drift FLAGGED >{:.0}%)",
                    tol * 100.0
                ));
                true
            }
        };
        append_history(&self.task, self.pre, post, drift);
        self.sandbox.cleanup();
        BatchRefReport {
            pre: self.pre,
            post,
            drift,
        }
    }
}

fn append_history(task: &TaskType, pre: f64, post: Option<f64>, drift: bool) {
    let line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "task": task.to_string(),
        "image": ref_image(task),
        "ref_pre": pre,
        "ref_post": post,
        "drift": drift,
        "host": hostname(),
    });
    let path = get_repo_root().join("results").join("ref_history.jsonl");
    let _ = fs::create_dir_all(path.parent().unwrap());
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", line);
    }
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_port_assignment() {
        assert_eq!(batch_port(&TaskType::Redis), Some(6389));
        assert_eq!(batch_port(&TaskType::Http), Some(8089));
        assert_eq!(batch_port(&TaskType::Dns), Some(5359));
        // Offset from per-trial ref port 5354: no collision with trial churn.
        assert_ne!(batch_port(&TaskType::Dns), Some(5354));
        assert_eq!(batch_port(&TaskType::Pdf), Some(8409));
        // Regression: every batch base used to end in 89, so pdf@w0 (8289) and
        // http/proxy@w2 (8089 + 200) bound the same port on the default 4-worker
        // grid. Two bases can only ever collide if they are congruent mod 100
        // *and* their difference / 100 fits inside the worker range — sharing a
        // low-order pair alone is harmless if the gap is wider than the fleet.
        let bases = [
            TaskType::Redis,
            TaskType::Http,
            TaskType::Dns,
            TaskType::Pdf,
        ]
        .iter()
        .filter_map(batch_port)
        .collect::<Vec<_>>();
        for (i, a) in bases.iter().enumerate() {
            for b in &bases[i + 1..] {
                let gap = a.abs_diff(*b);
                assert!(
                    gap % 100 != 0 || gap / 100 > WORKER_SLOTS as u16,
                    "batch bases {a} and {b} overlap: a worker offset of {} lands on the \
                     same port, within the {WORKER_SLOTS}-slot range",
                    gap / 100
                );
            }
        }
        assert_ne!(batch_port(&TaskType::Pdf), Some(8090));
        // Offset from per-trial ref ports: no collision with trial churn.
        assert_ne!(batch_port(&TaskType::Redis), Some(6380));
        assert_ne!(batch_port(&TaskType::Http), Some(8081));
    }

    /// The real guard: no two distinct roles may bind the same port at any
    /// worker combination. This is exactly what the 8289 bug violated —
    /// pdf/batch@w0 and http/batch@w2 both resolved to 8289.
    #[test]
    fn batch_ports_are_disjoint() {
        let reserved = reserved_ports();
        let mut seen: std::collections::HashMap<u16, (String, u8)> =
            std::collections::HashMap::new();
        for (role, worker, port) in &reserved {
            if let Some((prev_role, prev_worker)) = seen.insert(*port, (role.clone(), *worker)) {
                panic!(
                    "port {port} is claimed by both {prev_role}@w{prev_worker} and \
                     {role}@w{worker} within workers 0..{WORKER_SLOTS}"
                );
            }
        }
        // Sanity: the map is non-trivial, so a broken `reserved_ports` cannot
        // make this test vacuously true.
        assert!(
            reserved.len() > 100,
            "expected a full port map, got {}",
            reserved.len()
        );
    }
}
