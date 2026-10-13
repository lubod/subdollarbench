use anyhow::{anyhow, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Pinned reference images (content-addressed). Floating tags let upstream
/// rebuilds silently move the denominator: a digest change is a declared new
/// measurement regime, not silent drift. Re-pin quarterly or on CVE fixes.
pub const REF_REDIS_IMAGE: &str = "docker.io/library/redis@sha256:becdda6c7f4b3fb42e42fd7f120bbf5c54c4caaaf16f26da24e4563d2c1f0576";

/// Egress allowlist for the per-run allowlist forward proxy. The agent NEEDS
/// openrouter (model API) + language registries (builds). github.com is
/// deliberately allowed (frameworks live in git; the tradeoff is documented
/// in methodology). Everything else 403s. Raw sockets bypass proxy env
/// (documented residual); the API key in-container is protected by channel
/// blocking, not by secrecy.
pub const EGRESS_ALLOWLIST: &[&str] = &[
    "openrouter.ai",
    "api.openrouter.ai",
    "crates.io",
    "static.crates.io",
    "proxy.golang.org",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "github.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
];

/// Process-wide mutex serializing Docker network create/prune.
/// Parallel workers creating networks simultaneously trigger pool pressure;
/// a sibling's prune can reap a just-created-but-still-empty network.
static NETWORK_MUTEX: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

fn network_mutex() -> &'static std::sync::Mutex<()> {
    NETWORK_MUTEX.get_or_init(|| std::sync::Mutex::new(()))
}

/// Egress sidecar: python:alpine + stdlib allowlist proxy script (no new image
/// dependency; the `squid` image is absent from the registry mirror).
pub const EGRESS_PROXY_IMAGE: &str = "docker.io/library/python@sha256:2f17fc044b579bab302c2e8054d3a686e2cb9a83de48e70534b94cd8ebbe06a9";

/// Whether this run's egress filter is enabled. `ensure_network` (which owns
/// the run network) and the agent container (which sets the proxy env) both
/// consult this, so the network's isolation and the container's proxy
/// settings can never disagree about whether the run is meant to be closed.
pub fn egress_filter_enabled() -> bool {
    std::env::var("SDB_EGRESS_FILTER")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}
pub const REF_NGINX_IMAGE: &str = "docker.io/library/nginx@sha256:17ad11d84df6c69e327c0894125f712ec1f1de627b5e5ed7e12ca2ed8cd5daf8";

/// Host allowlist check (mirrors extensions/egress_proxy.py).
pub fn egress_allowed(host: &str) -> bool {
    let h = host.to_lowercase();
    let h = h.split(':').next().unwrap_or("").trim();
    if h.is_empty() || h.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    EGRESS_ALLOWLIST
        .iter()
        .any(|a| h == *a || h.ends_with(&format!(".{}", a)))
}
/// PDF reference: canonical extractor (extensions/pdf_reference.py).
/// Pinned by image-ID digest (content-addressed; no registry needed):
/// rebuild with `docker build -t pdf-ref:local .` in a dir containing the
/// Dockerfile + server.py, then `docker inspect pdf-ref:local --format {{.Id}}`.
/// A rebuild that changes behavior yields a new ID -> new regime label by
/// construction (regime_of keys on this string). Record the ID + source hash
/// in wave-freezes.log at each rebuild (REF_INTRO_PDF_* lines).
pub const REF_PDF_IMAGE: &str =
    "pdf-ref@sha256:1a950e870ddbe44aad6415472763d1ec5a00c47f0a672504041fa3379f06422f";
/// Bare image ID that actually runs (verified: `pdf-ref@sha256:<id>`
/// attempts a registry pull and fails locally; bare `sha256:<id>` runs).
/// Rebuild the tag, then `docker inspect pdf-ref:local --format {{.Id}}`.
/// A behavior-changing rebuild MUST update BOTH consts to the new ID
/// (label keeps name@digest shape for regime_of; run keeps bare ID).
/// Verified: identical source rebuilds yield the same ID twice.
pub const REF_PDF_RUN_IMAGE: &str =
    "sha256:1a950e870ddbe44aad6415472763d1ec5a00c47f0a672504041fa3379f06422f";
/// Benchmark *client* images are pinned too: the client binary is half the
/// measurement apparatus.
pub const BENCH_REDIS_CLIENT_IMAGE: &str = REF_REDIS_IMAGE;
pub const BENCH_WRK_IMAGE: &str = "docker.io/williamyeh/wrk@sha256:78adc0d9d51a99e6759e702a08d03eaece81c890ffcc9790ef9e5b199d54f091";
/// Fixed upstream ports for the proxy task (also baked into tasks/proxy/prompt.md).
pub const PROXY_UPSTREAM_PORTS: [u16; 3] = [8101, 8102, 8103];

/// Reference image for a task name ("redis"/"http"/"pdf"), if it has one.
pub fn ref_image_for_task(task: &str) -> Option<&'static str> {
    match task {
        "redis" => Some(REF_REDIS_IMAGE),
        "http" => Some(REF_NGINX_IMAGE),
        "pdf" => Some(REF_PDF_IMAGE),
        _ => None,
    }
}

pub struct SandboxManager {
    pub instance_id: String,
    pub needs_cleanup: AtomicBool,
}

impl Default for SandboxManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxManager {
    pub fn new() -> Self {
        Self::with_id("default")
    }

    pub fn with_id(id: &str) -> Self {
        let safe_id: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        Self {
            instance_id: if safe_id.is_empty() {
                "default".to_string()
            } else {
                safe_id
            },
            needs_cleanup: AtomicBool::new(false),
        }
    }

    pub fn candidate_container_name(&self) -> String {
        format!("subdollar-candidate-{}", self.instance_id)
    }

    pub fn egress_proxy_name(&self) -> String {
        format!("subdollar-egress-{}", self.instance_id)
    }

    /// Start the per-run egress allowlist proxy sidecar on the run network
    /// (alias `egress-proxy`, port 3128). Best-effort: failure only disables
    /// filtering (agent runs unfiltered, logged), never fails the run.
    /// Returns the proxy URL for agent env, or None when unavailable.
    pub fn start_egress_proxy(&self) -> Option<String> {
        let name = self.egress_proxy_name();
        let net = self.network_name();
        if self.ensure_network().is_err() {
            return None;
        }
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &name])
            .output();
        // Sidecar script lives in-repo; mount read-only.
        let script = crate::config::get_repo_root().join("extensions/egress_proxy.py");
        if !script.exists() {
            return None;
        }
        let allow = EGRESS_ALLOWLIST.join(",");
        let out = std::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                &format!("sdb.run={}", self.instance_id),
                "--label",
                "sdb.role=egress",
                "--name",
                &name,
                // Start on the default bridge: the sidecar is the one thing
                // that needs real egress. It joins the run network below,
                // which is created --internal and so has no route out.
                "--network",
                "bridge",
                "--memory=128m",
                "--cpus=0.25",
                "--pids-limit=64",
                "-v",
                &format!("{}:/proxy.py:ro", script.display()),
                "-e",
                &format!("ALLOWLIST={allow}"),
                EGRESS_PROXY_IMAGE,
                "python3",
                "/proxy.py",
                "3128",
            ])
            .output();
        // Disarm Drop-cleanup: this handle is ephemeral (created just to
        // start the sidecar). The sidecar's lifetime belongs to the run,
        // reaped by the run owner's cleanup(). Without this, Drop murders
        // the sidecar seconds after birth and the agent's proxy env dangles.
        self.needs_cleanup.store(false, Ordering::SeqCst);
        match out {
            Ok(o) if o.status.success() => {
                // Save the resolver the bridge gave us. Joining an --internal
                // network rewrites /etc/resolv.conf to an empty file (an
                // internal network's embedded resolver has no upstream), and
                // without a resolver the sidecar cannot match hostnames
                // against the allowlist. Docker permits editing the file.
                let _ = std::process::Command::new("docker")
                    .args(["exec", &name, "sh", "-c", "cp /etc/resolv.conf /tmp/rc.ok"])
                    .output();
                // Now join the run network under the alias the agent resolves.
                let attached = std::process::Command::new("docker")
                    .args(["network", "connect", "--alias", "egress-proxy", &net, &name])
                    .output();
                match attached {
                    Ok(a) if a.status.success() => {}
                    _ => {
                        // Without the alias the agent cannot reach the proxy.
                        // Tear it down instead of leaving it unreachable.
                        self.stop_egress_proxy();
                        return None;
                    }
                }
                let _ = std::process::Command::new("docker")
                    .args(["exec", &name, "sh", "-c", "cp /tmp/rc.ok /etc/resolv.conf"])
                    .output();
                Some("http://egress-proxy:3128".to_string())
            }
            _ => None,
        }
    }

    /// Stop the egress sidecar (cleanup path; missing container is fine).
    pub fn stop_egress_proxy(&self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.egress_proxy_name()])
            .output();
    }

    pub fn ref_redis_name(&self) -> String {
        format!("subdollar-ref-redis-{}", self.instance_id)
    }

    pub fn ref_http_name(&self) -> String {
        format!("subdollar-ref-http-{}", self.instance_id)
    }

    pub fn ref_pdf_name(&self) -> String {
        format!("subdollar-ref-pdf-{}", self.instance_id)
    }

    pub fn network_name(&self) -> String {
        format!("subdollar-net-{}", self.instance_id)
    }

    /// Docker object labels owned by this instance. Every container and
    /// network this manager creates carries `sdb.run=<instance_id>` (+ a
    /// `sdb.role`), so the reaper selects by label filter instead of
    /// name-grep: names collide across parents sharing a timestamp, labels
    /// do not. `sdb.role` is informational (candidate/ref-redis/.../batchref)
    /// except in `reap_run`, which never touches `batchref` from a trial.
    pub fn label_args(&self, role: &str) -> Vec<String> {
        vec![
            "--label".to_string(),
            format!("sdb.run={}", self.instance_id),
            "--label".to_string(),
            format!("sdb.role={}", role),
        ]
    }

    /// Label filter selecting this instance's objects.
    /// `include_batchref` is false for trial owners (BatchRef::finish owns
    /// that share; trial N's exit murdered it twice).
    pub fn label_filter(&self, include_batchref: bool) -> Vec<String> {
        let _ = include_batchref;
        vec![format!("label=sdb.run={}", self.instance_id)]
    }

    /// Idempotently create this run's private Docker network.
    ///
    /// Reference containers join it under stable aliases (`ref-redis`,
    /// `ref-http`) and the OMP agent container is attached to it,
    /// so the agent can probe ground truth by hostname while remaining
    /// isolated from host-network services.
    /// Process-wide mutex serializing Docker network create/prune.
    /// Parallel workers creating networks simultaneously trigger pool pressure;
    /// a sibling's prune can reap a just-created-but-still-empty network (0
    /// containers attached yet looks stale). Setup is seconds-cheap; contention
    /// is harmless, ghost runs are not.
    pub fn ensure_network(&self) -> Result<()> {
        self.needs_cleanup.store(true, Ordering::SeqCst);
        let _guard = network_mutex().lock().unwrap();
        let net = self.network_name();
        for attempt in 0..Self::NETWORK_CREATE_MAX_ATTEMPTS {
            let subnet = Self::run_subnet_attempt(&net, attempt);
            // When the filter is on, this network is the enforcement boundary:
            // the agent has no route off it, so the allowlist proxy sidecar is
            // its only path out. Reference containers keep working - they only
            // ever talk to the agent. With the filter off the network stays
            // NAT'd, because the model API itself needs a route.
            let mut cmd = Command::new("docker");
            cmd.args(["network", "create", "--subnet", &subnet]);
            if egress_filter_enabled() {
                cmd.arg("--internal");
            }
            cmd.args(["--label", &format!("sdb.run={}", self.instance_id)]);
            cmd.args(["--label", "sdb.role=network"]);
            let out = cmd.arg(&net).output()?;
            if out.status.success() {
                info!("Created run network {} ({})", net, subnet);
                Self::verify_network_exists(&net)?;
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if stderr.contains("already exists") {
                return Ok(());
            }
            if Self::is_pool_exhausted(&stderr) {
                warn!("Network pool exhausted creating {} ({}). Reaping own stale networks and retrying...", net, subnet);
                // Reap our own finished networks (same run id or stale
                // gated zombies); never foreign networks. Then retry with a
                // fresh deterministic subnet and re-verify (fail closed).
                self.cleanup_finished_networks();
                Self::prune_stale_networks();
                continue;
            }
            if stderr.contains("is already using subnet")
                || stderr.contains("overlaps with")
                || stderr.contains("Pool overlaps")
            {
                // Deterministic third-octet collision with a foreign
                // network: bump the wheel and retry once per attempt slot.
                continue;
            }
            return Err(anyhow!("docker network create failed: {}", stderr));
        }
        Err(anyhow!(
            "docker network create failed after {} attempts (pool pressure from stale own-networks?); aborting instead of ghost run",
            Self::NETWORK_CREATE_MAX_ATTEMPTS
        ))
    }

    /// Deterministic /24 inside our own 10.201/16 wheel, derived from the
    /// network name hash. Same (net, attempt) => same subnet (attempt 0 is
    /// stable, so idempotent retry hits "already exists"); the per-attempt
    /// salt guarantees a subnet-collision retry actually moves to a fresh
    /// /24 instead of re-hitting the identical lease. Distinct nets spread
    /// across 200 third octets so parallel workers never fight over one
    /// pool lease.
    ///
    /// There is deliberately no attempt-0-only wrapper: `ensure_network`
    /// always calls this with an explicit attempt, so a second entry point
    /// would be an indirection with no caller.
    fn run_subnet_attempt(net: &str, attempt: u32) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        net.hash(&mut h);
        attempt.hash(&mut h);
        let oct = ((h.finish() % Self::RUN_SUBNET_WHEEL) + 1) as u8;
        format!(
            "{}.{}.0/{}",
            Self::RUN_SUBNET_BASE,
            oct,
            Self::RUN_SUBNET_BITS
        )
    }

    /// True for every known daemon wording of "no address space left",
    /// across stock dockerd and distro/LXD builds (LXD's fork rewords the
    /// stock "could not find an available, non-overlapping IPv4 address
    /// pool" string). Case-insensitive; unknown stderr fails open to the
    /// loud error path (never silently treated as pool pressure).
    fn is_pool_exhausted(stderr: &str) -> bool {
        let e = stderr.to_lowercase();
        e.contains("non-overlapping ipv4 address pool")
            || e.contains("available ipv4 address")
            || e.contains("all predefined address pools")
            || e.contains("fully subnetted")
            || e.contains("no available network")
            || e.contains("no free network")
            || e.contains("address pool is exhausted")
            || e.contains("could not allocate subnet")
    }

    /// Reap the finished network owned by THIS run id. Never the batchref
    /// net: while trials run it is live by definition, and `network rm`
    /// on an attached net fails anyway — but on the brief window where the
    /// ref container is restarting it would succeed and murder the bracket.
    /// Sibling live trials keep their networks: attached endpoints fail the
    /// rm, which we tolerate.
    fn cleanup_finished_networks(&self) {
        let _ = Command::new("docker")
            .args([
                "network",
                "rm",
                &format!("subdollar-net-{}", self.instance_id),
            ])
            .output();
    }

    /// Confirm a just-created network is inspectable (retry ×5, 200ms).
    /// Fail closed: a missing network means a concurrent prune raced us;
    /// the caller logs a warning instead of launching into a ghost run.
    fn verify_network_exists(net: &str) -> Result<()> {
        for _ in 0..5 {
            let out = Command::new("docker")
                .args(["network", "inspect", net])
                .output()?;
            if out.status.success() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(anyhow!("docker network {net} not inspectable after create"))
    }

    pub fn start_reference_redis(&self, port: u16) -> Result<()> {
        let name = self.ref_redis_name();
        let net = self.network_name();
        self.ensure_network()?;
        info!(
            "Starting ground-truth reference Redis in Docker on port {} ({}, alias ref-redis on {})",
            port, name, net
        );
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        let _ = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                &format!("sdb.run={}", self.instance_id),
                "--label",
                "sdb.role=ref-redis",
                "--name",
                &name,
                "--network",
                &net,
                "--network-alias",
                "ref-redis",
                "--memory=2g",
                "--cpus=2.0",
                "--pids-limit=256",
                "-p",
                &format!("127.0.0.1:{}:6379", port),
                REF_REDIS_IMAGE,
            ])
            .output();

        Ok(())
    }

    pub fn start_reference_http(&self, port: u16) -> Result<()> {
        let name = self.ref_http_name();
        let net = self.network_name();
        self.ensure_network()?;
        info!(
            "Starting reference Nginx in Docker on port {} ({}, alias ref-http on {})",
            port, name, net
        );
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        let _ = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                &format!("sdb.run={}", self.instance_id),
                "--label",
                "sdb.role=ref-http",
                "--name",
                &name,
                "--network",
                &net,
                "--network-alias",
                "ref-http",
                "--memory=2g",
                "--cpus=2.0",
                "--pids-limit=256",
                "-p",
                &format!("127.0.0.1:{}:80", port),
                REF_NGINX_IMAGE,
            ])
            .output();

        Ok(())
    }

    /// SIGKILL the candidate (crash-consistency tests). Container persists (no --rm).
    pub fn kill_candidate(&self) -> Result<()> {
        let out = Command::new("docker")
            .args(["kill", "-s", "KILL", &self.candidate_container_name()])
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "docker kill failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    }

    /// Restart a killed candidate container.
    pub fn restart_candidate(&self) -> Result<()> {
        let out = Command::new("docker")
            .args(["start", &self.candidate_container_name()])
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "docker start failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    }

    /// Stop and forcibly remove any running candidate container for this instance.
    pub fn stop_candidate(&self) -> Result<()> {
        let name = self.candidate_container_name();
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        Ok(())
    }

    pub fn upstream_name(&self, idx: u8) -> String {
        format!("subdollar-upstream{}-{}", idx, self.instance_id)
    }

    fn upstream_marker_path(&self, idx: u8) -> PathBuf {
        std::env::temp_dir().join(format!(
            "subdollar-upstream-{}-{}.html",
            self.instance_id, idx
        ))
    }

    /// Start nginx upstreams with distinct marker bodies for the proxy task.
    /// Marker files live in tempdir (never in the candidate workspace).
    pub fn start_upstream_pool(&self, ports: &[u16]) -> Result<()> {
        let net = self.network_name();
        self.ensure_network()?;
        for (i, port) in ports.iter().enumerate() {
            let idx = (i + 1) as u8;
            self.start_upstream_on(idx, *port, &net)?;
        }
        Ok(())
    }
    pub fn start_upstream(&self, idx: u8, port: u16) -> Result<()> {
        let net = self.network_name();
        self.ensure_network()?;
        self.start_upstream_on(idx, port, &net)
    }

    /// Marker body served by upstream containers (`upstream-N marker`).
    /// Canonical source shared with the proxy verifier's in-process stubs:
    /// both must serve byte-identical bodies or marker attribution breaks.
    pub fn upstream_marker_body(idx: u8) -> String {
        format!("upstream-{} marker\n", idx)
    }

    fn start_upstream_on(&self, idx: u8, port: u16, net: &str) -> Result<()> {
        let name = self.upstream_name(idx);
        fs::write(
            self.upstream_marker_path(idx),
            Self::upstream_marker_body(idx),
        )?;
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--label",
                &format!("sdb.run={}", self.instance_id),
                "--label",
                &format!("sdb.role=upstream{}", idx),
                "--name",
                &name,
                "--network",
                net,
                "--network-alias",
                &format!("upstream{}", idx),
                "--memory=512m",
                "--cpus=1.0",
                "-p",
                &format!("127.0.0.1:{}:80", port),
                "-v",
                &format!(
                    "{}:/usr/share/nginx/html/index.html:ro",
                    self.upstream_marker_path(idx).display()
                ),
                REF_NGINX_IMAGE,
            ])
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "upstream {} start failed: {}",
                idx,
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    }

    /// Attach the candidate container to the run network so it can dial
    /// run-scoped aliases (proxy upstreams). Candidates launch on the default
    /// bridge (no --network flag), where neither 127.0.0.1-published ports
    /// (own loopback) nor run-net aliases resolve.
    pub fn connect_candidate_to_network(&self) -> Result<()> {
        let out = Command::new("docker")
            .args([
                "network",
                "connect",
                "--alias",
                // The bench client reaches the candidate by this alias on the
                // run network, the same way it reaches `ref-redis`. Measuring
                // both over one network path is what makes the ratio mean
                // anything: the candidate's primary network is bridge, so its
                // 127.0.0.1-published port measures it over a different path
                // than the reference, which starts on the run network.
                "candidate",
                &self.network_name(),
                &self.candidate_container_name(),
            ])
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("already exists") {
                Ok(())
            } else {
                Err(anyhow!("network connect failed: {}", stderr))
            }
        }
    }

    pub fn stop_upstream(&self, idx: u8) -> Result<()> {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.upstream_name(idx)])
            .output()?;
        Ok(())
    }

    /// Build the PDF reference image from the in-repo canonical source.
    /// Tag `pdf-ref:local` first (stable name for debugging), the digest
    /// const governs what actually runs. Fails loudly on build errors.
    fn build_pdf_reference_image() -> Result<()> {
        let script = crate::config::get_repo_root().join("extensions/pdf_reference.py");
        let dir = std::env::temp_dir().join("subdollar-pdfref-build");
        let _ = fs::create_dir_all(&dir);
        fs::copy(&script, dir.join("server.py"))?;
        fs::write(
            dir.join("Dockerfile"),
            "FROM docker.io/library/python@sha256:2f17fc044b579bab302c2e8054d3a686e2cb9a83de48e70534b94cd8ebbe06a9 AS base\nWORKDIR /srv\nCOPY server.py /srv/server.py\nEXPOSE 8090\nCMD [\"python3\", \"/srv/server.py\"]\n",
        )?;
        let out = Command::new("docker")
            .args(["build", "-t", "pdf-ref:local"])
            .arg(&dir)
            .output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "pdf reference image build failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    }

    /// Start the canonical PDF extractor reference (REF_PDF_IMAGE) on the
    /// run network under alias `ref-pdf`, host-published for measurement.
    /// The agent keeps its black-box oracle; batch calibration measures the
    /// same container via the host port.
    pub fn start_reference_pdf(&self, port: u16) -> Result<()> {
        let name = self.ref_pdf_name();
        let net = self.network_name();
        self.ensure_network()?;
        info!(
            "Starting reference PDF extractor in Docker on port {} ({}, alias ref-pdf on {})",
            port, name, net
        );
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        // Dual-attached, and the order matters.
        //
        // Primary network is the default bridge, so the `-p` mapping is created
        // and works: the PDF client is the in-process `PdfVerifier`, which
        // dials 127.0.0.1 from the harness process on the host. It cannot
        // resolve a container-internal alias at all, so unlike redis/http
        // there is no run-network route that would serve it.
        //
        // Then attach the run network under the `ref-pdf` alias, which is how
        // the agent reaches its black-box oracle.
        //
        // Starting on the run network instead would fail outright under egress
        // enforcement: docker silently drops `-p` on an `--internal` network
        // (`docker inspect` reports {"8090/tcp":null}), and attaching a second
        // network afterwards cannot resurrect an already-null port binding.
        let run_args = [
            "run",
            "-d",
            "--rm",
            "--label",
            &format!("sdb.run={}", self.instance_id),
            "--label",
            "sdb.role=ref-pdf",
            "--name",
            &name,
            "--network",
            "bridge",
            "--memory=512m",
            "--cpus=1.0",
            "-p",
            &format!("127.0.0.1:{port}:8090"),
            REF_PDF_RUN_IMAGE,
        ];
        let out = Command::new("docker").args(run_args).output()?;
        if out.status.success() {
            let attach = Command::new("docker")
                .args(["network", "connect", "--alias", "ref-pdf", &net, &name])
                .output()?;
            if !attach.status.success() {
                let _ = Command::new("docker").args(["rm", "-f", &name]).output();
                return Err(anyhow!(
                    "pdf reference started but could not join the run network {}: {}",
                    net,
                    String::from_utf8_lossy(&attach.stderr)
                ));
            }
        }
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            // Fresh-checkout bootstrap: image missing -> build from the
            // in-repo source, then FAIL LOUDLY (no retry with the pinned
            // digest: fresh builds get new config timestamps -> new ID, so
            // the retry could never succeed). The operator updates both
            // REF_PDF_* consts to the reported ID and rebuilds.
            // Fails LOUDLY: batch_ref propagates pdf errors (no silent
            // unscored fallback; see start_on_worker pdf arm).
            if stderr.contains("No such image") || stderr.contains("not found") {
                Self::build_pdf_reference_image()?;
                let id = Command::new("docker")
                    .args(["inspect", "pdf-ref:local", "--format", "{{.Id}}"])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                return Err(anyhow!(
                    "pdf reference image missing; rebuilt pdf-ref:local as {} — update REF_PDF_IMAGE/REF_PDF_RUN_IMAGE consts to this ID and rebuild the binary",
                    id
                ));
            }
            return Err(anyhow!("pdf reference start failed: {}", stderr));
        }
        Ok(())
    }

    fn find_go_main_recursive(dir: &Path) -> Option<PathBuf> {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("go") {
                    if let Ok(content) = fs::read_to_string(&p) {
                        if content.contains("package main") && content.contains("func main()") {
                            return Some(p);
                        }
                    }
                } else if p.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.')
                        && name != "target"
                        && name != "node_modules"
                        && name != "vendor"
                    {
                        if let Some(found) = Self::find_go_main_recursive(&p) {
                            return Some(found);
                        }
                    }
                }
            }
        }
        None
    }

    /// Recursively find a file by name within directory
    fn find_file_recursive(dir: &Path, filename: &str) -> Option<PathBuf> {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some(filename) {
                    return Some(path);
                } else if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with('.') || name == "target" || name == "node_modules" {
                            continue;
                        }
                    }
                    if let Some(found) = Self::find_file_recursive(&path, filename) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }

    /// Recursively check if directory contains a runnable candidate.
    /// If found in a subdirectory, creates a forwarding root start.sh.
    pub fn ensure_runnable_candidate(&self, workdir: &Path) -> Result<bool> {
        if !workdir.exists() {
            return Ok(false);
        }

        let root_dockerfile = workdir.join("Dockerfile");
        if root_dockerfile.exists() {
            return Ok(true);
        }

        let root_start_sh = workdir.join("start.sh");
        if root_start_sh.exists() {
            return Ok(true);
        }

        // Search for Dockerfile in subdirectories
        if let Some(found_df) = Self::find_file_recursive(workdir, "Dockerfile") {
            if found_df != root_dockerfile {
                info!(
                    "Found nested Dockerfile at {:?}. Hoisting to root workspace...",
                    found_df
                );
                let _ = fs::copy(&found_df, &root_dockerfile);
                return Ok(true);
            }
        }

        // Search for start.sh in subdirectories
        if let Some(found_sh) = Self::find_file_recursive(workdir, "start.sh") {
            if found_sh != root_start_sh {
                info!(
                    "Found nested start.sh at {:?}. Creating root bridge start.sh...",
                    found_sh
                );
                if let Ok(rel_path) = found_sh.strip_prefix(workdir) {
                    let parent_dir = rel_path.parent().unwrap_or(Path::new(""));
                    let script_name = rel_path.file_name().unwrap_or_default().to_string_lossy();
                    let bridge_content = format!(
                        "#!/bin/bash\ncd /workspace/{}\nchmod +x ./{}\nexec ./{}\n",
                        parent_dir.display(),
                        script_name,
                        script_name
                    );
                    let _ = fs::write(&root_start_sh, bridge_content);
                    return Ok(true);
                }
            }
        }

        // Search for any Go file with 'package main' and 'func main()'
        if let Some(main_go) = Self::find_go_main_recursive(workdir) {
            if let Ok(rel) = main_go.strip_prefix(workdir) {
                let parent = rel.parent().unwrap_or(Path::new(""));
                let cd_part = if parent.as_os_str().is_empty() {
                    "".to_string()
                } else {
                    format!("cd /workspace/{}\n", parent.display())
                };
                let bridge = format!("#!/bin/bash\n{}exec go run .\n", cd_part);
                info!(
                    "Found Go main at {:?}. Synthesizing root start.sh...",
                    main_go
                );
                let _ = fs::write(&root_start_sh, bridge);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&root_start_sh, fs::Permissions::from_mode(0o755));
                }
                return Ok(true);
            }
        }

        // Auto-detect common language entrypoints and create default start.sh
        let entrypoint_patterns: &[(&str, &str)] = &[
            ("main.py", "#!/bin/bash\nexec python3 main.py\n"),
            ("app.py", "#!/bin/bash\nexec python3 app.py\n"),
            ("server.py", "#!/bin/bash\nexec python3 server.py\n"),
            ("main.go", "#!/bin/bash\nexec go run main.go\n"),
            ("Cargo.toml", "#!/bin/bash\nexec cargo run --release\n"),
            ("index.js", "#!/bin/bash\nexec node index.js\n"),
            ("server.js", "#!/bin/bash\nexec node server.js\n"),
            ("main.js", "#!/bin/bash\nexec node main.js\n"),
        ];

        for (entry_file, start_content) in entrypoint_patterns {
            if workdir.join(entry_file).exists() {
                info!(
                    "Auto-detected entrypoint '{}'. Creating default start.sh...",
                    entry_file
                );
                let _ = fs::write(&root_start_sh, start_content);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&root_start_sh, fs::Permissions::from_mode(0o755));
                }
                return Ok(true);
            }
            if let Some(found) = Self::find_file_recursive(workdir, entry_file) {
                if let Ok(rel_path) = found.strip_prefix(workdir) {
                    let parent_dir = rel_path.parent().unwrap_or(Path::new(""));
                    let file_name = rel_path.file_name().unwrap_or_default().to_string_lossy();
                    let cd_part = if parent_dir.as_os_str().is_empty() {
                        "".to_string()
                    } else {
                        format!("cd /workspace/{}\n", parent_dir.display())
                    };
                    let run_cmd = match file_name.as_ref() {
                        "Cargo.toml" => "exec cargo run --release\n",
                        f if f.ends_with(".py") => "exec python3 main.py\n",
                        f if f.ends_with(".go") => "exec go run main.go\n",
                        f if f.ends_with(".js") => "exec node main.js\n",
                        _ => "exec ./start.sh\n",
                    };
                    let bridge = format!("#!/bin/bash\n{}{}", cd_part, run_cmd);
                    info!(
                        "Found nested entrypoint '{}' at {:?}. Creating root start.sh...",
                        entry_file, found
                    );
                    let _ = fs::write(&root_start_sh, bridge);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            fs::set_permissions(&root_start_sh, fs::Permissions::from_mode(0o755));
                    }
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// Start the candidate server container with resource limits
    /// Candidate containers run as arbitrary users (root, redis,
    /// app-specific). The workspace is host-ubuntu-owned 644/755, so any
    /// non-root candidate user gets EACCES on its own data files (measured:
    /// `appendonly.log: permission denied` ×3 across 2 parents). Loosen
    /// before launch: OR the permission bits (never strip), so executables
    /// keep +x (a forced 0666 broke `./sdb-redis` exec) while every UID
    /// gains rwX. The candidate is fully isolated, so this grants nothing
    /// beyond its sandbox. Symlinks skipped (never followed).
    fn relax_workspace_perms(workdir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![workdir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(ft) = entry.file_type() {
                    if ft.is_symlink() {
                        continue;
                    }
                    if ft.is_dir() {
                        let mode = path
                            .metadata()
                            .map(|m| {
                                use std::os::unix::fs::PermissionsExt;
                                m.permissions().mode() | 0o777
                            })
                            .unwrap_or(0o777);
                        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));
                        stack.push(path);
                    } else {
                        let mode = path
                            .metadata()
                            .map(|m| {
                                use std::os::unix::fs::PermissionsExt;
                                m.permissions().mode() | 0o666
                            })
                            .unwrap_or(0o666);
                        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));
                    }
                }
            }
        }
        let mode = workdir
            .metadata()
            .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() | 0o777
            })
            .unwrap_or(0o777);
        let _ = fs::set_permissions(workdir, fs::Permissions::from_mode(mode));
    }

    pub fn start_candidate_in_docker(&self, workdir: &Path, port: u16) -> Result<()> {
        Self::relax_workspace_perms(workdir);
        let name = self.candidate_container_name();
        info!(
            "Starting candidate server in isolated Docker container for port {} ({})",
            port, name
        );

        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        let _ = fs::create_dir_all(workdir);

        let canonical_workdir = if workdir.is_absolute() {
            workdir.to_path_buf()
        } else {
            workdir.canonicalize().unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|c| c.join(workdir))
                    .unwrap_or_else(|_| crate::config::get_repo_root().join(workdir))
            })
        };

        // Loopback-only publish: candidates are probed from the host, never
        // from the LAN. Docker still routes container->host via the bridge.
        let port_tcp = format!("127.0.0.1:{}:{}", port, port);
        let port_udp = format!("127.0.0.1:{}:{}/udp", port, port);

        // CASE A: Candidate provided their own Dockerfile
        let dockerfile = canonical_workdir.join("Dockerfile");
        if dockerfile.exists() {
            info!("Candidate provided custom Dockerfile. Building image...");
            let image_tag = format!("subdollar-candidate-{}", self.instance_id);
            let build_res = Command::new("timeout")
                .args(["120", "docker", "build", "-t", &image_tag, "."])
                .current_dir(&canonical_workdir)
                .output();

            match build_res {
                Ok(out) if out.status.success() => {
                    info!("Custom Dockerfile build succeeded. Running container...");
                    let run_res = Command::new("docker")
                        .args([
                            "run",
                            "-d",
                            "--label",
                            &format!("sdb.run={}", self.instance_id),
                            "--label",
                            "sdb.role=candidate",
                            "--name",
                            &name,
                            "--memory=2g",
                            "--cpus=2.0",
                            "--pids-limit=256",
                            "-p",
                            &port_tcp,
                            "-p",
                            &port_udp,
                            &image_tag,
                        ])
                        .output();
                    match run_res {
                        Ok(run_out) if run_out.status.success() => {}
                        Ok(run_out) => {
                            // Was warn + Ok(()): swallowed launch faults made
                            // the caller record a model 0% instead of an
                            // infra-error (indistinguishable verdicts).
                            return Err(anyhow!(
                                "candidate container failed to start: {}",
                                String::from_utf8_lossy(&run_out.stderr)
                            ));
                        }
                        Err(e) => {
                            return Err(anyhow!("candidate container spawn failed: {e}"));
                        }
                    }
                    return Ok(());
                }
                Ok(out) => {
                    warn!(
                        "Candidate Dockerfile build failed: {}. Falling back to start.sh...",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to execute docker build: {}. Falling back to start.sh...",
                        e
                    );
                }
            }
        }

        // CASE B: Standard Sandbox with start.sh
        let mount_arg = format!("{}:/workspace", canonical_workdir.display());
        let launch_cmd = "cd /workspace && if [ -f start.sh ]; then chmod +x start.sh && exec ./start.sh; elif [ -f main.py ]; then exec python3 main.py; elif [ -f app.py ]; then exec python3 app.py; elif [ -f server.py ]; then exec python3 server.py; elif [ -f main.go ]; then exec go run main.go; elif [ -f Cargo.toml ]; then exec cargo run --release; elif [ -f main.js ]; then exec node main.js; elif [ -f index.js ]; then exec node index.js; else chmod +x start.sh 2>/dev/null && exec ./start.sh; fi";
        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--label",
                &format!("sdb.run={}", self.instance_id),
                "--label",
                "sdb.role=candidate",
                "--name",
                &name,
                "--memory=2g",
                "--cpus=2.0",
                "--pids-limit=256",
                "-v",
                &mount_arg,
                "-p",
                &port_tcp,
                "-p",
                &port_udp,
                "subdollar-sandbox",
                "/bin/bash",
                "-c",
                launch_cmd,
            ])
            .output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "candidate sandbox failed to start: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }

        Ok(())
    }

    /// Actively poll the target TCP port until the candidate is ready, up to timeout_secs
    pub async fn wait_for_port(&self, port: u16, timeout_secs: u64) -> bool {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);
        while start.elapsed() < timeout {
            if let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                // When published via Docker, docker-proxy immediately accepts the TCP handshake on
                // 127.0.0.1 even if the service inside the container is not yet listening (e.g. compiling).
                // If the container service is not listening, docker-proxy immediately resets or closes the connection.
                // We sleep 80ms and perform a try_read:
                // - Err(WouldBlock): Socket is open and backend is listening waiting for request -> Ready!
                // - Ok(n) if n > 0: Backend sent a banner/greeting -> Ready!
                // - Ok(0) or Err(_): Backend is not listening; docker-proxy closed/reset -> Not ready yet.
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                let mut buf = [0u8; 1];
                match stream.try_read(&mut buf) {
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        return true;
                    }
                    Ok(n) if n > 0 => {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        return true;
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        false
    }

    pub fn get_candidate_logs(&self) -> String {
        let name = self.candidate_container_name();
        let output = Command::new("docker")
            .args(["logs", "--tail", "100", &name])
            .output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let mut logs = String::new();
                if !stdout.is_empty() {
                    logs.push_str(&format!("--- Container stdout ---\n{}\n", stdout));
                }
                if !stderr.is_empty() {
                    logs.push_str(&format!("--- Container stderr ---\n{}\n", stderr));
                }
                if logs.is_empty() {
                    logs.push_str("(No output logged by container)");
                }
                logs
            }
            Err(e) => format!("Failed to read container logs: {}", e),
        }
    }

    pub fn agent_container_name(&self) -> String {
        let clean = self.instance_id.replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
            "",
        );
        format!("subdollar-omp-agent-{}", clean)
    }

    pub fn cleanup_instance(&self) {
        self.cleanup();
    }

    pub fn remove_network_retrying(net: &str) {
        for _ in 0..5 {
            let res = Command::new("docker").args(["network", "rm", net]).output();
            if let Ok(out) = res {
                if out.status.success() {
                    return;
                }
                let stderr = String::from_utf8_lossy(&out.stderr);
                if stderr.contains("No such network") || stderr.contains("not found") {
                    return;
                }
                if stderr.contains("active endpoints") {
                    let _ = Command::new("sh")
                        .args([
                            "-c",
                            &format!(
                                "docker network inspect {} --format '{{{{range .Containers}}}}{{{{.Name}}}} {{{{end}}}}' | xargs -r -n1 docker network disconnect -f {}",
                                net, net
                            ),
                        ])
                        .output();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Remove stale `subdollar-net-*` networks left behind by crashed runs.
    ///
    /// NEVER removes a network with live/starting containers attached: each
    /// candidate is inspected first and skipped unless its container count is
    /// exactly zero. `docker network rm` itself also refuses networks with
    /// active endpoints, so a container attaching between our inspect and rm
    /// still fails safe (rm errors, network survives). Unparseable inspect
    /// output fails closed (skip) for the same reason.
    ///
    /// Background: an unconditional `ls -q | xargs docker network rm` raced
    /// pool-exhaustion retries against concurrently starting runs, deleting a
    /// just-created but not-yet-attached network (infra-failure race).
    /// Networks younger than this are "not yet attached", never stale.
    /// Parallel workers create-then-attach; a 0-container seconds-old network
    /// must survive a sibling's prune.
    pub const NETWORK_STALE_AGE_SECS: i64 = 300;

    /// Subnets for this run's own networks. `docker network create` without
    /// `--subnet` draws from the daemon's default pools; stale gated
    /// subdollar zombie networks hold leases there, so pool exhaustion is
    /// only fixable by reaping OUR OWN old networks (never foreign ones).
    pub const RUN_SUBNET_BASE: &str = "10.201";
    pub const RUN_SUBNET_BITS: u8 = 24;
    /// Max networks per /24 before the third-octet wheel wraps.
    pub const RUN_SUBNET_WHEEL: u64 = 200;
    /// Cap on consecutive failed-create-then-abort cycles before the caller
    /// fails loudly instead of launching a ghost run without a network.
    pub const NETWORK_CREATE_MAX_ATTEMPTS: u32 = 3;

    /// True when the network has attached containers or is younger than the
    /// staleness threshold (or inspect fails). Cleanup skips such networks.
    /// Takes &str so Drop/cleanup paths share the prune gate logic.
    pub fn network_in_use_or_young(net: &str) -> bool {
        let out = std::process::Command::new("docker")
            .args([
                "network",
                "inspect",
                net,
                "--format",
                "{{len .Containers}} {{.Created}}",
            ])
            .output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return true, // fail closed: unknown state => keep.
        };
        !Self::network_is_stale(&text)
    }

    pub fn prune_stale_networks() {
        let ls = Command::new("docker")
            .args(["network", "ls", "--filter", "name=subdollar-net-", "-q"])
            .output();
        let ids = match ls {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return,
        };
        for id in Self::parse_network_ids(&ids) {
            let inspect = Command::new("docker")
                .args([
                    "network",
                    "inspect",
                    id,
                    "--format",
                    "{{len .Containers}} {{.Created}}",
                ])
                .output();
            let count_out = match inspect {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => continue, // Inspect failed: fail closed, skip the network.
            };
            if !Self::network_is_stale(&count_out) {
                continue;
            }
            let _ = Command::new("docker").args(["network", "rm", id]).output();
        }
    }

    /// Parse `docker network ls -q` output into non-empty network IDs.
    fn parse_network_ids(output: &str) -> Vec<&str> {
        output.split_whitespace().collect()
    }

    /// True only when the inspect line shows 0 containers AND a Created
    /// timestamp older than NETWORK_STALE_AGE_SECS. Anything else (attached,
    /// young, or unparseable) means "do not touch".
    fn network_is_stale(inspect_output: &str) -> bool {
        let mut parts = inspect_output.split_whitespace();
        let count: u64 = match parts.next().and_then(|t| t.parse().ok()) {
            Some(c) => c,
            None => return false,
        };
        if count != 0 {
            return false;
        }
        // Remaining tokens form the Created timestamp ("2026-09-09 05:22:07.063 +0000 UTC").
        let created = parts.collect::<Vec<_>>().join(" ");
        // Parse leniently: compare lexicographically against (now - age) in the
        // same layout. Unparseable -> fail closed (not stale).
        let age_boundary = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(
                Self::NETWORK_STALE_AGE_SECS as u64,
            ))
            .and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64)
                // Format back via chrono if available, else compare raw.
            });
        // Simplest robust check: ask docker for Created seconds directly is an
        // extra call; instead parse the leading date "YYYY-MM-DD HH:MM:SS".
        let ts = match chrono::NaiveDateTime::parse_from_str(
            created.get(..19).unwrap_or(""),
            "%Y-%m-%d %H:%M:%S",
        ) {
            Ok(t) => t.and_utc().timestamp(),
            Err(_) => return false,
        };
        match age_boundary {
            Some(b) => ts < b,
            None => false,
        }
    }

    /// Containers this instance may own: exactly its own. BatchRef uses
    /// `<parent>_batchref` while trials use `<parent>_trial<N>`/`<parent>`,
    /// and trials must NEVER reap the batchref share: the batch bracket
    /// (pre ... post) outlives every trial, so trial N's normal exit
    /// murdered the denominator trial N+1 still needed (live: post-batch
    /// re-measure failed with "network ..._batchref not found" on both gate
    /// cells). BatchRef::finish() owns its own cleanup; the batchref sandbox
    /// drops via its own Drop. Trial cleanup keeps own containers, own
    /// network, files — nothing else.
    fn owned_container_names(&self) -> Vec<String> {
        let mut names = vec![
            self.candidate_container_name(),
            self.ref_redis_name(),
            self.ref_http_name(),
            self.ref_pdf_name(),
            self.egress_proxy_name(),
            self.agent_container_name(),
            format!("subdollar-omp-agent-{}", std::process::id()),
        ];
        for idx in 1..=8u8 {
            names.push(self.upstream_name(idx));
        }
        names
    }

    /// Reap every container carrying this instance's label, except the
    /// batchref share when this is a trial id. `docker rm -f` on an explicit
    /// id list (no shell, no grep): names cannot collide across parents
    /// sharing a timestamp because selection is by label, not by pattern.
    /// Networks: only this instance's own (batchref excluded for trials).
    pub fn reap_run(&self) {
        let filter = format!("label=sdb.run={}", self.instance_id);
        let list = Command::new("docker")
            .args(["ps", "-a", "-q", "--filter", &filter])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        // No batchref exclusion needed here: batchref containers carry
        // `sdb.run=<parent>_batchref`, a different label value than this
        // trial's `sdb.run=<parent>_trialN`, so the label filter above can
        // never return them. (The old name-grep needed the exclusion because
        // `<parent>` is a substring of both; labels do not have that flaw —
        // this is the point of the migration.)
        let ids: Vec<&str> = list.split_whitespace().collect();
        if !ids.is_empty() {
            let mut args = vec!["rm", "-f"];
            args.extend(ids);
            let _ = Command::new("docker").args(&args).output();
        }
        // Own network only when it holds zero containers (same rule as
        // the guard: never force-disconnect a live endpoint).
        // Batchref nets belong to BatchRef::finish, never to a trial.
        let net = self.network_name();
        if !Self::network_in_use_or_young(&net) {
            Self::remove_network_retrying(&net);
        }
    }

    /// Label-filtered network prune for stale runs: only networks carrying
    /// an sdb.run label AND zero containers AND older than the staleness
    /// threshold. Replaces name-pattern sweeps that reaped live siblings.
    pub fn prune_labeled_networks() {
        let ls = Command::new("docker")
            .args([
                "network",
                "ls",
                "--filter",
                "label=sdb.run",
                "--format",
                "{{.ID}}",
            ])
            .output();
        let ids = match ls {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return,
        };
        for id in Self::parse_network_ids(&ids) {
            let detail = Command::new("docker")
                .args([
                    "network",
                    "inspect",
                    id,
                    "--format",
                    "{{len .Containers}} {{.Created}}",
                ])
                .output();
            let text = match detail {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => continue,
            };
            if !Self::network_is_stale(&text) {
                continue;
            }
            let _ = Command::new("docker").args(["network", "rm", id]).output();
        }
    }

    /// Reap orphan *containers*: labeled containers whose network is gone
    /// (or which outlived their run: crashed engine, killed process, test
    /// scaffolding without Drop). Only containers whose sdb.run network no
    /// Running containers older than this whose run matches no live cell
    /// are abandoned (engine restarts orphan them while their network
    /// survives, deadlocking the old stopped&&net_gone rule: 140 leaked).
    /// Below the age they are left alone even when unmatched (kill -9 crash
    /// windows, slow starts).
    pub const MAX_RUNNING_ORPHAN_AGE_SECS: u64 = 3 * 3600;

    /// True when a container label belongs to a live run: stems match after
    /// stripping trial/batchref/extend suffixes (`..._w0` stems
    /// `..._w0_trial1` and `..._w0_batchref`), or either side prefixes the
    /// other as a fallback.
    pub fn run_matches_live(label: &str, live_ids: &std::collections::HashSet<String>) -> bool {
        fn stem(s: &str) -> &str {
            let mut s = s;
            loop {
                let cut = s
                    .rfind("_trial")
                    .filter(|&i| s[i + 6..].chars().all(|c| c.is_ascii_digit()))
                    .or_else(|| s.rfind("_batchref").filter(|&i| i + 9 == s.len()))
                    .or_else(|| {
                        s.rfind("_ext")
                            .filter(|&i| s[i + 4..].chars().all(|c| c.is_ascii_digit()))
                    });
                match cut {
                    Some(i) => s = &s[..i],
                    None => return s,
                }
            }
        }
        let ls = stem(label);
        live_ids.iter().any(|id| {
            let s = stem(id);
            ls == s || ls.starts_with(s) || s.starts_with(ls)
        })
    }

    /// longer exists AND which are not running a live trial: selected by
    /// label, filtered to subdollar-* names, never by bare timestamp.
    /// Returns the reaped count. Fail-closed: any inspect error keeps the
    /// container (a live cell's container always has its network present).
    pub fn reap_orphan_containers() -> usize {
        Self::reap_orphan_containers_except(&std::collections::HashSet::new())
    }

    /// Same as [`Self::reap_orphan_containers`], plus an age gate for
    /// RUNNING orphans: running past MAX_RUNNING_ORPHAN_AGE_SECS with no
    /// live match is abandoned, not busy — force-remove so its network can
    /// prune next. Anything live, young, stopped-with-network, or
    /// unparseable is kept.
    pub fn reap_orphan_containers_except(live_ids: &std::collections::HashSet<String>) -> usize {
        let ls = Command::new("docker")
            .args(["ps", "-a", "-q", "--filter", "label=sdb.run"])
            .output();
        let ids = match ls {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return 0,
        };
        let mut reaped = 0;
        for id in Self::parse_network_ids(&ids) {
            let detail = Command::new("docker")
                .args([
                    "inspect",
                    id,
                    "--format",
                    "{{.Name}} {{.State.Running}} {{index .Config.Labels \"sdb.run\"}} {{.Created}}",
                ])
                .output();
            let text = match detail {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => continue,
            };
            let parts: Vec<&str> = text.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }
            let (name, running, run) = (parts[0], parts[1], parts[2]);
            let created = parts.get(3).copied().unwrap_or("");
            // Only our names; never foreign containers that happen to share
            // a label key.
            if !name.contains("subdollar-") && !name.contains("guardprobe-") {
                continue;
            }
            // Orphan = stopped AND its network is gone. A legitimately
            // stopped candidate (kill -9 crash window before restart) keeps
            // its network present, so AND keeps the sweep out of live
            // trials. `stopped || net_gone` murdered mid-trial candidates
            // in their kill window (same class as the batchref murders).
            // kill_candidate containers persist (no --rm) by design.
            let net_gone = Command::new("docker")
                .args(["network", "inspect", &format!("subdollar-net-{run}")])
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(false);
            let stopped = running != "true";
            if stopped && net_gone {
                let rm = Command::new("docker").args(["rm", "-f", id]).output();
                if rm.map(|o| o.status.success()).unwrap_or(false) {
                    reaped += 1;
                }
                continue;
            }
            // Running but ancient and unmatched: abandoned by a dead engine,
            // never coming back. Live matches always survive regardless of age
            // (a 9-trial cell legitimately runs for hours).
            if !stopped && !Self::run_matches_live(run, live_ids) {
                let age_ok = Self::container_created_secs(created).is_some_and(|ts| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|now| {
                            now.as_secs().saturating_sub(ts) > Self::MAX_RUNNING_ORPHAN_AGE_SECS
                        })
                        .unwrap_or(false)
                });
                if age_ok {
                    let rm = Command::new("docker").args(["rm", "-f", id]).output();
                    if rm.map(|o| o.status.success()).unwrap_or(false) {
                        reaped += 1;
                    }
                }
            }
        }
        reaped
    }

    /// Parse `docker inspect .Created` (RFC3339) to epoch seconds.
    /// None when unparseable (fail closed: never stale).
    pub fn container_created_secs(created: &str) -> Option<u64> {
        chrono::DateTime::parse_from_rfc3339(created)
            .ok()
            .map(|dt| dt.timestamp() as u64)
    }

    /// True when a candidate-launch failure is the model's packaging
    /// fault, not harness infrastructure: bad entrypoint paths, missing
    /// +x, image-internal permission or layout errors. Such trials are
    /// `failed_tests` (0%), never `infra_error` — the model built
    /// something unrunnable. Daemon-side failures (socket, bridge, disk
    /// pressure, timeouts) stay infra. Matching is substring-based on the
    /// docker CLI stderr, fail-closed toward infra on anything unrecognized.
    pub fn is_model_packaging_error(err: &str) -> bool {
        let l = err.to_lowercase();
        // NOTE: no bare "not found" — daemon "network ... not found" is
        // infra, while "no such file" paths are entrypoint faults.
        l.contains("no such file or directory")
            || l.contains("is a directory")
            || l.contains("permission denied")
            || l.contains("executable file not found")
            || l.contains("exec format error")
    }

    /// Legacy path, kept for Drop + tests. New code prefers RunGuard
    /// (label-filtered, panic-safe). Idempotent: safe to call twice.
    pub fn cleanup(&self) {
        self.needs_cleanup.store(false, Ordering::SeqCst);
        // Label-filtered reaper (no shell, no name-grep): selects by
        // `sdb.run=<instance_id>`. Covers legacy unlabelled names via
        // owned-rm belt below.
        self.reap_run();
        // Belt: legacy containers created before labels existed carry no
        // label; remove by exact owned names.
        let owned = self.owned_container_names();
        if !owned.is_empty() {
            let owned_refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
            let _ = Command::new("docker")
                .args([["rm", "-f"].as_slice(), owned_refs.as_slice()].concat())
                .output();
        }
        let net = self.network_name();
        // Never reap a live or newborn network: attached containers OR age
        // under the staleness threshold mean a sibling worker is
        // creating-then-attaching (parallel ghost-run race). remove_network
        // refuses attached endpoints anyway; the age check covers the
        // not-yet-attached window.
        // File cleanup is unconditional: the gated network rm above must
        // never swallow these (a kept live/young network still leaves
        // stale upstream files otherwise).
        for idx in 1..=8u8 {
            let _ = fs::remove_file(self.upstream_marker_path(idx));
        }
        if !Self::network_in_use_or_young(&net) {
            Self::remove_network_retrying(&net);
        }
    }
}

/// RAII scope guard for one trial cell or one batch bracket. Owns the
/// SandboxManager; on drop (normal return, `?`, panic, cancel) it runs the
/// label-filtered reaper, so no early return can leak. Replacing the manual
/// `sandbox.cleanup()` callsites one by one; both paths stay correct during
/// migration because labels make reaping idempotent.
pub struct RunGuard {
    manager: SandboxManager,
    disarmed: bool,
}

impl RunGuard {
    pub fn new(manager: SandboxManager) -> Self {
        Self {
            manager,
            disarmed: false,
        }
    }
    pub fn manager(&self) -> &SandboxManager {
        &self.manager
    }
    /// Keep resources on success paths that hand ownership onward (e.g. the
    /// batch bracket between pre and post). A disarmed guard drops silently.
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        // Containers first (label-filtered, exact instance value only).
        let filter = format!("label=sdb.run={}", self.manager.instance_id);
        let list = Command::new("docker")
            .args(["ps", "-a", "-q", "--filter", &filter])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let ids: Vec<&str> = list.split_whitespace().collect();
        if !ids.is_empty() {
            let mut args = vec!["rm", "-f"];
            args.extend(ids);
            let _ = Command::new("docker").args(&args).output();
        }
        Self::prune_files(&self.manager);
        // Own network ONLY when it holds zero containers. Never
        // remove_network_retrying here: its disconnect loop force-detaches
        // endpoints by name and murdered the sibling (guardproof-b lived on
        // A's network because the test attached both probes to it — the
        // disconnect took B's endpoint down with the net).
        // Batchref nets belong to BatchRef::finish, never to a trial guard.
        if !SandboxManager::network_in_use_or_young(&self.manager.network_name()) {
            SandboxManager::remove_network_retrying(&self.manager.network_name());
        }
    }
}

impl RunGuard {
    fn prune_files(m: &SandboxManager) {
        for idx in 1..=8u8 {
            let _ = fs::remove_file(m.upstream_marker_path(idx));
        }
    }
}

impl Drop for SandboxManager {
    fn drop(&mut self) {
        if self.needs_cleanup.load(Ordering::SeqCst) {
            self.cleanup();
        }
    }
}

#[cfg(test)]
mod launch_class_tests {
    use super::SandboxManager;

    #[test]
    fn test_model_packaging_vs_daemon_errors() {
        // The five quoted launch failures from the audit: model faults.
        for e in [
            "exec: \"/broker\": stat /broker: no such file or directory",
            "exec: \"/proxy\": stat /proxy: no such file or directory",
            "exec: \"/server\": is a directory: permission denied",
            "exec: \"./start.sh\": permission denied",
            "exec: \"/app/sdb-redis\": permission denied",
        ] {
            assert!(SandboxManager::is_model_packaging_error(e), "{e}");
        }
        // Daemon-side failures stay infra (incl. network-not-found:
        // bare "not found" must NOT match).
        for e in [
            "Cannot connect to the Docker daemon",
            "network subdollar-net-x not found",
            "no space left on device",
            "driver failed programming external connectivity",
        ] {
            assert!(!SandboxManager::is_model_packaging_error(e), "{e}");
        }
    }
}

#[cfg(test)]
mod gc_tests {
    use super::SandboxManager;
    use std::collections::HashSet;

    fn live(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_run_matches_live_covers_trial_parent_batchref() {
        let live = live(&["pdf_model_123_w0_trial3", "redis_m_1_w0"]);
        // Exact, parent-prefix, and batchref-suffix stems all match.
        assert!(SandboxManager::run_matches_live(
            "pdf_model_123_w0_trial3",
            &live
        ));
        assert!(SandboxManager::run_matches_live("pdf_model_123_w0", &live));
        assert!(SandboxManager::run_matches_live(
            "pdf_model_123_w0_batchref",
            &live
        ));
        // Unrelated runs never match, however similar the prefix.
        assert!(!SandboxManager::run_matches_live(
            "pdf_other_9_w0_trial1",
            &live
        ));
        assert!(!SandboxManager::run_matches_live("unrelated", &live));
    }

    #[test]
    fn test_created_secs_parses_inspect_format() {
        // docker inspect .Created layout.
        let ts = SandboxManager::container_created_secs("2026-10-06T06:13:10.123456789Z");
        assert!(ts.is_some_and(|t| t > 1_700_000_000));
        assert!(SandboxManager::container_created_secs("not-a-date").is_none());
        assert!(SandboxManager::container_created_secs("").is_none());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_egress_proxy_allowlist_blocks_exfil() {
        use super::egress_allowed;
        for h in [
            "openrouter.ai",
            "api.openrouter.ai",
            "x.proxy.golang.org",
            "github.com",
            "a.b.github.com",
        ] {
            assert!(egress_allowed(h), "{}", h);
        }
        for h in [
            "evil.com",
            "1.2.3.4",
            "openrouter.ai.evil.com",
            "notgithub.com",
            "",
        ] {
            assert!(!egress_allowed(h), "{}", h);
        }
    }

    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn test_ensure_runnable_candidate_scenarios() {
        let temp_dir = std::env::temp_dir().join(format!("test_docker_sb_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let sm = SandboxManager::new();

        // 1. Empty dir
        assert!(!sm.ensure_runnable_candidate(&temp_dir).unwrap());

        // 2. Root Dockerfile
        fs::write(temp_dir.join("Dockerfile"), "FROM alpine").unwrap();
        assert!(sm.ensure_runnable_candidate(&temp_dir).unwrap());
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));

        // 3. Nested Dockerfile
        let nested = temp_dir.join("subdir/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("Dockerfile"), "FROM alpine").unwrap();
        assert!(sm.ensure_runnable_candidate(&temp_dir).unwrap());
        assert!(temp_dir.join("Dockerfile").exists());
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));
        let _ = fs::remove_dir_all(&nested);

        // 4. Root start.sh
        fs::write(temp_dir.join("start.sh"), "#!/bin/bash\necho ok").unwrap();
        assert!(sm.ensure_runnable_candidate(&temp_dir).unwrap());
        let _ = fs::remove_file(temp_dir.join("start.sh"));

        // 5. Nested start.sh
        let nested_sh = temp_dir.join("project");
        fs::create_dir_all(&nested_sh).unwrap();
        fs::write(nested_sh.join("start.sh"), "#!/bin/bash\necho nested").unwrap();
        assert!(sm.ensure_runnable_candidate(&temp_dir).unwrap());
        assert!(temp_dir.join("start.sh").exists());
        let root_content = fs::read_to_string(temp_dir.join("start.sh")).unwrap();
        assert!(root_content.contains("/workspace/project"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_wait_for_port() {
        let sm = SandboxManager::new();

        // 1. Unbound port should return false
        let ok = sm.wait_for_port(59995, 1).await;
        assert!(!ok);

        // 2. Active port should return true
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ok2 = sm.wait_for_port(port, 2).await;
        assert!(ok2);

        // 3. Port that accepts and immediately closes (simulating dead backend across proxy)
        let listener_rst = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_rst = listener_rst.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener_rst.accept().await {
                drop(stream); // Immediate close / EOF
            }
        });
        let ok3 = sm.wait_for_port(port_rst, 1).await;
        assert!(
            !ok3,
            "Port that drops connections immediately should not be considered ready"
        );
    }

    #[test]
    fn test_docker_cleanup_and_logs() {
        let sm = SandboxManager::with_id("test-cleanup");
        let _guard = RunGuard::new(SandboxManager::with_id("test-cleanup"));
        sm.cleanup();
        let logs = sm.get_candidate_logs();
        assert!(!logs.is_empty());
    }

    #[test]
    fn test_start_and_cleanup_reference_containers() {
        let sm = SandboxManager::with_id("test-ref");
        let _guard = RunGuard::new(SandboxManager::with_id("test-ref"));
        let res_redis = sm.start_reference_redis(59994);
        assert!(res_redis.is_ok());
        let res_http = sm.start_reference_http(59993);
        assert!(res_http.is_ok());
        sm.cleanup();
    }

    #[test]
    fn test_run_network_reference_alias_reachable() {
        let sm = SandboxManager::with_id("net-alias-test");
        let _guard = RunGuard::new(SandboxManager::with_id("net-alias-test"));
        sm.cleanup();
        assert!(sm.ensure_network().is_ok());
        // Idempotent: second create must not fail
        assert!(sm.ensure_network().is_ok());
        assert!(sm.start_reference_redis(59991).is_ok());
        // Give the reference a moment to accept connections
        std::thread::sleep(std::time::Duration::from_millis(2500));

        let net = sm.network_name();
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                &net,
                BENCH_REDIS_CLIENT_IMAGE,
                "redis-cli",
                "-h",
                "ref-redis",
                "-p",
                "6379",
                "ping",
            ])
            .output();
        sm.cleanup();

        let stdout = out
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        assert!(
            stdout.contains("PONG"),
            "expected PONG via ref-redis network alias, got: {:?}",
            stdout
        );
    }

    #[test]
    fn test_relax_workspace_perms_opens_any_uid_write() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("test_relax_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("aof.log"), b"x").unwrap();
        std::fs::set_permissions(sub.join("aof.log"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", sub.join("link")).ok();
        std::fs::write(sub.join("exec.sh"), b"x").ok();
        std::fs::set_permissions(sub.join("exec.sh"), std::fs::Permissions::from_mode(0o755)).ok();
        SandboxManager::relax_workspace_perms(&base);
        let fmode = std::fs::metadata(sub.join("aof.log"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            fmode, 0o666,
            "data files must be world-writable (any candidate UID)"
        );
        let xmode = std::fs::metadata(sub.join("exec.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            xmode, 0o777,
            "executables must keep +x while gaining rw (forced 0666 broke exec)"
        );
        let dmode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dmode, 0o777,
            "dirs must be world-rwx (runtime file creation)"
        );
        // Symlink itself must survive untouched (never followed into).
        assert!(std::fs::symlink_metadata(sub.join("link"))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_start_candidate_in_docker_start_sh_and_dockerfile() {
        let temp_dir = std::env::temp_dir().join(format!("test_docker_run_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let sm = SandboxManager::with_id("test-cand");
        let _guard = RunGuard::new(SandboxManager::with_id("test-cand"));

        // 1. start.sh case
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\necho candidate running\n",
        )
        .unwrap();
        let res_sh = sm.start_candidate_in_docker(&temp_dir, 59990);
        assert!(res_sh.is_ok());
        sm.cleanup();

        // 2. Dockerfile case
        let _ = fs::remove_file(temp_dir.join("start.sh"));
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM alpine\nCMD [\"echo\", \"done\"]\n",
        )
        .unwrap();
        let res_df = sm.start_candidate_in_docker(&temp_dir, 59989);
        assert!(res_df.is_ok());
        sm.cleanup();

        let _ = fs::remove_dir_all(&temp_dir);
    }
    #[test]
    fn test_prune_skips_attached_networks_by_container_count() {
        // Old gate retired by network_is_stale (count + age); coverage moved
        // to test_prune_age_gate below. Kept as documentation of intent.
    }

    #[test]
    fn test_prune_age_gate() {
        // 0 containers + old timestamp = stale.
        assert!(SandboxManager::network_is_stale(
            "0 2020-01-01 00:00:00.000 +0000 UTC"
        ));
        // Attached containers never stale, however old.
        assert!(!SandboxManager::network_is_stale(
            "2 2020-01-01 00:00:00.000 +0000 UTC"
        ));
        // Young empty network = not yet attached (parallel workers), keep.
        // Far-future timestamp is always young.
        assert!(!SandboxManager::network_is_stale(
            "0 2999-01-01 00:00:00.000 +0000 UTC"
        ));
        // Unparseable fails closed.
        assert!(!SandboxManager::network_is_stale(""));
        assert!(!SandboxManager::network_is_stale("<no value>"));
        assert!(!SandboxManager::network_is_stale("0"));
    }

    #[test]
    fn test_pool_exhaustion_wordings() {
        // Stock dockerd + distro/LXD rewordings all match; unknown fails open.
        assert!(SandboxManager::is_pool_exhausted(
            "Error response from daemon: could not find an available, non-overlapping IPv4 address pool among the defaults to assign to the network"
        ));
        assert!(SandboxManager::is_pool_exhausted(
            "Error response from daemon: all predefined address pools have been fully subnetted"
        ));
        assert!(SandboxManager::is_pool_exhausted("no available network"));
        // Subnet collision is a different retry path (wheel bump), not exhaustion.
        assert!(!SandboxManager::is_pool_exhausted(
            "Pool overlaps with other one on this address space"
        ));
        assert!(!SandboxManager::is_pool_exhausted(
            "Error response from daemon: network not found"
        ));
        assert!(!SandboxManager::is_pool_exhausted(""));
    }

    #[test]
    fn test_run_subnet_deterministic_and_spread() {
        // Distinct nets spread (no mass collision onto one /24).
        let mut set = std::collections::HashSet::new();
        for i in 0..50 {
            set.insert(SandboxManager::run_subnet_attempt(
                &format!("subdollar-net-run{i}"),
                0,
            ));
        }
        assert!(set.len() > 25, "subnets collapse: {}", set.len());
        // Attempt salt moves the subnet (collision retry is real).
        assert_ne!(
            SandboxManager::run_subnet_attempt("subdollar-net-abc", 0),
            SandboxManager::run_subnet_attempt("subdollar-net-abc", 1)
        );
        // All inside our own wheel.
        for sn in &set {
            assert!(sn.starts_with("10.201."), "foreign subnet: {sn}");
            assert!(sn.ends_with("/24"), "bad mask: {sn}");
        }
    }

    #[test]
    fn test_prune_parses_network_id_list() {
        assert!(SandboxManager::parse_network_ids("").is_empty());
        assert!(SandboxManager::parse_network_ids("   \n").is_empty());
        let ids = SandboxManager::parse_network_ids("abc123\ndef456\n");
        assert_eq!(ids, vec!["abc123", "def456"]);
    }

    #[test]
    fn test_owned_names_cover_pdf_egress_upstreams() {
        let sm = SandboxManager::with_id("parent_20261002_120000_trial2");
        let names = sm.owned_container_names();
        // Previously leaked: nothing named the pdf ref, the egress
        // sidecar, or the proxy upstreams, so trial cleanup left them
        // running (97 leaked corefiles proved the file side; the
        // container side was the missing ref-pdf/upstream/egress rms).
        for want in [
            "subdollar-ref-pdf-parent_20261002_120000_trial2",
            "subdollar-egress-parent_20261002_120000_trial2",
            "subdollar-upstream1-parent_20261002_120000_trial2",
            "subdollar-upstream8-parent_20261002_120000_trial2",
            "subdollar-agent-nonexistent-should-not-matter",
        ] {
            if want.contains("nonexistent") {
                assert!(!names.iter().any(|n| n == want));
                continue;
            }
            assert!(names.iter().any(|n| n == want), "missing {want}");
        }
        // Trial cleanup must NEVER name the batchref share: the bracket
        // outlives every trial, and trial N's exit murdered it (gate cells:
        // post-batch "network ..._batchref not found" on both cells).
        assert!(
            names.iter().all(|n| !n.ends_with("_batchref")),
            "trial owns batchref share!"
        );
    }

    #[test]
    fn test_owned_names_do_not_truncate_non_trial_ids() {
        // An id that merely contains "_trial" without the trailing
        // `<parent>_trial<N>` shape must not sprout batchref names.
        let sm = SandboxManager::with_id("my_trial_run");
        assert!(sm
            .owned_container_names()
            .iter()
            .all(|n| !n.ends_with("_batchref")));
        let plain = SandboxManager::with_id("parent_20261002_120000");
        assert!(plain
            .owned_container_names()
            .iter()
            .all(|n| !n.ends_with("_batchref")));
    }

    #[test]
    fn test_label_filter_is_exact_instance_value() {
        // Labels replace the name-grep: the filter value is the full
        // instance id, so `<parent>` never matches `<parent>_trialN` or
        // `<parent>_batchref`. Pin the exact-match construction.
        let sm = SandboxManager::with_id("parent_20261002_120000_trial2");
        assert_eq!(
            sm.label_filter(false),
            vec!["label=sdb.run=parent_20261002_120000_trial2"]
        );
        let b = SandboxManager::with_id("parent_20261002_120000_batchref");
        assert_eq!(
            b.label_filter(true),
            vec!["label=sdb.run=parent_20261002_120000_batchref"]
        );
    }

    /// Live-docker: a guard drop reaps its labeled container; the sibling
    /// trial's container (different label value) survives. Ignored so the
    /// default suite stays hermetic.
    #[test]
    #[ignore = "requires docker; guard-drop reaping proof"]
    fn guard_drop_reaps_own_label_only() {
        let a = "guardproof-a-456";
        let b = "guardproof-b-789";
        for id in [a, b] {
            // Pre-clean: a previous failed run leaves name + net behind, and
            // `docker run --name` fails with nonzero status (not Err), which
            // .output().unwrap() swallows — the probe then "never starts".
            // NOTE: do NOT use SandboxManager here: ensure_network arms
            // needs_cleanup, so the temp manager's Drop at loop-end calls
            // cleanup() and reaps the probe just created (this faked
            // "never started" for three runs). Bare docker calls instead.
            let net = format!("subdollar-net-{}", id);
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &format!("guardprobe-{}", id)])
                .output();
            let _ = std::process::Command::new("docker")
                .args(["network", "rm", &net])
                .output();
            let co = std::process::Command::new("docker")
                .args([
                    "network",
                    "create",
                    "--label",
                    &format!("sdb.run={}", id),
                    &net,
                ])
                .output()
                .unwrap();
            assert!(
                co.status.success(),
                "net {id}: {}",
                String::from_utf8_lossy(&co.stderr)
            );
            let out = std::process::Command::new("docker")
                .args([
                    "run",
                    "-d",
                    "--rm",
                    "--label",
                    &format!("sdb.run={}", id),
                    "--label",
                    "sdb.role=probe",
                    "--name",
                    &format!("guardprobe-{}", id),
                    "--network",
                    &net,
                    "alpine:3",
                    "sleep",
                    "300",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "probe {id} failed to start: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // Drop a guard for A: only A's container may die.
        // Precondition: both probes actually running (alpine pull / start
        // race would otherwise fake a pass or a confusing fail).
        for id in [a, b] {
            let running = std::process::Command::new("docker")
                .args(["ps", "--format", "{{.Names}}"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            assert!(
                running.contains(&format!("guardprobe-{}", id)),
                "probe {id} never started; cannot prove isolation"
            );
        }
        {
            let _g = RunGuard::new(SandboxManager::with_id(a));
        }
        let left = std::process::Command::new("docker")
            .args(["ps", "-a", "--format", "{{.Names}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        assert!(
            !left.contains(&format!("guardprobe-{}", a)),
            "own container must be reaped"
        );
        assert!(
            left.contains(&format!("guardprobe-{}", b)),
            "sibling container must survive"
        );
        // Full teardown through the guard path (nets carry the label now):
        // B's guard reaps container + zero-container net; A's net likewise.
        {
            let _gb = RunGuard::new(SandboxManager::with_id(b));
        }
        for id in [a, b] {
            let _ = std::process::Command::new("docker")
                .args(["network", "rm", &format!("subdollar-net-{}", id)])
                .output();
        }
    }

    #[test]
    fn test_sweep_pattern_is_exact_instance_match() {
        // The retired `^subdollar-.*-{id}$` pattern matched a parent id
        // against `<parent>_trial<N>` containers: trial 1's cleanup reaped
        // trial 2's live candidate. The narrowed alternation fires on the
        // exact id only — never the `_batchref` sibling (BatchRef::finish
        // owns that share; trial N's exit murdered the bracket through it).
        let pat = |id: &str| {
            format!(
                "^subdollar-(candidate|ref-redis|ref-http|ref-pdf|omp-agent|egress|upstream[1-8])-{}($)",
                id
            )
        };
        let re = ::regex::Regex::new(&pat("parent_20261002_120000_trial2")).unwrap();
        assert!(re.is_match("subdollar-candidate-parent_20261002_120000_trial2"));
        assert!(!re.is_match("subdollar-ref-pdf-parent_20261002_120000_batchref"));
        assert!(!re.is_match("subdollar-candidate-parent_20261002_120000_trial22"));
    }
}
