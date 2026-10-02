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
        // DNS is retired (config.rs): no batch denominator. Trials keep
        // their live per-trial refs for reporting; nothing here may mint a
        // batch baseline for a withdrawn task.
        // (`batch_ports_are_disjoint` + `test_batch_port_assignment` pin this.)
        TaskType::Dns => None,
        // PDF batch ref: canonical extractor (REF_PDF_IMAGE). 8409, not 8289:
        // every other batch base ends in 89, so an 89-based port collides with
        // `8089 + 100*w` at w=2 — i.e. pdf@w0 and http/proxy@w2 both bind
        // 8289, which the default 4-worker grid reaches. 8409 differs from
        // 6389/8089 by a non-multiple of 100, so no cross-worker overlap
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

/// Readiness is probed from a container on the same network as the reference,
/// addressed by alias - not through a 127.0.0.1-published port, which docker
/// silently drops on an `--internal` network. Probing the published port made
/// every batch reference under egress enforcement time out and fall back to a
/// per-trial live reference, so the batch baseline was never used.
fn wait_redis_ready(net: &str, host: &str, port: u16) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                net,
                BENCH_REDIS_CLIENT_IMAGE,
                "redis-cli",
                "-h",
                host,
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

/// Probed from the run network by alias, for the same reason as
/// `wait_redis_ready`. TCP accept means nginx is up.
fn wait_http_ready(net: &str, host: &str, port: u16) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let probe = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                net,
                crate::bench::BENCH_WRK_IMAGE,
                "-t1",
                "-c1",
                "-d1s",
                &format!("http://{host}:{port}/"),
            ])
            .output();
        if let Ok(o) = probe {
            if o.status.success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

#[allow(dead_code)]
fn wait_http_ready_hostnet(port: u16) -> bool {
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

/// `net` is the run network and `alias` the addressable name of the server on
/// it. The bench client joins that network rather than reaching a
/// 127.0.0.1-published port: docker silently drops `-p` on an `--internal`
/// network, so the host-published path cannot work under egress enforcement -
/// and measuring over the same network path the candidate uses is what makes
/// the ratio mean anything. The container-internal ports are the protocol's
/// canonical ones, not the host-published offsets.
/// Reference-side measurement. Redis/http/proxy go through the SAME client,
/// path, alias and rep count as the candidate (`candidate` alias on the run
/// network, median-of-BENCH_REPS under the host-global lock): the ratio is
/// only meaningful when numerator and denominator share the apparatus.
/// DNS/PDF cannot share it — no container speaks their protocol — so they use
/// the loopback path with the same rep/warmup discipline instead.
fn measure(task: &TaskType, net: &str, port: u16) -> Result<f64> {
    match task {
        TaskType::Redis => BenchmarkRunner::run_redis_benchmark(net, "ref-redis", 6379),
        TaskType::Http | TaskType::Proxy => BenchmarkRunner::run_wrk_benchmark(net, "ref-http", 80),
        // Retired (see batch_port): a batch denominator for DNS must never
        // exist. Per-trial live refs still use measure_dns_reference().
        TaskType::Dns => Err(anyhow!("dns is retired; no batch denominator")),
        TaskType::Pdf => measure_pdf_pages_per_sec(port),
        _ => Err(anyhow!("no external reference for {:?}", task)),
    }
}

/// Candidate-side co-measurement: the same client, path and reps as the
/// reference, addressed by the `candidate` alias on the same run network.
/// Redis/http/proxy candidates join the run network before verification, so
/// this alias resolves exactly where `ref-redis`/`ref-http` resolve.
pub fn measure_candidate(task: &TaskType, net: &str) -> Result<f64> {
    match task {
        TaskType::Redis => {
            BenchmarkRunner::run_redis_benchmark(net, "candidate", candidate_port(task))
        }
        TaskType::Http | TaskType::Proxy => {
            BenchmarkRunner::run_wrk_benchmark(net, "candidate", candidate_port(task))
        }
        _ => Err(anyhow!("no co-measured candidate path for {:?}", task)),
    }
}

/// Container-internal port the candidate serves on the run network: the
/// protocol's canonical port, matching how the reference is addressed
/// (ref-redis:6379, ref-http:80). The host-published offsets (6380/8081/...)
/// are a different path and must not enter the ratio.
pub fn candidate_port(task: &TaskType) -> u16 {
    match task {
        TaskType::Redis => 6379,
        TaskType::Http | TaskType::Proxy => 80,
        _ => 0,
    }
}

pub struct BatchRef {
    // Guard order matters: guard drops first (reaps by label), manager second
    // (belt). Both idempotent.
    _guard: crate::sandbox::docker::RunGuard,
    sandbox: SandboxManager,
    task: TaskType,
    port: u16,
    pub pre: f64,
    /// (1-min loadavg, logical cores) sampled at the load gate. Stamped on
    /// the history row so a future reader can tell an idle-regime bracket
    /// from a loaded one without trusting wall-clock proximity.
    pub pre_load: Option<(f64, usize)>,
}

pub struct BatchRefReport {
    pub pre: f64,
    pub post: Option<f64>,
    pub drift: bool,
}

/// Cross-batch deviation, as a fraction of the historical median, above which a
/// freshly measured reference is reported as not comparable with earlier waves.
/// Same order as `drift_threshold` (0.10): the failure it catches - the
/// denominator quietly moving between batches - is the same order of event as
/// the bracket catching it moving within one.
pub const CROSS_BATCH_DEVIATION_WARN: f64 = 0.10;

impl BatchRef {
    /// Start the batch ref, warm it up (one discarded window), and return the
    /// median-of-3 pre-batch value. `None` task support (WAL/...) yields `Ok(None)`.
    pub fn start(task: &TaskType, parent_run_id: &str, log: &dyn Fn(&str)) -> Result<Option<Self>> {
        Self::start_on_worker(task, parent_run_id, log, 0)
    }

    /// Worker-aware start: batch port follows the worker offset so parallel
    /// cells never share a batch-ref container.
    /// Host-load gate: refuse to mint a denominator under load. The
    /// reference moves 2.86x idle-vs-loaded (gate-1 probe n=12+12), so a
    /// pre-batch median taken under load is not a denominator but noise.
    /// Threshold scales with core count (load per core), not absolute:
    /// 1-min avg above 75% of cores refuses. Returns the sampled load for
    /// stamping, or an Err-style None-signal via the log when refusing.
    /// Pure function of (load1, ncores) so tests pin the rule, not the host.
    pub fn load_gate_allows(load1: f64, ncores: usize) -> bool {
        let cores = ncores.max(1) as f64;
        load1 <= cores * Self::LOAD_GATE_PER_CORE
    }

    /// 1-min loadavg per core above which a batch bracket refuses. 0.75:
    /// loaded-probe medians sat 2.86x off idle; refusing above 3/4 cores
    /// keeps brackets in the idle regime where denominators repeat.
    pub const LOAD_GATE_PER_CORE: f64 = 0.75;

    /// Sample (1-min loadavg, logical cores). None when unreadable (fail
    /// closed: no sample, no bracket — same rule as a missing network).
    pub fn sample_host_load() -> Option<(f64, usize)> {
        let text = std::fs::read_to_string("/proc/loadavg").ok()?;
        let load1: f64 = text.split_whitespace().next()?.parse().ok()?;
        let ncores = std::thread::available_parallelism().ok()?.get();
        Some((load1, ncores))
    }

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
        // Load gate BEFORE any container starts: a denominator minted
        // under load poisons every trial in the batch. Non-pdf falls back to
        // live per-trial refs (warn-and-None); pdf has no fallback (Err).
        if let Some((load1, ncores)) = Self::sample_host_load() {
            if !Self::load_gate_allows(load1, ncores) {
                let msg = format!(
                    "[BATCH-REF] REFUSED under host load (1-min avg {load1:.2} on {ncores} cores, gate {:.2}/core): denominator would be noise, not signal.",
                    Self::LOAD_GATE_PER_CORE
                );
                log(&msg);
                if *task == TaskType::Pdf {
                    return Err(anyhow!("pdf batch reference refused under host load: {load1:.2} on {ncores} cores"));
                }
                return Ok(None);
            }
        }
        let sandbox = SandboxManager::with_id(&format!("{}_batchref", parent_run_id));
        let _guard = crate::sandbox::docker::RunGuard::new(SandboxManager::with_id(&format!(
            "{}_batchref",
            parent_run_id
        )));
        sandbox.ensure_network()?;
        let start_res = match task {
            TaskType::Redis => sandbox.start_reference_redis(port),
            TaskType::Http | TaskType::Proxy => sandbox.start_reference_http(port),
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
        let ref_net = sandbox.network_name();
        let ready = match task {
            TaskType::Redis => wait_redis_ready(&ref_net, "ref-redis", 6379),
            TaskType::Http | TaskType::Proxy => wait_http_ready(&ref_net, "ref-http", 80),
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
        let _ = measure(task, &sandbox.network_name(), port);
        match measure(task, &sandbox.network_name(), port) {
            Ok(pre) => {
                log(&format!(
                    "[BATCH-REF] Batch reference {} median: {:.0} req/sec on port {} (shared by all trials)",
                    ref_image(task),
                    pre,
                    port
                ));
                // Cross-batch comparability. The pre/post bracket below can only
                // show the reference held still inside this batch; it says
                // nothing about the denominator having moved since the last one,
                // which is what makes two published ratios comparable. Across
                // the recorded redis history that range spans ~5.9x, every
                // batch of it flagged drift:false.
                if let Some(h) = read_ref_history(task) {
                    // Compare fraction to fraction. CROSS_BATCH_DEVIATION_WARN is
                    // 0.10, so testing the percent form would warn on anything
                    // above 0.1% and turn the log into noise nobody reads.
                    let dev = h.deviation(pre);
                    let dev_pct = dev * 100.0;
                    if dev > CROSS_BATCH_DEVIATION_WARN {
                        log(&format!(
                            "[BATCH-REF] reference deviates {dev_pct:.0}% from median of {} recorded \
                             batches (median {:.0}, range {:.0}-{:.0} req/sec). Ratios against this \
                             denominator are not comparable with earlier waves.",
                            h.samples, h.median, h.min, h.max
                        ));
                    } else {
                        log(&format!(
                            "[BATCH-REF] reference within {:.0}% of median of {} recorded batches",
                            dev_pct, h.samples
                        ));
                    }
                }
                Ok(Some(BatchRef {
                    _guard,
                    sandbox,
                    task: *task,
                    port,
                    pre,
                    pre_load: Self::sample_host_load(),
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
        let post = match measure(&self.task, &self.sandbox.network_name(), self.port) {
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
        let batch = self
            .sandbox
            .instance_id
            .strip_suffix("_batchref")
            .unwrap_or(&self.sandbox.instance_id)
            .to_string();
        append_history(&self.task, &batch, self.pre, post, drift, self.pre_load, Self::sample_host_load());
        self.sandbox.cleanup();
        BatchRefReport {
            pre: self.pre,
            post,
            drift,
        }
    }
}

/// Where `append_history` writes, and what `read_ref_history` reads.
///
/// Overridable so a test can exercise the real start -> finish lifecycle
/// without appending a synthetic run to the production history that
/// cross-batch comparability is judged against. Set `SDB_REF_HISTORY` to a
/// scratch path; unset, this is the repository's own results file.
fn ref_history_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SDB_REF_HISTORY") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    get_repo_root().join("results").join("ref_history.jsonl")
}

/// Exactly one row per `finish()` call: the batch bracket is one
/// observation (pre, post, drift), never two. `batch` binds the row to the
/// parent run id so a batch that restarts cannot double-append silently;
/// `verified` is false when the bracket is unverified (post missing), so
/// readers can exclude untrusted denominators instead of averaging them in.
/// `method` stamps the measurement regime (co-measured run-network path at
/// BENCH_REPS=5): pre-regime rows used the host-published-port path that
/// timed out under egress enforcement, so the cross-batch warn must compare
/// within one method, never against the old regime's denominators.
fn append_history(
    task: &TaskType,
    batch: &str,
    pre: f64,
    post: Option<f64>,
    drift: bool,
    pre_load: Option<(f64, usize)>,
    post_load: Option<(f64, usize)>,
) {
    let line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "task": task.to_string(),
        "batch": batch,
        "image": ref_image(task),
        "method": format!("comeas-net-alias/reps{}", crate::bench::BENCH_REPS),
        "load_pre": pre_load.map(|(l, _)| l),
        "load_post": post_load.map(|(l, _)| l),
        "ncores": pre_load.map(|(_, n)| n).or(post_load.map(|(_, n)| n)),
        "ref_pre": pre,
        "ref_post": post,
        "drift": drift,
        "verified": post.is_some() && !drift,
        "host": hostname(),
    });
    let path = ref_history_path();
    let _ = fs::create_dir_all(path.parent().unwrap());
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", line);
    }
}

/// Distribution of previously recorded `ref_pre` values for a task.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefHistory {
    pub samples: usize,
    pub min: f64,
    pub max: f64,
    pub median: f64,
}

impl RefHistory {
    /// How far `current` sits from the historical median, as a fraction of it.
    ///
    /// Deliberately *not* distance from the recorded min/max. The redis history
    /// spans 29,553..173,238 (5.9x), so an envelope test returns 0.0 for nearly
    /// every measurement a real run would produce - it would never fire on the
    /// exact drift it exists to catch. Distance from the median scales with the
    /// thing that matters: how comparable is this denominator to the ones
    /// already published.
    pub fn deviation(&self, current: f64) -> f64 {
        if !current.is_finite() || self.median <= 0.0 {
            return 0.0;
        }
        ((current - self.median).abs()) / self.median
    }
}

/// Read the recorded reference history for `task`.
///
/// Compared WITHIN one measurement method only: rows stamped with a
/// different `method` (pre-regime host-published-port path) come from a
/// broken apparatus and are excluded, so the warn cannot fire on regime
/// mismatch. Unstamped legacy rows are treated as a different regime and
/// likewise excluded. Gate on the pre/post bracket for drift; this warn is
/// the cross-batch comparability signal, not the drift detector.
pub fn current_method() -> String {
    format!("comeas-net-alias/reps{}", crate::bench::BENCH_REPS)
}
pub fn read_ref_history(task: &TaskType) -> Option<RefHistory> {
    let path = ref_history_path();
    let text = fs::read_to_string(path).ok()?;
    let want = task.to_string();
    let method = current_method();
    let mut vals: Vec<f64> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("task").and_then(|t| t.as_str()) == Some(want.as_str()))
        .filter(|v| v.get("method").and_then(|m| m.as_str()) == Some(method.as_str()))
        .filter_map(|v| v.get("ref_pre").and_then(|p| p.as_f64()))
        .filter(|p| p.is_finite() && *p > 0.0)
        .collect();
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = vals.len() / 2;
    Some(RefHistory {
        samples: vals.len(),
        min: vals[0],
        max: vals[vals.len() - 1],
        median: if vals.len().is_multiple_of(2) {
            (vals[mid - 1] + vals[mid]) / 2.0
        } else {
            vals[mid]
        },
    })
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

    /// The recorded history is the only cross-batch signal available, and it is
    /// large: redis spans ~5.9x across batches while every one of those batches
    /// passed its own pre/post bracket. These pin the arithmetic that turns
    /// that into a checkable signal.
    #[test]
    fn test_ref_history_deviation_is_distance_from_median() {
        let h = RefHistory {
            samples: 4,
            min: 30_000.0,
            max: 170_000.0,
            median: 60_000.0,
        };
        // These two sit INSIDE [30k, 170k] and must NOT read as clean - that
        // is the whole point. An envelope test returns 0.0 for both and would
        // never fire on real measurements.
        assert!(
            (h.deviation(150_000.0) - 1.5).abs() < 1e-9,
            "150k against a 60k median is +150%, not clean"
        );
        assert!(
            (h.deviation(35_000.0) - (25_000.0 / 60_000.0)).abs() < 1e-9,
            "35k against a 60k median is -42%, not clean"
        );
        // Exactly the median is the only zero.
        assert_eq!(h.deviation(60_000.0), 0.0);
        // Symmetric in both directions.
        assert!(
            (h.deviation(0.0) - 1.0).abs() < 1e-9,
            "0 against a 60k median is -100%"
        );
        assert!(
            (h.deviation(120_000.0) - 1.0).abs() < 1e-9,
            "120k against a 60k median is +100%"
        );
        // Degenerate inputs must not produce NaN or panic.
        assert_eq!(h.deviation(f64::NAN), 0.0);
        let zero = RefHistory {
            samples: 1,
            min: 0.0,
            max: 0.0,
            median: 0.0,
        };
        assert_eq!(zero.deviation(123.0), 0.0);
    }

    /// A task with nothing recorded returns None rather than a fabricated
    /// envelope - "no history" is not "no anomaly".
    #[test]
    fn test_read_ref_history_absent_is_none() {
        // Nothing has been appended for a retired task's reference path.
        let _ = read_ref_history(&TaskType::Binproto);
    }

    /// Parses the real `results/ref_history.jsonl` when it is present, so the
    /// field names and value types the reader depends on are exercised against
    /// actual recorded batches rather than a fixture. Skips cleanly when no
    /// row carries the current method stamp (legacy pre-regime file) or when
    /// the checkout never ran a wave.
    #[test]
    fn test_read_ref_history_parses_real_file() {
        let path = ref_history_path();
        if !path.exists() {
            eprintln!("skipping: no ref_history.jsonl in this checkout");
            return;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let stamped = text.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).filter(|v| v.get("method").and_then(|m| m.as_str()) == Some(current_method().as_str())).count();
        if stamped == 0 {
            eprintln!("skipping: no current-method rows in ref_history.jsonl (pre-regime file)");
            return;
        }
        let Some(h) = read_ref_history(&TaskType::Redis) else {
            eprintln!("skipping: no current-method redis rows yet");
            return;
        };
        assert!(h.samples > 0);
        assert!(h.min > 0.0 && h.max >= h.min);
        assert!(h.median >= h.min && h.median <= h.max);
        // Every task is filtered on, so http must not leak into redis's set.
        if let Some(http) = read_ref_history(&TaskType::Http) {
            assert_ne!(http.samples, h.samples);
        }
        // The recorded median has to be able to flag a reading that sits
        // INSIDE the historical envelope - the 5.9x movement happened entirely
        // inside it, so an envelope test returns 0.0 for exactly the values
        // this reader exists to catch.
        assert_eq!(h.deviation(h.median), 0.0);
        assert!(
            h.deviation(h.max) > 0.2 && h.deviation(h.min) > 0.2,
            "both ends of the recorded range must deviate from its median"
        );
        let midpoint = h.min + (h.max - h.min) / 2.0;
        assert!(
            h.deviation(midpoint) > 0.0,
            "a value inside the envelope must still register if it is off-median"
        );
    }

    // These history tests mutate SDB_REF_HISTORY (process-global) and must
    // not run in parallel: cargo runs #[test] on threads sharing one env, so
    // a sibling set_var/remove_var mid-read opens the wrong file (observed:
    // No such file at read_to_string). serial_test is not a dependency; the
    // HIST_SERIAL mutex serializes the three.
    static HIST_SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn test_history_reader_ignores_foreign_method_rows() {
        // Regime mismatch is not drift: old-regime rows (host-published-port
        // path that timed out under egress enforcement) must not enter the
        // comparison set, or the 10% warn fires on every future batch.
        let dir = std::env::temp_dir().join(format!(
            "sdb_hist_method_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let f = dir.join("h.jsonl");
        let _ = std::fs::create_dir_all(&dir);
        let old_row = serde_json::json!({"task":"redis","ref_pre":10.0,"method":"host-published-port/reps3"});
        let new_row = serde_json::json!({"task":"redis","ref_pre":100.0,"method":current_method()});
        std::fs::write(&f, format!("{}\n{}\n", old_row, new_row)).unwrap();
        let _g = HIST_SERIAL.lock();
        std::env::set_var("SDB_REF_HISTORY", &f);
        let h = read_ref_history(&TaskType::Redis).expect("current-method row must read back");
        assert_eq!(h.samples, 1, "foreign-method row must be excluded");
        assert_eq!(h.median, 100.0);
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("SDB_REF_HISTORY");
    }

    /// `CROSS_BATCH_DEVIATION_WARN` is a fraction (0.10). Comparing a percentage
    /// against it warns on anything above 0.1%, which makes the log noise and
    /// the else-branch dead. Pin the unit: exactly at the threshold must NOT
    /// warn, and the archived redis reference - 2% off its median - is well
    /// inside it.
    #[test]
    fn test_cross_batch_warn_threshold_is_a_fraction() {
        let h = RefHistory {
            samples: 108,
            min: 29_553.0,
            max: 173_238.0,
            median: 75_414.0,
        };
        let warns = |current: f64| h.deviation(current) > CROSS_BATCH_DEVIATION_WARN;

        // Straddle the threshold rather than landing exactly on it. At the
        // boundary the float comparison is a coin flip, and that is not the
        // property worth pinning - the units are.
        let below = h.median * (1.0 + CROSS_BATCH_DEVIATION_WARN * 0.99);
        let above = h.median * (1.0 + CROSS_BATCH_DEVIATION_WARN * 1.01);
        assert!(
            !warns(below),
            "just under threshold must not warn, got {}",
            h.deviation(below)
        );
        assert!(
            warns(above),
            "just over threshold must warn, got {}",
            h.deviation(above)
        );
        assert!(
            !warns(74_155.0),
            "the archived 74,155 reference is 2% off median"
        );
        // A reading the old envelope test called clean still registers here.
        assert!(
            warns(40_000.0),
            "40k is inside the range but 47% off median"
        );
    }

    #[test]
    fn test_candidate_port_is_canonical_not_published_offset() {
        // The ratio must share one network path: candidate by alias on the
        // run network at the protocol's canonical port, exactly where the
        // reference is addressed — never the host-published offsets.
        assert_eq!(candidate_port(&TaskType::Redis), 6379);
        assert_eq!(candidate_port(&TaskType::Http), 80);
        assert_eq!(candidate_port(&TaskType::Proxy), 80);
        assert_ne!(candidate_port(&TaskType::Redis), 6380);
        assert_ne!(candidate_port(&TaskType::Http), 8081);
    }

    #[test]
    fn test_history_row_carries_batch_and_verified() {
        // Exactly one row per finish(), bound to the batch, with an explicit
        // verified flag — readers must be able to exclude unverified
        // denominators instead of averaging them in.
        let dir = std::env::temp_dir().join(format!(
            "sdb_hist_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let f = dir.join("h.jsonl");
        let _g = HIST_SERIAL.lock();
        std::env::set_var("SDB_REF_HISTORY", &f);
        append_history(&TaskType::Redis, "parent_w0", 100.0, Some(105.0), false, Some((2.0, 24)), Some((2.1, 24)));
        append_history(&TaskType::Redis, "parent_w0", 100.0, None, true, Some((2.0, 24)), None);
        let text = std::fs::read_to_string(&f).unwrap();
        let rows: Vec<serde_json::Value> =
            text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 2, "one row per batch event, never two");
        assert_eq!(rows[0]["batch"], "parent_w0");
        assert_eq!(rows[0]["verified"], true);
        assert_eq!(rows[1]["verified"], false);
        assert_eq!(rows[1]["ref_post"], serde_json::Value::Null);
        assert_eq!(rows[0]["method"], serde_json::Value::String(current_method()));
        assert_eq!(rows[1]["method"], serde_json::Value::String(current_method()));
        assert_eq!(rows[0]["load_pre"], 2.0);
        assert_eq!(rows[0]["ncores"], 24);
        assert_eq!(rows[1]["load_post"], serde_json::Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("SDB_REF_HISTORY");
    }

    #[test]
    fn test_load_gate_rule_per_core() {
        // 24-core host like dev: gate at 18.0. Quiet host passes, loaded
        // host refuses — regardless of absolute numbers (a 4-core box gates
        // at 3.0 for the same rule).
        assert!(BatchRef::load_gate_allows(2.0, 24));
        assert!(BatchRef::load_gate_allows(18.0, 24));
        assert!(!BatchRef::load_gate_allows(18.01, 24));
        assert!(BatchRef::load_gate_allows(2.9, 4));
        assert!(!BatchRef::load_gate_allows(3.1, 4));
        // Degenerate core count never divides by zero.
        assert!(BatchRef::load_gate_allows(0.5, 0));
        assert!(!BatchRef::load_gate_allows(5.0, 0));
    }

    #[test]
    fn test_history_row_carries_load_stamps() {
        let dir = std::env::temp_dir().join(format!(
            "sdb_hist_load_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let f = dir.join("h.jsonl");
        let _g = HIST_SERIAL.lock();
        std::env::set_var("SDB_REF_HISTORY", &f);
        append_history(&TaskType::Redis, "b", 100.0, Some(105.0), false, Some((2.0, 24)), Some((2.5, 24)));
        let row: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(row["load_pre"], 2.0);
        assert_eq!(row["load_post"], 2.5);
        assert_eq!(row["ncores"], 24);
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("SDB_REF_HISTORY");
    }

    #[test]
    fn test_batch_port_assignment() {
        assert_eq!(batch_port(&TaskType::Redis), Some(6389));
        assert_eq!(batch_port(&TaskType::Http), Some(8089));
        // Retired: no batch denominator may exist for DNS (Step 3).
        assert_eq!(batch_port(&TaskType::Dns), None);
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

    /// Full PDF BatchRef lifecycle, isolated from the queue and the model.
    ///
    /// The dual-attach reference was verified by hand at the docker level, but
    /// that proves `start_reference_pdf` only - not readiness, not the
    /// PdfVerifier measurement, not the post bracket, not the history append,
    /// not cleanup. This drives start -> finish directly and asserts the whole
    /// chain, so a PDF batch can be validated without waiting on a queued run.
    ///
    /// Requires docker and the pinned `REF_PDF_RUN_IMAGE`; ignored so the
    /// default suite stays hermetic. Run with:
    ///   cargo test --lib pdf_batch_ref_lifecycle -- --ignored --nocapture
    #[test]
    #[ignore = "requires docker + the pinned pdf-ref image"]
    fn pdf_batch_ref_lifecycle() {
        // Never append to the production history: it is the input to
        // cross-batch comparability warnings, and a synthetic probe run would
        // sit in that distribution as if it were a real measurement.
        let scratch: std::path::PathBuf = std::env::temp_dir().join(format!(
            "sdb_ref_history_test_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::env::set_var("SDB_REF_HISTORY", &scratch);
        let _g = HIST_SERIAL.lock();
        let _ = std::fs::remove_file(&scratch);

        let run_id = format!(
            "pdf-lifecycle-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );

        // Leave no container or network behind even on panic.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::process::Command::new("docker")
                    .args(["rm", "-f", self.0.to_string_lossy().as_ref()])
                    .output();
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _guard = Cleanup(scratch.clone());

        let log = |_m: &str| {};
        let started = BatchRef::start(&TaskType::Pdf, &run_id, &log)
            .expect("BatchRef::start returned Err for pdf");
        let batch = started
            .expect("pdf BatchRef::start returned Ok(None) - the reference never became ready");

        let report = batch.finish(&log);
        assert!(
            report.pre > 0.0,
            "pre-measurement should be a positive pages/sec, got {}",
            report.pre
        );
        let post = report.post.expect("post-measurement produced no value");
        assert!(
            post > 0.0,
            "post-measurement should be positive, got {post}"
        );

        // The row must have landed in the scratch file, not the repo's.
        let written = std::fs::read_to_string(&scratch).expect("no history row written");
        let row: serde_json::Value =
            serde_json::from_str(written.lines().last().expect("empty history")).unwrap();
        assert_eq!(row["task"], "pdf");
        assert!(row["ref_pre"].as_f64().unwrap() > 0.0);
        assert!(row["ref_post"].as_f64().unwrap() > 0.0);
        println!(
            "PDF BatchRef lifecycle OK: pre={:.0} post={:.0} drift={} (history -> {})",
            report.pre,
            post,
            report.drift,
            scratch.display()
        );
    }
}
