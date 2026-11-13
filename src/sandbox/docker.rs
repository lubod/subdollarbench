use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Egress allowlist for the per-run allowlist forward proxy. R81 F2: the
/// `.47` sandbox keeps the model API only. The registries (crates.io,
/// Go, npm, PyPI) and GitHub are gone with the other toolchains: a Rust
/// candidate is standard library only and builds offline. Everything
/// else 403s. Raw sockets bypass proxy env (documented residual); the
/// API key in-container is protected by channel blocking, not by secrecy.
pub const EGRESS_ALLOWLIST: &[&str] = &["openrouter.ai", "api.openrouter.ai"];

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

/// R48 F2: the Docker arguments for a CPU set. Empty when `set` is empty.
pub fn cpuset_args(set: &str) -> Vec<String> {
    if set.is_empty() {
        Vec::new()
    } else {
        vec!["--cpuset-cpus".to_string(), set.to_string()]
    }
}

/// R48 F2/R48-3: the agent's development reference (`ref-sql`) runs on the
/// session cores, not the fast grading set. It is alive during the model's
/// session, so it must not share the graded candidates' cores.
pub fn reference_sql_args(
    instance_id: &str,
    name: &str,
    net: &str,
    mount: &str,
    port: u16,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--rm".into(),
        "--label".into(),
        format!("sdb.run={instance_id}"),
        "--label".into(),
        "sdb.role=ref-sql".into(),
        "--name".into(),
        name.to_string(),
        "--network".into(),
        net.to_string(),
        "--network-alias".into(),
        "ref-sql".into(),
        "--memory=2g".into(),
        "--cpus=2.0".into(),
    ];
    args.extend(cpuset_args(&crate::grading::session_cpuset()));
    args.extend([
        "--pids-limit=256".into(),
        "-v".into(),
        mount.to_string(),
        "-p".into(),
        format!("127.0.0.1:{port}:5480"),
        EGRESS_PROXY_IMAGE.to_string(),
        "python3".into(),
        "-u".into(),
        "/srv/sql_server.py".into(),
        "--port".into(),
        "5480".into(),
        "--data".into(),
        "/tmp".into(),
    ]);
    args
}

pub struct SandboxManager {
    pub instance_id: String,
    pub needs_cleanup: AtomicBool,
}

/// Extra runtime knobs for a candidate container. Used by campaign fault
/// injectors: the ENOSPC incident runs the candidate against a small tmpfs
/// data volume (`SDB_DATA_DIR=/data`) so "disk full" is real, deterministic
/// and contained.
#[derive(Default, Clone)]
pub struct CandidateRunExtras {
    /// tmpfs mounts in docker syntax (`/data:size=8m`).
    pub tmpfs: Vec<String>,
    /// Environment variables for the container.
    pub env: Vec<(String, String)>,
    /// Extra container ports published 1:1 on 127.0.0.1 (dashboard stage:
    /// the worker's HTTP port must be reachable from the harness).
    pub ports: Vec<u16>,
    /// Memory limit (docker syntax, e.g. `64m`). None means the default 2g.
    /// The security stage runs the candidate under 64MB so an oversized
    /// protocol frame must be rejected *before* allocating it.
    pub memory: Option<String>,
    /// R48 F2: the CPU set the graded container is pinned to. None leaves
    /// Docker's scheduler free (tests and older callers).
    pub cpuset: Option<String>,
    /// Host bind mounts `(host_path, container_path)`. The disk-resident /
    /// compaction stages bind a host scratch dir to `/data` (with
    /// SDB_DATA_DIR=/data) so the harness can measure the on-disk footprint
    /// directly and keep it across candidate restarts.
    pub binds: Vec<(String, String)>,
    /// Extra argv appended to the candidate's launch command (the sdb-sql
    /// launch contract: `--port <p> --data <dir>`; the harness owns the
    /// data directory so restarts and the upgrade check see the same
    /// store). Values are harness-generated and shell-safe.
    pub launch_args: Vec<String>,
    /// `--pids-limit` for the container. None means the default 256. The
    /// security stage holds 256 connections (one thread each in the
    /// reference), so it raises the limit to leave room for the server
    /// process itself.
    pub pids_limit: Option<String>,
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

    pub fn anchor_container_name(&self) -> String {
        format!("subdollar-anchor-{}", self.instance_id)
    }

    /// Keep at least one container attached to the run network for the whole
    /// intended life of the run.
    ///
    /// Docker's `{{len .Containers}}` counts only *running* endpoints: a
    /// candidate stopped for its kill -9 window (or the gap between agent
    /// phase and candidate start) makes the network look empty, and every
    /// staleness gate is "empty AND older than 300s". A foreground campaign
    /// is invisible to the engine's 60s periodic sweep (its `busy` check
    /// only covers engine-owned worker_runs), so the sweep's
    /// `prune_labeled_networks` reached into a live stage: live incident
    /// 2026-10-14, s4 network 354s old, candidate killed for the AOF probe
    /// -> sweep removed the network -> `docker start` died with
    /// "network ... not found" and the ticket was scored as a model
    /// regression. The anchor makes the network non-empty for its entire
    /// intended life; a zombie anchor (hard-killed CLI) is reaped by the
    /// orphan sweep's 3h age gate, after which the net prunes normally.
    pub fn ensure_run_anchor(&self) -> Result<()> {
        let name = self.anchor_container_name();
        let inspect = Command::new("docker")
            .args(["inspect", "--format", "{{.State.Running}}", &name])
            .output();
        if let Ok(o) = inspect {
            if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true" {
                return Ok(());
            }
        }
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                &format!("sdb.run={}", self.instance_id),
                "--label",
                "sdb.role=anchor",
                "--name",
                &name,
                "--network",
                &self.network_name(),
                "--memory=32m",
                "--cpus=0.1",
                "--pids-limit=32",
                "--entrypoint",
                "tail",
                EGRESS_PROXY_IMAGE,
                "-f",
                "/dev/null",
            ])
            .output()
            .map_err(|e| anyhow!("run anchor spawn failed: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            // A concurrent ensure_network may have won the name race.
            if !err.contains("already exists") {
                return Err(anyhow!(
                    "run anchor failed to start (network would be sweepable): {}",
                    err.trim()
                ));
            }
        }
        Ok(())
    }

    pub fn egress_proxy_name(&self) -> String {
        format!("subdollar-egress-{}", self.instance_id)
    }

    /// HA node container name (three-node finale: aliases ha1/ha2/ha3).
    pub fn ha_node_container_name(&self, idx: u8) -> String {
        format!("subdollar-ha{}-{}", idx, self.instance_id)
    }

    /// Pure arg builder for one HA node container (unit-testable): docker
    /// args and the node's launch command. `replicaof` points at the
    /// leader's alias + port (`("ha1", port)` on cluster start; the new
    /// leader's alias on rejoin).
    #[allow(clippy::too_many_arguments)]
    fn ha_node_args(
        instance_id: &str,
        name: &str,
        net: &str,
        canonical_workdir: &Path,
        idx: u8,
        port: u16,
        replicaof: Option<(&str, u16)>,
        dash_port: Option<u16>,
        extra_env: &[(String, String)],
        tmpfs: Option<&str>,
    ) -> (Vec<String>, String) {
        let mut launch = format!(
            "cd /workspace && chmod +x start.sh 2>/dev/null; ./start.sh --port {port} --data /data{idx}"
        );
        if let Some((alias, master_port)) = replicaof {
            launch.push_str(&format!(" --replicaof {alias}:{master_port}"));
        }
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--label".into(),
            format!("sdb.run={instance_id}"),
            "--label".into(),
            format!("sdb.role=ha{idx}"),
            "--name".into(),
            name.to_string(),
        ];
        for (k, v) in extra_env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.extend([
            "--memory=2g".into(),
            "--cpus=2.0".into(),
            "--pids-limit=256".into(),
            "--network".into(),
            net.to_string(),
            "--network-alias".into(),
            format!("ha{idx}"),
            "-p".into(),
            format!("127.0.0.1:{port}:{port}"),
        ]);
        if idx == 1 {
            if let Some(dp) = dash_port {
                args.push("-p".into());
                args.push(format!("127.0.0.1:{dp}:{dp}"));
            }
        }
        if let Some(tmpfs) = tmpfs {
            args.push("--tmpfs".into());
            args.push(tmpfs.to_string());
        }
        args.push("-v".into());
        args.push(format!("{}:/workspace", canonical_workdir.display()));
        args.push("subdollar-sandbox".into());
        args.push("/bin/bash".into());
        args.push("-lc".into());
        args.push(launch.clone());
        (args, launch)
    }

    fn canonical_workdir_for(workdir: &Path) -> std::path::PathBuf {
        if workdir.is_absolute() {
            workdir.to_path_buf()
        } else {
            workdir
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir().join(workdir))
        }
    }

    fn run_ha_container(&self, args: &[String], idx: u8) -> Result<()> {
        let out = Command::new("docker").args(args).output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "HA node {idx} failed to start: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Launch the three-node HA cluster. `ports` are the published RESP
    /// ports (node order); node 1 also gets `dash_port`. The candidate's
    /// `./start.sh` must accept `--port/--data/--replicaof` (ticket 10).
    pub fn start_ha_nodes(
        &self,
        workdir: &Path,
        ports: &[u16],
        dash_port: Option<u16>,
    ) -> Result<()> {
        if ports.len() != 3 {
            return Err(anyhow!("HA cluster needs exactly 3 node ports"));
        }
        self.ensure_network()?;
        Self::relax_workspace_perms(workdir);
        let canonical = Self::canonical_workdir_for(workdir);
        let net = self.network_name();
        for (i, port) in ports.iter().enumerate() {
            let idx = (i + 1) as u8;
            let name = self.ha_node_container_name(idx);
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
            let replicaof = if idx == 1 {
                None
            } else {
                Some(("ha1", ports[0]))
            };
            let (args, _launch) = Self::ha_node_args(
                &self.instance_id,
                &name,
                &net,
                &canonical,
                idx,
                *port,
                replicaof,
                dash_port,
                &[],
                None,
            );
            self.run_ha_container(&args, idx)?;
        }
        Ok(())
    }

    /// Recreate one node with a fresh `--replicaof` target (the rejoin
    /// path: the old master comes back pointed at the new leader).
    pub fn respawn_ha_node(
        &self,
        workdir: &Path,
        idx: u8,
        port: u16,
        replicaof: Option<(&str, u16)>,
    ) -> Result<()> {
        self.ensure_network()?;
        let canonical = Self::canonical_workdir_for(workdir);
        let net = self.network_name();
        let name = self.ha_node_container_name(idx);
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        let (args, _launch) = Self::ha_node_args(
            &self.instance_id,
            &name,
            &net,
            &canonical,
            idx,
            port,
            replicaof,
            None,
            &[],
            None,
        );
        self.run_ha_container(&args, idx)
    }

    /// SQL ticket 19: the same three-node topology with a writable tmpfs
    /// per node and the cluster's node list in `SDB_SQL_NODES`. The tmpfs
    /// makes the reference's data dir writable without a host bind; a
    /// respawned node starts fresh and rebuilds from the leader's log.
    pub fn start_sql_ha_nodes(&self, workdir: &Path, ports: &[u16], nodes_env: &str) -> Result<()> {
        if !(2..=3).contains(&ports.len()) {
            return Err(anyhow!("SQL HA cluster needs two or three node ports"));
        }
        self.ensure_network()?;
        Self::relax_workspace_perms(workdir);
        let canonical = Self::canonical_workdir_for(workdir);
        let net = self.network_name();
        let env = vec![("SDB_SQL_NODES".to_string(), nodes_env.to_string())];
        for (i, port) in ports.iter().enumerate() {
            let idx = (i + 1) as u8;
            let name = self.ha_node_container_name(idx);
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
            let replicaof = if idx == 1 {
                None
            } else {
                Some(("ha1", ports[0]))
            };
            let tmpfs = format!("/data{idx}:size=256m");
            let (args, _launch) = Self::ha_node_args(
                &self.instance_id,
                &name,
                &net,
                &canonical,
                idx,
                *port,
                replicaof,
                None,
                &env,
                Some(&tmpfs),
            );
            self.run_ha_container(&args, idx)?;
        }
        Ok(())
    }

    /// Recreate one SQL node with a fresh data tmpfs, pointed at a new
    /// leader (the failover rejoin).
    pub fn respawn_sql_ha_node(
        &self,
        workdir: &Path,
        idx: u8,
        port: u16,
        replicaof: Option<(&str, u16)>,
        nodes_env: &str,
    ) -> Result<()> {
        self.ensure_network()?;
        let canonical = Self::canonical_workdir_for(workdir);
        let net = self.network_name();
        let name = self.ha_node_container_name(idx);
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        let env = vec![("SDB_SQL_NODES".to_string(), nodes_env.to_string())];
        let tmpfs = format!("/data{idx}:size=256m");
        let (args, _launch) = Self::ha_node_args(
            &self.instance_id,
            &name,
            &net,
            &canonical,
            idx,
            port,
            replicaof,
            None,
            &env,
            Some(&tmpfs),
        );
        self.run_ha_container(&args, idx)
    }

    pub fn stop_ha_nodes(&self, count: u8) {
        for idx in 1..=count {
            let name = self.ha_node_container_name(idx);
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        }
    }

    /// SIGKILL one node (crash simulation; container persists for restart).
    pub fn kill_ha_node(&self, idx: u8) -> Result<()> {
        let name = self.ha_node_container_name(idx);
        let out = Command::new("docker")
            .args(["kill", "-s", "KILL", &name])
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "HA node {idx} kill failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Freeze one HA node so it cannot pull the leader's log or ack a
    /// commit (round-17 J1's sync-commit probe).
    pub fn pause_ha_node(&self, idx: u8) -> Result<()> {
        let name = self.ha_node_container_name(idx);
        let out = Command::new("docker").args(["pause", &name]).output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "HA node {idx} pause failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Thaw a node previously frozen with [`Self::pause_ha_node`].
    pub fn resume_ha_node(&self, idx: u8) -> Result<()> {
        let name = self.ha_node_container_name(idx);
        let out = Command::new("docker").args(["unpause", &name]).output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "HA node {idx} resume failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// The node container's Docker state (`running`, `paused`, `exited`,
    /// ...), or `None` when it cannot be inspected. A failed control action
    /// on a node that already exited is the candidate's crash, not a
    /// harness fault (round-18 N2).
    pub fn ha_node_state(&self, idx: u8) -> Option<String> {
        let name = self.ha_node_container_name(idx);
        let out = Command::new("docker")
            .args(["inspect", "-f", "{{.State.Status}}", &name])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// HA nodes in the owned-container belt (label reaping covers them too).
    fn ha_node_names(&self) -> Vec<String> {
        (1..=3).map(|i| self.ha_node_container_name(i)).collect()
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

    pub fn ref_sql_name(&self) -> String {
        format!("subdollar-ref-sql-{}", self.instance_id)
    }

    /// Verification-time isolation: reference implementations are the
    /// harness's oracle, never the candidate's tool. Candidates run on the
    /// run network, so while verifying, every reference is detached from it
    /// — a candidate must not be able to proxy to `ref-*` and fake its way
    /// through stages. The agent keeps them during development and the
    /// verification can re-attach them by alias.
    pub fn detach_references(&self) {
        let net = self.network_name();
        let name = self.ref_sql_name();
        let _ = Command::new("docker")
            .args(["network", "disconnect", &net, &name])
            .output();
    }

    /// Re-attach references under their stable aliases (idempotent).
    pub fn reattach_references(&self) {
        let net = self.network_name();
        let name = self.ref_sql_name();
        let _ = Command::new("docker")
            .args(["network", "connect", "--alias", "ref-sql", &net, &name])
            .output();
    }

    pub fn network_name(&self) -> String {
        format!("subdollar-net-{}", self.instance_id)
    }

    /// Docker object labels owned by this instance. Every container and
    /// network this manager creates carries `sdb.run=<instance_id>` (+ a
    /// `sdb.role`), so the reaper selects by label filter instead of
    /// name-grep: names collide across parents sharing a timestamp, labels
    /// do not. `sdb.role` is informational (candidate/ref-sql/.../batchref)
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
    /// Reference containers join it under a stable alias (`ref-sql`) and
    /// the OMP agent container is attached to it,
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
                self.ensure_run_anchor()?;
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if stderr.contains("already exists") {
                self.ensure_run_anchor()?;
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

    /// The sdb-sql reference: a Python `sqlite3` adapter speaking the
    /// text protocol, on the run network as `ref-sql`. The agent compares
    /// semantics against it during development; verification detaches it
    /// like every other reference.
    pub fn start_reference_sql(&self, port: u16) -> Result<()> {
        // Sweep staging copies a hard kill left behind (the per-instance
        // dir is otherwise reused; anything an hour old cannot belong to a
        // live run).
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("subdollar-ref-sql-src-") {
                    continue;
                }
                let stale = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().map(|d| d.as_secs() > 3600).unwrap_or(false))
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let name = self.ref_sql_name();
        let net = self.network_name();
        self.ensure_network()?;
        info!(
            "Starting sdb-sql reference on port {} ({}, alias ref-sql on {})",
            port, name, net
        );
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        // The server script is generated on the host and mounted read-only;
        // the container's own DB lives in its /tmp (fresh per container).
        let script_dir =
            std::env::temp_dir().join(format!("subdollar-ref-sql-src-{}", self.instance_id));
        crate::verifier::sql::ref_server::write_reference(&script_dir, 5480)
            .map_err(|e| anyhow!("write ref-sql script: {e}"))?;
        let mount = format!("{}:/srv:ro", script_dir.display());

        let out = Command::new("docker")
            .args(reference_sql_args(
                &self.instance_id,
                &name,
                &net,
                &mount,
                port,
            ))
            .output()
            .map_err(|e| anyhow!("reference sql spawn failed: {e}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "reference sql failed to start on host port {port}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
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

    /// Graceful stop: SIGTERM, up to `timeout_secs` to flush, then SIGKILL
    /// and remove. The ticket-4 contract ("the harness stops the server
    /// cleanly; you have a few seconds to flush"); ticket 5's ladder uses
    /// [`Self::kill_candidate`] instead.
    ///
    /// `docker stop` signals PID 1 only, and a `start.sh` without `exec`
    /// leaves the server as a child of a shell that ignores SIGTERM (PID 1
    /// has no default action for it): the signal goes to every process in
    /// the container instead, so the server sees it however it was started.
    pub fn stop_candidate_gracefully(&self, timeout_secs: u64) -> Result<()> {
        let name = self.candidate_container_name();
        let state = |format: &str| -> Option<String> {
            Command::new("docker")
                .args(["inspect", &name, "--format", format])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        let Some(running) = state("{{.State.Running}}") else {
            return Ok(()); // already gone
        };
        if running == "true" {
            // PID 1 first: an `exec`'d server *is* PID 1, and `kill(-1)`
            // never signals it (kill(2)), so `docker stop -t 1`'s single
            // second was all it got. `docker kill` reaches PID 1 without
            // needing a shell in the image, then the rest of the
            // processes get their own SIGTERM (`kill -TERM -1` is the
            // spelling dash and busybox both accept). The whole
            // `timeout_secs` is theirs to flush.
            let pid1 = Command::new("docker")
                .args(["kill", "-s", "TERM", &name])
                .output();
            let rest = Command::new("docker")
                .args(["exec", &name, "sh", "-c", "kill -TERM -1"])
                .output();
            if let Ok(out) = &rest {
                // PID 1 may have exited on its signal already; that is the
                // good case, not a warning.
                let still_running = state("{{.State.Running}}").as_deref() == Some("true");
                if !out.status.success() && still_running {
                    // Distroless/scratch images have no `sh`: PID 1 still
                    // got its signal, but the log should say so.
                    warn!(
                        "graceful stop: could not signal the other processes ({}); \
                         PID 1 was signalled",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
            let _ = pid1;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
            while std::time::Instant::now() < deadline
                && state("{{.State.Running}}").as_deref() == Some("true")
            {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            let _ = Command::new("docker")
                .args(["stop", "-t", "1", &name])
                .output();
        }
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        Ok(())
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

    /// Attach the candidate container to the run network so it can dial
    /// run-scoped aliases. Candidates launch on the default
    /// bridge (no --network flag), where neither 127.0.0.1-published ports
    /// (own loopback) nor run-net aliases resolve.
    pub fn connect_candidate_to_network(&self) -> Result<()> {
        let out = Command::new("docker")
            .args([
                "network",
                "connect",
                "--alias",
                // Candidates reach the reference by this alias on the run
                // network (the SQL reference joins it at startup).
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

    /// R81 F3: the `.47` workspace contract, checked before the build so
    /// a non-Rust workspace gets the `[LANG]` line, not a cargo error.
    /// The offline build is the hard enforcement; this reads the manifest
    /// for the clear message and rejects the Dockerfile entrypoint and a
    /// start.sh that does not run the built binary.
    pub fn rust_workspace_violation(workdir: &Path) -> Option<String> {
        let cargo = workdir.join("Cargo.toml");
        if !cargo.is_file() {
            return Some("no Cargo.toml at the workspace root".to_string());
        }
        let Ok(text) = fs::read_to_string(&cargo) else {
            return Some("Cargo.toml is not readable".to_string());
        };
        if let Some(why) = Self::cargo_dependency_violation(&text) {
            return Some(why);
        }
        let start = workdir.join("start.sh");
        if !start.is_file() {
            return Some(
                "./start.sh is missing (the Dockerfile entrypoint is not graded)".to_string(),
            );
        }
        let Ok(script) = fs::read_to_string(&start) else {
            return Some("./start.sh is not readable".to_string());
        };
        if !script.contains("target/release") {
            return Some("./start.sh does not start the binary under target/release/".to_string());
        }
        if !script.contains("exec") {
            return Some("./start.sh does not exec the server".to_string());
        }
        None
    }

    /// The manifest half of [`Self::rust_workspace_violation`]: every
    /// dependency table must be empty or absent, and `[patch]`/`[replace]`
    /// are not allowed. A line-based read is enough for the message; the
    /// offline build is the hard enforcement.
    pub fn cargo_dependency_violation(text: &str) -> Option<String> {
        let is_dep = |p: &str| {
            matches!(
                p,
                "dependencies" | "dev-dependencies" | "build-dependencies"
            )
        };
        let parts = |section: &str| -> Vec<String> {
            section
                .split('.')
                .map(|p| p.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
                .collect()
        };
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_string();
                let fields = parts(&section);
                if fields.iter().any(|p| p == "patch" || p == "replace") {
                    return Some(format!("Cargo.toml has a [{section}] table"));
                }
                // `[dependencies]` itself is allowed when empty; a nested
                // table (`[dependencies.foo]`, `[workspace.dependencies]`,
                // `[target.'cfg(unix)'.dependencies]`) declares one.
                if fields.len() > 1 && fields.iter().any(|p| is_dep(p)) {
                    return Some(format!(
                        "Cargo.toml declares a dependency table [{section}]"
                    ));
                }
                continue;
            }
            let fields = parts(&section);
            if fields.iter().any(|p| is_dep(p)) && line.contains('=') {
                return Some(format!("Cargo.toml declares dependencies in [{section}]"));
            }
        }
        None
    }

    /// The first compiler error and a few following lines, for the graded
    /// build-failure line. Falls back to the output tail.
    pub fn first_build_error(text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        if let Some(i) = lines.iter().position(|l| {
            let t = l.trim_start();
            t.starts_with("error[") || t.starts_with("error:")
        }) {
            return lines[i..(i + 6).min(lines.len())].join("\n");
        }
        let start = lines.len().saturating_sub(8);
        lines[start..].join("\n")
    }

    /// R81 F3: the `.47` entrypoint is `./start.sh` only. A Dockerfile
    /// would bypass the Rust rule, so it is not a runnable candidate.
    pub fn ensure_runnable_candidate(&self, workdir: &Path) -> Result<bool> {
        Ok(workdir.is_dir() && workdir.join("start.sh").is_file())
    }

    /// R81 F3: the harness builds the candidate before every start. A
    /// Rust build is too slow to race the 25 s start-up limit. The limit
    /// is chosen in the round notes from a 10,000-line crate's measured
    /// release build under the sandbox's 2 CPU / 2 GiB.
    pub const RUST_BUILD_TIMEOUT_SECS: u64 = 300;

    pub fn build_rust_candidate(&self, dir: &Path, timeout_secs: u64) -> Result<(), String> {
        let (ok, timed_out, text) = self
            .run_workspace_command_capped(
                dir,
                "cargo build --release --offline --locked 2>&1",
                timeout_secs,
            )
            .map_err(|e| format!("[BUILD] the release build could not run: {e}"))?;
        if timed_out {
            return Err(format!(
                "[BUILD] the release build exceeded its {timeout_secs} s limit"
            ));
        }
        if !ok {
            return Err(format!(
                "[BUILD] the release build failed: {}",
                Self::first_build_error(&text)
            ));
        }
        Ok(())
    }

    /// R81 F3: the Rust rule on the running server. `harness_roots` are
    /// the PIDs the runner started for the agent harness (excluded with
    /// their descendants); the candidate container normally has none,
    /// because the harness runs in its own container, and the F3 fixture
    /// proves the exclusion. A violation is a graded ticket failure.
    pub fn check_rust_server_processes(
        &self,
        port: u16,
        harness_roots: &[u32],
    ) -> Result<(), String> {
        let script = include_str!("docker/lang_check.sh");
        // Run as the container user, the same user start.sh runs as: the
        // kernel's ptrace check lets a process read its own UID's
        // /proc/<pid>/{exe,fd}, and Docker drops CAP_SYS_PTRACE, so a
        // root pass could not read the candidate's ubuntu processes.
        let mut args: Vec<String> = vec![
            "exec".to_string(),
            self.candidate_container_name(),
            "bash".to_string(),
            "-c".to_string(),
            script.to_string(),
            "sdb-lang".to_string(),
            port.to_string(),
        ];
        for pid in harness_roots {
            args.push(pid.to_string());
        }
        let out = Command::new("docker")
            .args(&args)
            .output()
            .map_err(|e| format!("docker exec: {e}"))?;
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            return Err(format!("the language check could not run: {}", text.trim()));
        }
        if let Some(line) = text.lines().find(|l| l.starts_with("SDB-LANG-FAIL ")) {
            return Err(line.trim_start_matches("SDB-LANG-FAIL ").to_string());
        }
        if text.lines().any(|l| l.trim() == "SDB-LANG-OK") {
            return Ok(());
        }
        Err(format!(
            "the language check printed no verdict: {}",
            text.trim()
        ))
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
        self.start_candidate_in_docker_with(workdir, port, &CandidateRunExtras::default())
    }

    /// ENOSPC incident container: the candidate's log directory
    /// (`SDB_DATA_DIR`) lives on an 8MB tmpfs, so filling it produces real
    /// ENOSPC without touching the host or the workspace.
    pub fn start_candidate_enospc(&self, workdir: &Path, port: u16) -> Result<()> {
        self.start_candidate_in_docker_with(
            workdir,
            port,
            &CandidateRunExtras {
                tmpfs: vec!["/data:size=8m".to_string()],
                env: vec![("SDB_DATA_DIR".to_string(), "/data".to_string())],
                ports: Vec::new(),
                memory: None,
                cpuset: None,
                binds: Vec::new(),
                launch_args: Vec::new(),
                pids_limit: None,
            },
        )
    }

    fn candidate_extras_args(extras: &CandidateRunExtras) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(set) = &extras.cpuset {
            out.extend(cpuset_args(set));
        }
        for t in &extras.tmpfs {
            out.push("--tmpfs".to_string());
            out.push(t.clone());
        }
        for (k, v) in &extras.env {
            out.push("-e".to_string());
            out.push(format!("{k}={v}"));
        }
        for p in &extras.ports {
            out.push("-p".to_string());
            out.push(format!("127.0.0.1:{p}:{p}"));
        }
        for (host, container) in &extras.binds {
            out.push("-v".to_string());
            out.push(format!("{host}:{container}"));
        }
        out
    }

    pub fn start_candidate_in_docker_with(
        &self,
        workdir: &Path,
        port: u16,
        extras: &CandidateRunExtras,
    ) -> Result<()> {
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

        // R81 F3: `./start.sh` is the only entrypoint. The Dockerfile
        // path is gone (a custom image would bypass the Rust rule), and
        // so is the language auto-detection: a non-Rust workspace gets
        // the graded `[LANG]` line before it ever reaches this function.
        let mount_arg = format!("{}:/workspace", canonical_workdir.display());
        let extra = if extras.launch_args.is_empty() {
            String::new()
        } else {
            format!(" {}", extras.launch_args.join(" "))
        };
        let launch_cmd = format!("cd /workspace && chmod +x start.sh && exec ./start.sh{extra}");
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--label".into(),
            format!("sdb.run={}", self.instance_id),
            "--label".into(),
            "sdb.role=candidate".into(),
            "--name".into(),
            name.clone(),
        ];
        // A memory limit without an explicit swap limit lets Docker add an
        // equal amount of swap; a "hard" cap must disable it (round-15 M1).
        let memory = extras.memory.as_deref().unwrap_or("2g");
        args.push(format!("--memory={memory}"));
        if extras.memory.is_some() {
            args.push(format!("--memory-swap={memory}"));
        }
        args.extend([
            "--cpus=2.0".into(),
            format!(
                "--pids-limit={}",
                extras.pids_limit.as_deref().unwrap_or("256")
            ),
        ]);
        args.extend(Self::candidate_extras_args(extras));
        args.extend([
            "-v".into(),
            mount_arg.clone(),
            "-p".into(),
            port_tcp.clone(),
            "-p".into(),
            port_udp.clone(),
            "subdollar-sandbox".into(),
            "/bin/bash".into(),
            "-c".into(),
            launch_cmd,
        ]);
        let out = Command::new("docker").args(&args).output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "candidate sandbox failed to start: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }

        Ok(())
    }

    /// Actively poll the target TCP port until the candidate is ready, up to timeout_secs
    /// Run a command against a directory mounted as `/workspace` inside the
    /// sandbox image, network-isolated (`--network none`) and time-capped.
    /// Used by the tests/CI stage: the candidate's suite must run offline.
    /// The container is named so a timeout kill cannot leave it behind.
    pub fn run_workspace_command(
        &self,
        dir: &Path,
        cmd: &str,
        timeout_secs: u64,
    ) -> Result<(bool, String)> {
        let (ok, _timed_out, text) = self.run_workspace_command_capped(dir, cmd, timeout_secs)?;
        Ok((ok, text))
    }

    /// R81 F3: the capped form, with `timed_out` separate from a command
    /// that ran and failed. `timeout` exits 124 when the cap fires (even
    /// after the KILL), so a build over its limit gets its own clear line.
    pub fn run_workspace_command_capped(
        &self,
        dir: &Path,
        cmd: &str,
        timeout_secs: u64,
    ) -> Result<(bool, bool, String)> {
        let canonical = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            dir.canonicalize().unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|c| c.join(dir))
                    .unwrap_or_else(|_| crate::config::get_repo_root().join(dir))
            })
        };
        let name = format!("subdollar-tests-{}", self.instance_id);
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        let mount = format!("{}:/workspace", canonical.display());
        let cap = (timeout_secs + 10).to_string();
        let output = Command::new("timeout")
            .args([
                "-k",
                "5",
                &cap,
                "docker",
                "run",
                "--rm",
                "--name",
                &name,
                "--network",
                "none",
                "--memory=2g",
                "--cpus=2.0",
                "--pids-limit=256",
                "-v",
                &mount,
                "-w",
                "/workspace",
                "subdollar-sandbox",
                "bash",
                "-c",
                cmd,
            ])
            .output()
            .map_err(|e| anyhow!("failed to run workspace command: {e}"))?;
        // `timeout` returns 124 for a TERM-clean command, but the docker
        // CLI ignores TERM and is KILLed after `-k 5`: then it returns
        // 137. Both mean the cap fired.
        let timed_out = matches!(output.status.code(), Some(124) | Some(137));
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        let err = String::from_utf8_lossy(&output.stderr);
        if !err.trim().is_empty() {
            text.push_str("\n--- stderr ---\n");
            text.push_str(&err);
        }
        Ok((output.status.success(), timed_out, text))
    }

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
        self.get_candidate_logs_tail(100)
    }

    /// Container logs with an explicit tail bound. The security stage scans
    /// a large tail for the secret, so a startup banner cannot scroll out
    /// of a diagnostics-sized window first.
    pub fn get_candidate_logs_tail(&self, tail: usize) -> String {
        let name = self.candidate_container_name();
        let output = Command::new("docker")
            .args(["logs", "--tail", &tail.to_string(), &name])
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
            self.anchor_container_name(),
            self.ref_sql_name(),
            self.egress_proxy_name(),
            self.agent_container_name(),
            format!("subdollar-omp-agent-{}", std::process::id()),
        ];
        names.extend(self.ha_node_names());
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
        Self::prune_labeled_networks_except(&std::collections::HashSet::new())
    }

    /// R53 F3: keep a live campaign's networks regardless of age. The
    /// network carries the same `sdb.run` label as its containers.
    pub fn prune_labeled_networks_except(live_ids: &std::collections::HashSet<String>) {
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
                    "{{len .Containers}} {{.Created}} {{index .Labels \"sdb.run\"}}",
                ])
                .output();
            let text = match detail {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => continue,
            };
            let label = text.split_whitespace().nth(2).unwrap_or("");
            if Self::run_matches_live(label, live_ids) {
                continue;
            }
            if !Self::network_is_stale(&text) {
                continue;
            }
            let _ = Command::new("docker").args(["network", "rm", id]).output();
        }
    }

    /// R53 F3: a running container older than the age limit is reaped only
    /// when its run matches no live campaign/cell. Split out so the age
    /// and the live set are testable without Docker.
    pub fn should_reap_running(
        created: &str,
        run: &str,
        live_ids: &std::collections::HashSet<String>,
    ) -> bool {
        if Self::run_matches_live(run, live_ids) {
            return false;
        }
        Self::container_created_secs(created).is_some_and(|ts| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|now| now.as_secs().saturating_sub(ts) > Self::MAX_RUNNING_ORPHAN_AGE_SECS)
                .unwrap_or(false)
        })
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
            if !stopped && Self::should_reap_running(created, run, live_ids) {
                let rm = Command::new("docker").args(["rm", "-f", id]).output();
                if rm.map(|o| o.status.success()).unwrap_or(false) {
                    reaped += 1;
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
        // The sdb-sql reference is staged in a per-instance temp dir and
        // bind-mounted into its container: once the containers go, the
        // staging copy goes too (round-3 review finding 9).
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("subdollar-ref-sql-src-{}", self.instance_id)),
        );
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
        // The sdb-sql reference staging copy for this instance (finding 9).
        let _ = fs::remove_dir_all(
            std::env::temp_dir().join(format!("subdollar-ref-sql-src-{}", m.instance_id)),
        );
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

    /// R53 F3: a live campaign's old running container is kept; the same
    /// container with no live lock is reaped. Young containers are kept.
    #[test]
    fn r53_f3_sweep_keeps_campaign_containers() {
        let old = (chrono::Utc::now() - chrono::Duration::hours(4)).to_rfc3339();
        let live = live(&["campaign_x_w0"]);
        assert!(
            !SandboxManager::should_reap_running(&old, "campaign_x_w0_s0_full", &live),
            "a live campaign's old container must be kept"
        );
        assert!(
            SandboxManager::should_reap_running(&old, "campaign_y_w0_s0_full", &live),
            "an old container with no live lock is reaped"
        );
        let young = chrono::Utc::now().to_rfc3339();
        assert!(
            !SandboxManager::should_reap_running(&young, "campaign_y_w0_s0_full", &live),
            "the age gate keeps young containers"
        );
        // Networks carry the same sdb.run label and use this match.
        assert!(SandboxManager::run_matches_live("campaign_x_w0", &live));
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
mod tests;

/// Live-Docker coverage suite. Every test uses a `covtest-<pid>-<tag>`
/// instance id, so its containers, networks, and marker files can never
/// collide with a live campaign, and host ports are ephemeral (>20000; live
/// runs stay below 10000). `InstanceCleanup` is a panic-safe belt on top of
/// the exact cleanup/Drop paths the tests themselves exercise.
#[cfg(test)]
mod live_docker_cov_tests {
    use super::*;
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::atomic::Ordering;

    /// Unique-per-test instance id: never shared with a live run.
    fn id_for(tag: &str) -> String {
        format!("covtest-{}-{}", std::process::id(), tag)
    }

    /// Free loopback port outside the live port map (live runs stay <10000)
    /// and outside the privileged range. Also excludes the legacy fixed test
    /// ports (59980..=59999) so parallel tests in this process never steal
    /// `test_start_candidate_in_docker_start_sh_and_dockerfile`'s bind.
    /// Small race window until docker binds it; worst case the launch fails
    /// loudly instead of measuring the wrong thing.
    fn free_port() -> u16 {
        loop {
            let l = TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
            let p = l.local_addr().expect("local addr").port();
            drop(l);
            if p > 20000 && p < 59980 {
                return p;
            }
        }
    }

    fn free_ports(n: usize) -> Vec<u16> {
        let mut out: Vec<u16> = Vec::new();
        while out.len() < n {
            let p = free_port();
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    fn docker(args: &[&str]) -> std::process::Output {
        Command::new("docker")
            .args(args)
            .output()
            .expect("docker CLI must be available")
    }

    fn running(name: &str) -> bool {
        let o = docker(&["inspect", "--format", "{{.State.Running}}", name]);
        o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true"
    }

    fn exists(name: &str) -> bool {
        docker(&["inspect", name]).status.success()
    }

    fn attached_to(name: &str, net: &str) -> bool {
        let o = docker(&[
            "inspect",
            "--format",
            "{{json .NetworkSettings.Networks}}",
            name,
        ]);
        o.status.success() && String::from_utf8_lossy(&o.stdout).contains(net)
    }

    /// Panic-safe teardown for exactly one instance id: label-filtered
    /// reaping (own label value only) plus the exact names this instance can
    /// have created, its own network, and its marker files. Deliberately not
    /// a global sweep, so sibling test processes and live runs are untouched.
    struct InstanceCleanup {
        id: String,
    }

    impl Drop for InstanceCleanup {
        fn drop(&mut self) {
            let filter = format!("label=sdb.run={}", self.id);
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
            let sm = SandboxManager::with_id(&self.id);
            let mut names = sm.owned_container_names();
            names.push(format!("subdollar-tests-{}", self.id));
            let mut args: Vec<String> = vec!["rm".into(), "-f".into()];
            args.extend(names);
            let _ = Command::new("docker").args(&args).output();
            SandboxManager::remove_network_retrying(&sm.network_name());
        }
    }

    /// Round-13 R1: both harnesses take the prompt as a *positional*
    /// argument, so a prompt that starts with `-` (the campaign's harness
    /// note used to start with `---`) is parsed as an unknown flag unless it
    /// follows `--`. The agent then ran 0 turns and the verifier graded the
    /// previous ticket's code. This drives the real parsers in the sandbox
    /// image, in both directions.
    #[test]
    fn live_test_harness_cli_parses_a_dash_leading_prompt_after_separator() {
        let image = "subdollar-sandbox";
        if !Command::new("docker")
            .args(["image", "inspect", image])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            eprintln!("{image} missing; skipping the harness argv live proof");
            return;
        }
        let run = |args: &[&str]| -> String {
            let out = Command::new("docker")
                .args(["run", "--rm", image])
                .args(args)
                .output()
                .expect("docker run");
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        };
        let prompt = "--- harness note (added by the benchmark, not the ticket) ---";
        // omp: the bare positional is a flag (the bug)...
        let bare = run(&["omp", "-p", "--model=foo/bar", prompt]);
        assert!(
            bare.contains("unknown flag"),
            "the live omp parser should reject a bare dash prompt: {bare}"
        );
        // ...and `--` fixes it (the next error is about the model, not argv).
        let fixed = run(&["omp", "-p", "--model=foo/bar", "--", prompt]);
        assert!(
            !fixed.contains("unknown flag"),
            "`omp -p -- <prompt>` must parse the note: {fixed}"
        );
        // opencode: the bare form only prints usage; `--` reaches execution.
        let oc: &[&str] = &[
            "opencode",
            "run",
            "--standalone",
            "--format",
            "json",
            "--session",
            "probe",
            "--title",
            "t",
            "-m",
            "foo/bar",
            "--auto",
        ];
        let mut bare_args = oc.to_vec();
        bare_args.push(prompt);
        let bare = run(&bare_args);
        assert!(
            bare.contains("DESCRIPTION"),
            "the bare opencode form should not run: {bare}"
        );
        let mut fixed_args = oc.to_vec();
        fixed_args.push("--");
        fixed_args.push(prompt);
        let fixed = run(&fixed_args);
        assert!(
            !fixed.contains("DESCRIPTION"),
            "`opencode run ... -- <prompt>` must parse: {fixed}"
        );
    }

    /// ensure_network creates the run network plus the anchor that keeps the
    /// network from looking stale; cleanup reaps the anchor but the age gate
    /// deliberately keeps the young empty network; the explicit own-network
    /// reaper still removes it.
    #[test]
    fn live_test_anchor_network_lifecycle_and_age_gated_cleanup() {
        let id = id_for("anchor");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        sm.cleanup(); // pre-clean leftovers from a killed prior run

        assert!(sm.ensure_network().is_ok());
        let net = sm.network_name();
        assert!(exists(&net), "network must exist after ensure_network");
        let anchor = sm.anchor_container_name();
        assert!(
            running(&anchor),
            "anchor must be running for the run's life"
        );
        assert!(attached_to(&anchor, &net), "anchor must be attached");
        assert!(
            SandboxManager::network_in_use_or_young(&net),
            "attached anchor keeps the network non-sweepable"
        );

        // Idempotent: "already exists" + anchor already running.
        assert!(sm.ensure_network().is_ok());
        assert!(running(&anchor));

        sm.cleanup();
        assert!(!exists(&anchor), "cleanup must reap its anchor");
        assert!(
            exists(&net),
            "young empty network survives cleanup by design (sibling create-then-attach race)"
        );
        // The explicit pool-pressure path removes this instance's own net.
        sm.cleanup_finished_networks();
        assert!(!exists(&net), "explicit own-network reaper must remove it");

        // Unknown network fails closed: never reported as safe to reap.
        assert!(SandboxManager::network_in_use_or_young(&format!(
            "subdollar-net-does-not-exist-{}",
            std::process::id()
        )));
    }

    /// `remove_network_retrying` must force-detach this instance's own
    /// endpoint (rm refuses attached nets) and then succeed.
    #[test]
    fn live_test_remove_network_retrying_disconnects_own_endpoint() {
        let id = id_for("rmnet");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        sm.cleanup();
        assert!(sm.ensure_network().is_ok());
        let net = sm.network_name();
        assert!(running(&sm.anchor_container_name()));
        // First rm sees "active endpoints"; the helper disconnects the
        // inspected names (ours only) and retries.
        SandboxManager::remove_network_retrying(&net);
        assert!(
            !exists(&net),
            "retrying removal must succeed on own live net"
        );
        sm.cleanup();
    }

    /// Deterministic subnet collision: the first-attempt subnet is leased to
    /// the test, so `ensure_network` must bump the wheel and still succeed
    /// instead of failing or launching without a network.
    #[test]
    fn live_test_ensure_network_retries_on_subnet_collision() {
        let id = id_for("subnet");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        sm.cleanup();
        let net = sm.network_name();
        let blocked = SandboxManager::run_subnet_attempt(&net, 0);
        let holder = format!("covtest-subnet-holder-{}", std::process::id());
        let _ = docker(&["network", "rm", holder.as_str()]);
        let created = docker(&["network", "create", "--subnet", &blocked, holder.as_str()]);
        if !created.status.success() {
            // The deterministic attempt-0 subnet is already leased (usually
            // a live run's network): ensure_network hits the same collision
            // branch, so the assertion below still proves the retry.
            eprintln!(
                "holder create skipped, subnet already leased: {}",
                String::from_utf8_lossy(&created.stderr).trim()
            );
        }
        assert!(
            sm.ensure_network().is_ok(),
            "subnet collision must retry, not abort"
        );
        let sn = docker(&[
            "network",
            "inspect",
            &net,
            "--format",
            "{{(index .IPAM.Config 0).Subnet}}",
        ]);
        assert!(sn.status.success());
        assert_ne!(
            String::from_utf8_lossy(&sn.stdout).trim(),
            blocked,
            "network must have moved off the leased subnet"
        );
        let _ = docker(&["network", "rm", holder.as_str()]);
        sm.cleanup();
        SandboxManager::remove_network_retrying(&net);
    }

    /// References are the harness's oracle: on the run network while the
    /// agent works, off it while verification runs (a candidate must not be
    /// able to proxy to `ref-*` and fake stages).
    #[test]
    fn live_test_references_detached_for_verification() {
        let id = id_for("refdetach");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        sm.cleanup();
        assert!(sm.ensure_network().is_ok());
        assert!(sm.start_reference_sql(free_port()).is_ok());

        // A probe container on the run network resolves the alias…
        let ws = std::env::temp_dir().join(format!("covtest-refdetach-ws-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).expect("workspace");
        let port = free_port();
        fs::write(
            ws.join("start.sh"),
            format!("#!/bin/bash\nexec python3 -m http.server {port} --bind 0.0.0.0\n"),
        )
        .expect("start.sh");
        let extras = CandidateRunExtras {
            tmpfs: Vec::new(),
            env: Vec::new(),
            ports: Vec::new(),
            memory: None,
            cpuset: None,
            binds: Vec::new(),
            launch_args: Vec::new(),
            pids_limit: None,
        };
        assert!(sm
            .start_candidate_in_docker_with(&ws, port, &extras)
            .is_ok());
        assert!(sm.connect_candidate_to_network().is_ok());
        let candidate = sm.candidate_container_name();
        let probe = |sm: &SandboxManager| -> bool {
            let out = docker(&[
                "exec",
                sm.candidate_container_name().as_str(),
                "python3",
                "-c",
                "import socket; socket.create_connection(('ref-sql', 5480), 2)",
            ]);
            out.status.success()
        };
        let _ = &candidate;
        assert!(probe(&sm), "ref-sql must resolve while the agent works");

        sm.detach_references();
        assert!(
            !probe(&sm),
            "ref-sql must be unreachable during verification"
        );

        sm.reattach_references();
        assert!(probe(&sm), "ref-sql must come back for the bench");

        sm.cleanup();
        let _ = fs::remove_dir_all(&ws);
    }

    /// Candidate launch knobs (tmpfs/env/ports/memory) land on the
    /// container; kill leaves it stopped-but-present, restart revives it,
    /// stop removes it; logs and the network alias path work.
    #[test]
    fn live_test_candidate_extras_kill_restart_stop_and_alias() {
        let id = id_for("cand");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        sm.cleanup();

        let ws = std::env::temp_dir().join(format!("covtest-cand-ws-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).expect("create workspace");
        // Host bind mount: the disk-residency/compaction stages read the
        // on-disk footprint through a `-v host:/bound` mount, so the
        // container must be able to write into it.
        let bind_host =
            std::env::temp_dir().join(format!("covtest-cand-bind-{}", std::process::id()));
        let _ = fs::remove_dir_all(&bind_host);
        fs::create_dir_all(&bind_host).expect("create bind dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&bind_host, fs::Permissions::from_mode(0o777));
        }
        let port = free_port();
        let extra_port = free_port();
        fs::write(
            ws.join("start.sh"),
            format!(
                "#!/bin/bash\necho COVTEST_CANDIDATE_UP\nmkdir -p /bound && echo hello-from-container > /bound/from-container.txt\nexec python3 -m http.server {port} --bind 0.0.0.0\n"
            ),
        )
        .expect("write start.sh");

        let extras = CandidateRunExtras {
            tmpfs: vec!["/data:size=16m".to_string()],
            env: vec![
                ("SDB_DATA_DIR".to_string(), "/data".to_string()),
                ("COVTEST_ENV".to_string(), "cov-value".to_string()),
            ],
            ports: vec![extra_port],
            memory: Some("128m".to_string()),
            cpuset: None,
            launch_args: Vec::new(),
            binds: vec![(
                bind_host.to_string_lossy().to_string(),
                "/bound".to_string(),
            )],
            pids_limit: None,
        };
        assert!(sm
            .start_candidate_in_docker_with(&ws, port, &extras)
            .is_ok());
        let name = sm.candidate_container_name();
        assert!(running(&name));

        let mem = docker(&["inspect", "--format", "{{.HostConfig.Memory}}", &name]);
        assert_eq!(String::from_utf8_lossy(&mem.stdout).trim(), "134217728");
        let envs = docker(&["inspect", "--format", "{{json .Config.Env}}", &name]);
        let envs = String::from_utf8_lossy(&envs.stdout);
        assert!(envs.contains("COVTEST_ENV=cov-value"), "{envs}");
        assert!(envs.contains("SDB_DATA_DIR=/data"), "{envs}");
        let tmpfs = docker(&["inspect", "--format", "{{json .HostConfig.Tmpfs}}", &name]);
        assert!(String::from_utf8_lossy(&tmpfs.stdout).contains("/data"));
        let ports = docker(&[
            "inspect",
            "--format",
            "{{json .NetworkSettings.Ports}}",
            &name,
        ]);
        let ports = String::from_utf8_lossy(&ports.stdout);
        assert!(
            ports.contains(&format!("{extra_port}/tcp")),
            "dashboard port must be published 1:1: {ports}"
        );

        let binds = docker(&["inspect", "--format", "{{json .HostConfig.Binds}}", &name]);
        let binds = String::from_utf8_lossy(&binds.stdout);
        assert!(
            binds.contains("/bound"),
            "bind mount must reach the container: {binds}"
        );
        let logs = sm.get_candidate_logs_tail(50);
        assert!(logs.contains("COVTEST_CANDIDATE_UP"), "{logs}");
        // Round-trip: a file the container writes under the bind appears on
        // the host (the footprint measurement path).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(got) = fs::read_to_string(bind_host.join("from-container.txt")) {
                assert!(got.contains("hello-from-container"), "{got}");
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("container never wrote into the host bind mount");
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        // Candidate accepts on its published loopback port (docker-proxy
        // handshake + backend liveness heuristic).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        assert!(
            rt.block_on(sm.wait_for_port(port, 10)),
            "candidate must accept on the published port"
        );

        // kill -9 semantics: stopped but present, restartable.
        assert!(sm.kill_candidate().is_ok());
        assert!(!running(&name), "SIGKILL leaves the container stopped");
        assert!(exists(&name), "kill -9 must not remove the container");
        assert!(sm.restart_candidate().is_ok());
        assert!(running(&name));

        // Attach to the run network under the `candidate` alias; the second
        // call exercises the already-connected arm.
        assert!(sm.ensure_network().is_ok());
        assert!(sm.connect_candidate_to_network().is_ok());
        assert!(sm.connect_candidate_to_network().is_ok());
        assert!(
            attached_to(&name, &sm.network_name()),
            "candidate must be attached under its alias"
        );

        assert!(sm.stop_candidate().is_ok());
        assert!(!exists(&name));

        // ENOSPC wrapper: 8MB tmpfs data volume + SDB_DATA_DIR env.
        assert!(sm.start_candidate_enospc(&ws, free_port()).is_ok());
        let tmpfs = docker(&["inspect", "--format", "{{json .HostConfig.Tmpfs}}", &name]);
        let tmpfs = String::from_utf8_lossy(&tmpfs.stdout);
        assert!(tmpfs.contains("\"/data\""), "{tmpfs}");
        assert!(tmpfs.contains("size=8m"), "{tmpfs}");
        let _ = sm.stop_candidate();
        sm.cleanup();
        SandboxManager::remove_network_retrying(&sm.network_name());
        let _ = fs::remove_dir_all(&ws);
    }

    /// Three-node HA: leader/replica launch args, dashboard port on node 1
    /// only, kill (crash) keeps the container, respawn re-points replicaof,
    /// stop removes the cluster.
    #[test]
    fn live_test_ha_nodes_start_kill_respawn_stop() {
        let id = id_for("ha");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        sm.cleanup();

        let ws = std::env::temp_dir().join(format!("covtest-ha-ws-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).expect("create ha workspace");
        fs::write(
            ws.join("start.sh"),
            "#!/bin/bash\necho HA_NODE $@\nexec sleep 120\n",
        )
        .expect("write ha start.sh");

        let ports = free_ports(3);
        let dash = free_port();
        assert!(
            sm.start_ha_nodes(&ws, &ports[..1], None).is_err(),
            "HA needs exactly 3 node ports"
        );
        assert!(sm.start_ha_nodes(&ws, &ports, Some(dash)).is_ok());
        let net = sm.network_name();
        for idx in 1..=3u8 {
            let n = sm.ha_node_container_name(idx);
            assert!(running(&n), "ha{idx} must run");
            assert!(attached_to(&n, &net), "ha{idx} must join the run net");
        }
        // Fixture echoes its args: node 2 must point at ha1, node 1 must not.
        let logs2 = docker(&["logs", sm.ha_node_container_name(2).as_str()]);
        let text2 = format!(
            "{}{}",
            String::from_utf8_lossy(&logs2.stdout),
            String::from_utf8_lossy(&logs2.stderr)
        );
        assert!(
            text2.contains(&format!(
                "--port {} --data /data2 --replicaof ha1:{}",
                ports[1], ports[0]
            )),
            "{text2}"
        );
        let logs1 = docker(&["logs", sm.ha_node_container_name(1).as_str()]);
        let text1 = format!(
            "{}{}",
            String::from_utf8_lossy(&logs1.stdout),
            String::from_utf8_lossy(&logs1.stderr)
        );
        assert!(text1.contains(&format!("--port {} --data /data1", ports[0])));
        assert!(!text1.contains("--replicaof"), "{text1}");
        let ports1 = docker(&[
            "inspect",
            "--format",
            "{{json .NetworkSettings.Ports}}",
            sm.ha_node_container_name(1).as_str(),
        ]);
        assert!(String::from_utf8_lossy(&ports1.stdout).contains(&format!("{dash}/tcp")));
        let ports3 = docker(&[
            "inspect",
            "--format",
            "{{json .NetworkSettings.Ports}}",
            sm.ha_node_container_name(3).as_str(),
        ]);
        assert!(!String::from_utf8_lossy(&ports3.stdout).contains(&format!("{dash}/tcp")));

        // Crash node 2: container persists, is restartable via respawn.
        assert!(sm.kill_ha_node(2).is_ok());
        assert!(!running(&sm.ha_node_container_name(2)));
        assert!(exists(&sm.ha_node_container_name(2)));
        assert!(sm.kill_ha_node(9).is_err(), "missing node must report Err");
        // Rejoin: node 2 recreated pointed at the new leader alias ha3.
        assert!(sm
            .respawn_ha_node(&ws, 2, ports[1], Some(("ha3", ports[2])))
            .is_ok());
        assert!(running(&sm.ha_node_container_name(2)));
        let logs2b = docker(&["logs", sm.ha_node_container_name(2).as_str()]);
        let text2b = format!(
            "{}{}",
            String::from_utf8_lossy(&logs2b.stdout),
            String::from_utf8_lossy(&logs2b.stderr)
        );
        assert!(
            text2b.contains(&format!("--replicaof ha3:{}", ports[2])),
            "{text2b}"
        );

        sm.stop_ha_nodes(3);
        for idx in 1..=3u8 {
            assert!(!exists(&sm.ha_node_container_name(idx)));
        }
        sm.cleanup();
        SandboxManager::remove_network_retrying(&net);
        let _ = fs::remove_dir_all(&ws);
    }

    /// Egress sidecar: starts, disarms the ephemeral handle's Drop (the run
    /// owner keeps it), joins the run network under its alias, stops cleanly.
    #[test]
    fn live_test_egress_proxy_start_disarm_alias_and_stop() {
        let id = id_for("egress");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        sm.cleanup();
        let url = sm.start_egress_proxy();
        assert_eq!(url.as_deref(), Some("http://egress-proxy:3128"));
        let name = sm.egress_proxy_name();
        assert!(running(&name));
        assert!(attached_to(&name, &sm.network_name()));
        assert!(
            !sm.needs_cleanup.load(Ordering::SeqCst),
            "ephemeral handle must not claim cleanup ownership of the sidecar"
        );
        sm.stop_egress_proxy();
        assert!(!exists(&name));
        SandboxManager::remove_network_retrying(&sm.network_name());
    }

    /// tests/CI stage seam: candidate command runs offline in the sandbox
    /// image, exit status is reported, and the container never leaks.
    #[test]
    fn live_test_run_workspace_command_reports_status_and_reaps() {
        let id = id_for("ws-cmd");
        let _cleanup = InstanceCleanup::new_guard(&id);
        let sm = SandboxManager::with_id(&id);
        let ws = std::env::temp_dir().join(format!("covtest-wscmd-ws-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).expect("create workspace");
        fs::write(ws.join("marker.txt"), "covtest-marker\n").expect("write marker");

        let (ok, out) = sm
            .run_workspace_command(&ws, "cat marker.txt", 60)
            .expect("run must execute");
        assert!(ok, "{out}");
        assert!(out.contains("covtest-marker"), "{out}");

        let (bad, _) = sm
            .run_workspace_command(&ws, "exit 3", 60)
            .expect("run must execute");
        assert!(!bad, "nonzero exit must be reported");
        assert!(
            !exists(&format!("subdollar-tests-{}", id)),
            "container leaks"
        );
        let _ = fs::remove_dir_all(&ws);
    }

    /// R81 F3: the `.47` entrypoint is `./start.sh` at the root only.
    /// A Dockerfile, a nested start.sh, or a language entrypoint is not
    /// a runnable candidate.
    #[test]
    fn test_ensure_runnable_candidate_start_sh_only() {
        let base = std::env::temp_dir().join(format!("covtest_runnable_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let sm = SandboxManager::with_id("covtest-unit");
        fs::create_dir_all(&base).expect("create base");
        assert!(!sm.ensure_runnable_candidate(&base).expect("scan"));

        fs::write(base.join("Dockerfile"), "FROM alpine\n").expect("write dockerfile");
        assert!(!sm.ensure_runnable_candidate(&base).expect("scan"));
        let _ = fs::remove_file(base.join("Dockerfile"));

        let nested = base.join("project");
        fs::create_dir_all(&nested).expect("create nested");
        fs::write(
            nested.join("start.sh"),
            "#!/bin/sh\nexec ./target/release/s\n",
        )
        .expect("write nested start");
        assert!(!sm.ensure_runnable_candidate(&base).expect("scan"));

        fs::write(
            base.join("start.sh"),
            "#!/bin/sh\nexec ./target/release/s\n",
        )
        .expect("write start");
        assert!(sm.ensure_runnable_candidate(&base).expect("scan"));
        let _ = fs::remove_dir_all(&base);
    }

    /// R81 F3: the workspace contract. Empty dependency tables are fine;
    /// any declared dependency, patch/replace table, missing start.sh or
    /// a start.sh that does not run the built binary is not.
    #[test]
    fn test_rust_workspace_contract() {
        let base = std::env::temp_dir().join(format!("covtest_rustws_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("create base");
        assert!(SandboxManager::rust_workspace_violation(&base)
            .expect("no Cargo.toml")
            .contains("Cargo.toml"));
        let cargo = "[package]\nname = \"sdb\"\nversion = \"0.1.0\"\n\n\
                     [dependencies]\n\n[dev-dependencies]\n";
        fs::write(base.join("Cargo.toml"), cargo).expect("write cargo");
        assert!(SandboxManager::rust_workspace_violation(&base)
            .expect("no start.sh")
            .contains("start.sh"));
        fs::write(
            base.join("start.sh"),
            "#!/bin/sh\nexec ./target/release/sdb \"$@\"\n",
        )
        .expect("write start");
        assert_eq!(SandboxManager::rust_workspace_violation(&base), None);

        fs::write(
            base.join("Cargo.toml"),
            "[package]\nname = \"sdb\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .expect("write dep cargo");
        let why = SandboxManager::rust_workspace_violation(&base).expect("dep");
        assert!(why.contains("dependencies"), "{why}");

        fs::write(
            base.join("Cargo.toml"),
            "[package]\nname = \"sdb\"\n\n[dependencies.foo]\npath = \"foo\"\n",
        )
        .expect("write dep table");
        let why = SandboxManager::rust_workspace_violation(&base).expect("dep table");
        assert!(why.contains("dependency table"), "{why}");

        fs::write(
            base.join("Cargo.toml"),
            "[package]\nname = \"sdb\"\n\n[patch.crates-io]\nfoo = { path = \"foo\" }\n",
        )
        .expect("write patch");
        let why = SandboxManager::rust_workspace_violation(&base).expect("patch");
        assert!(why.contains("[patch.crates-io]"), "{why}");

        fs::write(
            base.join("Cargo.toml"),
            "[package]\nname = \"sdb\"\n\n[target.'cfg(unix)'.dependencies]\nfoo = \"1\"\n",
        )
        .expect("write target dep");
        let why = SandboxManager::rust_workspace_violation(&base).expect("target dep");
        assert!(why.contains("dependency table"), "{why}");

        fs::write(base.join("Cargo.toml"), cargo).expect("restore cargo");
        fs::write(base.join("start.sh"), "#!/bin/sh\ncargo run --release\n")
            .expect("write cargo run");
        let why = SandboxManager::rust_workspace_violation(&base).expect("cargo run");
        assert!(why.contains("target/release"), "{why}");
        fs::write(base.join("start.sh"), "#!/bin/sh\n./target/release/sdb\n")
            .expect("write no exec");
        let why = SandboxManager::rust_workspace_violation(&base).expect("no exec");
        assert!(why.contains("exec"), "{why}");

        let _ = fs::remove_dir_all(&base);
    }

    /// R81 F3: the build-failure line names the first compiler error, not
    /// the whole log; a log without one falls back to the tail.
    #[test]
    fn test_first_build_error() {
        let log = "   Compiling sdb v0.1.0\nwarning: unused\nerror[E0425]: cannot find value `x`\n --> src/main.rs:3:5\n  |\n3 | x\n  | ^\n";
        let first = SandboxManager::first_build_error(log);
        assert!(first.starts_with("error[E0425]"), "{first}");
        assert!(!first.contains("Compiling"), "{first}");
        let no_error = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let tail = SandboxManager::first_build_error(no_error);
        assert!(tail.ends_with("j"), "{tail}");
        assert!(!tail.contains('a'), "{tail}");
    }

    /// Workdir canonicalization: absolute stays verbatim; an unusable
    /// relative path falls back to tempdir join (never panics).
    #[test]
    fn test_canonical_workdir_for_absolute_and_fallback() {
        use std::path::PathBuf;
        let abs = Path::new("/tmp/covtest-abs");
        assert_eq!(
            SandboxManager::canonical_workdir_for(abs),
            PathBuf::from("/tmp/covtest-abs")
        );
        let missing = Path::new("covtest-definitely-missing-dir");
        assert_eq!(
            SandboxManager::canonical_workdir_for(missing),
            std::env::temp_dir().join("covtest-definitely-missing-dir")
        );
    }

    impl InstanceCleanup {
        fn new_guard(id: &str) -> Self {
            Self { id: id.to_string() }
        }
    }
}
