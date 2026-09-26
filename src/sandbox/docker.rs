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
pub const REF_NGINX_IMAGE: &str = "docker.io/library/nginx@sha256:17ad11d84df6c69e327c0894125f712ec1f1de627b5e5ed7e12ca2ed8cd5daf8";
pub const REF_DNS_IMAGE: &str = "docker.io/coredns/coredns@sha256:7efd3c635b03efd68c4e8398fc45f0d993d0e9ab016f72c1cefb0fd6d01aa286";
/// Benchmark *client* images are pinned too: the client binary is half the
/// measurement apparatus.
pub const BENCH_REDIS_CLIENT_IMAGE: &str = REF_REDIS_IMAGE;
pub const BENCH_WRK_IMAGE: &str = "docker.io/williamyeh/wrk@sha256:78adc0d9d51a99e6759e702a08d03eaece81c890ffcc9790ef9e5b199d54f091";
/// Fixed upstream ports for the proxy task (also baked into tasks/proxy/prompt.md).
pub const PROXY_UPSTREAM_PORTS: [u16; 3] = [8101, 8102, 8103];

/// Reference image for a task name ("redis"/"http"/"dns"), if it has one.
pub fn ref_image_for_task(task: &str) -> Option<&'static str> {
    match task {
        "redis" => Some(REF_REDIS_IMAGE),
        "http" => Some(REF_NGINX_IMAGE),
        "dns" => Some(REF_DNS_IMAGE),
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

    pub fn ref_redis_name(&self) -> String {
        format!("subdollar-ref-redis-{}", self.instance_id)
    }

    pub fn ref_http_name(&self) -> String {
        format!("subdollar-ref-http-{}", self.instance_id)
    }

    pub fn ref_dns_name(&self) -> String {
        format!("subdollar-ref-dns-{}", self.instance_id)
    }

    pub fn network_name(&self) -> String {
        format!("subdollar-net-{}", self.instance_id)
    }

    /// Idempotently create this run's private Docker network.
    ///
    /// Reference containers join it under stable DNS aliases (`ref-redis`,
    /// `ref-http`, `ref-dns`) and the OMP agent container is attached to it,
    /// so the agent can probe ground truth by hostname while remaining
    /// isolated from host-network services.
    pub fn ensure_network(&self) -> Result<()> {
        self.needs_cleanup.store(true, Ordering::SeqCst);
        let net = self.network_name();
        let out = Command::new("docker")
            .args(["network", "create", &net])
            .output()?;
        if out.status.success() {
            info!("Created run network {}", net);
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if stderr.contains("already exists") {
            return Ok(());
        }
        if stderr.contains("could not find an available, non-overlapping IPv4 address pool")
            || stderr.contains("Pool overlaps with other one on this address space")
        {
            warn!("Network pool exhausted. Attempting to prune stale subdollar networks and retrying...");
            Self::prune_stale_networks();
            let retry_out = Command::new("docker")
                .args(["network", "create", &net])
                .output()?;
            if retry_out.status.success()
                || String::from_utf8_lossy(&retry_out.stderr).contains("already exists")
            {
                info!("Created run network {} after pruning stale networks", net);
                return Ok(());
            }
        }
        Err(anyhow!("docker network create failed: {}", stderr))
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
                &format!("{}:6379", port),
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
                &format!("{}:80", port),
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

    fn start_upstream_on(&self, idx: u8, port: u16, net: &str) -> Result<()> {
        let name = self.upstream_name(idx);
        fs::write(
            self.upstream_marker_path(idx),
            format!("upstream-{} marker\n", idx),
        )?;
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &name,
                "--network",
                net,
                "--network-alias",
                &format!("upstream{}", idx),
                "--memory=512m",
                "--cpus=1.0",
                "-p",
                &format!("{}:80", port),
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
            .args(["stop", &self.upstream_name(idx)])
            .output()?;
        Ok(())
    }

    pub fn start_reference_dns(&self, port: u16) -> Result<()> {
        let name = self.ref_dns_name();
        let net = self.network_name();
        self.ensure_network()?;
        info!(
            "Starting reference DNS server in Docker on UDP port {} ({}, alias ref-dns on {})",
            port, name, net
        );
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        // The stock coredns image defaults to the `whoami` plugin, which echoes the
        // client's own address for every query instead of resolving. Mount a
        // forwarding Corefile so the reference behaves like a real recursive
        // resolver that the agent can inspect black-box.
        let corefile = Self::coredns_corefile_path(&self.instance_id);
        let _ = fs::write(
            &corefile,
            ".:53 {\n    errors\n    log\n    forward . /etc/resolv.conf\n    cache 30\n}\n",
        );

        let _ = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "--network",
                &net,
                "--network-alias",
                "ref-dns",
                "--memory=512m",
                "--cpus=1.0",
                "-p",
                &format!("{}:53/udp", port),
                "-v",
                &format!("{}:/Corefile:ro", corefile.display()),
                REF_DNS_IMAGE,
                "-conf",
                "/Corefile",
            ])
            .output();

        Ok(())
    }

    fn coredns_corefile_path(instance_id: &str) -> PathBuf {
        std::env::temp_dir().join(format!("subdollar-coredns-{}.corefile", instance_id))
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
    pub fn start_candidate_in_docker(&self, workdir: &Path, port: u16) -> Result<()> {
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

        let port_tcp = format!("{}:{}", port, port);
        let port_udp = format!("{}:{}/udp", port, port);

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
                    if let Ok(run_out) = run_res {
                        if !run_out.status.success() {
                            warn!(
                                "Failed to start custom container: {}",
                                String::from_utf8_lossy(&run_out.stderr)
                            );
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
        let _ = Command::new("docker")
            .args([
                "run",
                "-d",
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
            .output();

        Ok(())
    }

    /// Actively poll the target TCP port until the candidate is ready, up to timeout_secs
    pub async fn wait_for_port(&self, port: u16, timeout_secs: u64) -> bool {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);
        while start.elapsed() < timeout {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                // Give a 500ms stabilization window
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
                .args(["network", "inspect", id, "--format", "{{len .Containers}}"])
                .output();
            let count_out = match inspect {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => continue, // Inspect failed: fail closed, skip the network.
            };
            if !Self::network_is_detached(&count_out) {
                continue;
            }
            let _ = Command::new("docker").args(["network", "rm", id]).output();
        }
    }

    /// Parse `docker network ls -q` output into non-empty network IDs.
    fn parse_network_ids(output: &str) -> Vec<&str> {
        output.split_whitespace().collect()
    }

    /// True only when `docker network inspect --format '{{len .Containers}}'`
    /// output parses to exactly 0. Anything else (attached containers or
    /// unparseable output) means "do not touch".
    fn network_is_detached(count_output: &str) -> bool {
        matches!(count_output.trim().parse::<u64>(), Ok(0))
    }

    pub fn cleanup(&self) {
        self.needs_cleanup.store(false, Ordering::SeqCst);
        let cand = self.candidate_container_name();
        let ref_redis = self.ref_redis_name();
        let ref_http = self.ref_http_name();
        let ref_dns = self.ref_dns_name();
        let net = self.network_name();
        let agent_name = self.agent_container_name();
        let agent_pid = format!("subdollar-omp-agent-{}", std::process::id());
        let mut rm_args: Vec<&str> = vec![
            "rm",
            "-f",
            &cand,
            &ref_redis,
            &ref_http,
            &ref_dns,
            &agent_name,
            &agent_pid,
        ];
        if self.instance_id == "default" {
            rm_args.extend_from_slice(&[
                "subdollar-ref-redis",
                "subdollar-ref-http",
                "subdollar-ref-dns",
                "subdollar-candidate",
                "subdollar-omp-agent",
            ]);
        }
        let _ = Command::new("docker").args(&rm_args).output();

        let clean = self.instance_id.replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
            "",
        );
        if !clean.is_empty() && clean != "default" {
            let _ = Command::new("sh")
                .args([
                    "-c",
                    &format!(
                        "docker ps -a --format '{{{{.Names}}}}' | grep -E '^subdollar-.*-{}' | xargs -r docker rm -f",
                        clean
                    ),
                ])
                .output();
        }

        Self::remove_network_retrying(&net);
        let _ = fs::remove_file(Self::coredns_corefile_path(&self.instance_id));
        for idx in 1..=8u8 {
            let _ = fs::remove_file(self.upstream_marker_path(idx));
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
mod tests {
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
    }

    #[test]
    fn test_docker_cleanup_and_logs() {
        let sm = SandboxManager::with_id("test-cleanup");
        sm.cleanup();
        let logs = sm.get_candidate_logs();
        assert!(!logs.is_empty());
    }

    #[test]
    fn test_start_and_cleanup_reference_containers() {
        let sm = SandboxManager::with_id("test-ref");
        let res_redis = sm.start_reference_redis(59994);
        assert!(res_redis.is_ok());
        let res_http = sm.start_reference_http(59993);
        assert!(res_http.is_ok());
        let res_dns = sm.start_reference_dns(59992);
        assert!(res_dns.is_ok());
        sm.cleanup();
    }

    #[test]
    fn test_run_network_reference_alias_reachable() {
        let sm = SandboxManager::with_id("net-alias-test");
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
    fn test_start_candidate_in_docker_start_sh_and_dockerfile() {
        let temp_dir = std::env::temp_dir().join(format!("test_docker_run_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let sm = SandboxManager::with_id("test-cand");

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
        // The prune gate: only a zero-container network may be removed.
        assert!(SandboxManager::network_is_detached("0"));
        assert!(SandboxManager::network_is_detached("0\n"));
        assert!(!SandboxManager::network_is_detached("1"));
        assert!(!SandboxManager::network_is_detached("3\n"));
        // Unparseable inspect output fails closed: skip, never delete.
        assert!(!SandboxManager::network_is_detached(""));
        assert!(!SandboxManager::network_is_detached("<no value>"));
        assert!(!SandboxManager::network_is_detached("template parsing error"));
    }

    #[test]
    fn test_prune_parses_network_id_list() {
        assert!(SandboxManager::parse_network_ids("").is_empty());
        assert!(SandboxManager::parse_network_ids("   \n").is_empty());
        let ids = SandboxManager::parse_network_ids("abc123\ndef456\n");
        assert_eq!(ids, vec!["abc123", "def456"]);
    }
}
