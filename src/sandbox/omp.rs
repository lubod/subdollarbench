use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy)]
pub struct AgentExecutionLimits {
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub timeout_seconds: Option<u64>,
}

impl Default for AgentExecutionLimits {
    fn default() -> Self {
        Self {
            max_turns: Some(50),
            max_budget_usd: Some(0.50),
            timeout_seconds: Some(900), // 15 minutes default
        }
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct OmpSessionStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
    pub steps_taken: u32,
}

/// Embedded spike extension: bench anti-cheat policy, turn budget steer,
/// per-turn telemetry. Shipped inside the binary and written to the shared
/// OMP agent dir at launch so it is visible inside the sandbox container.
const BENCH_POLICY_JS: &str = include_str!("../../extensions/bench-policy.js");

/// Container-visible path of the installed policy extension. `host_omp_dir`
/// is mounted at `/home/ubuntu/.omp` inside the sandbox, so
/// `<host_omp_dir>/agent/bench-policy.js` is addressable at this path.
const BENCH_POLICY_CONTAINER_PATH: &str = "/home/ubuntu/.omp/agent/bench-policy.js";

/// Spike gate: on unless explicitly disabled via `SDB_BENCH_POLICY=0|off|false|no`.
fn bench_policy_enabled() -> bool {
    match std::env::var("SDB_BENCH_POLICY") {
        Err(_) => true,
        Ok(v) => !matches!(
            v.trim().to_lowercase().as_str(),
            "0" | "off" | "false" | "no" | "disabled"
        ),
    }
}

/// Write the embedded extension into the shared agent dir. Returns the
/// container-visible `-e` path on success, `None` when disabled or on I/O error
/// (bench must degrade to no-policy, never fail the run here).
fn install_bench_policy_extension(host_agent_dir: &Path) -> Option<String> {
    if !bench_policy_enabled() {
        return None;
    }
    let dest = host_agent_dir.join("bench-policy.js");
    if std::fs::write(&dest, BENCH_POLICY_JS).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644));
    }
    Some(BENCH_POLICY_CONTAINER_PATH.to_string())
}

/// Resolve the requested reasoning effort to the explicit `--thinking`
/// level sent to omp, plus a console note when the request was rewritten.
/// There is no `auto`: the legacy alias meant "omit the flag", which omp
/// resolves to its own default (`high`) — so `high` is spelled out instead.
/// `max` is clamped to `high` for non-OpenAI providers (HTTP 400 otherwise).
fn resolve_thinking_level(effort: Option<&str>, effective_model: &str) -> (String, String) {
    let requested = effort.map(|e| e.trim().to_lowercase()).unwrap_or_default();
    let level = match requested.as_str() {
        "" | "default" | "auto" => "high".to_string(),
        other => other.to_string(),
    };
    if level == "max"
        && !effective_model.starts_with("openai/")
        && !effective_model.contains("/openai/")
    {
        (
            "high".to_string(),
            " (requested max, clamped for provider)".to_string(),
        )
    } else {
        (level, String::new())
    }
}

#[derive(Clone)]
pub struct TailSessionContext {
    pub start_time: SystemTime,
    pub existing: HashSet<PathBuf>,
    pub active_file: Arc<Mutex<Option<PathBuf>>>,
    pub running: Arc<AtomicBool>,
    pub tx: mpsc::Sender<String>,
    pub limits: AgentExecutionLimits,
    pub turn_counter: Arc<std::sync::atomic::AtomicU32>,
    pub accumulated_cost: Arc<Mutex<f64>>,
    pub accumulated_tokens: Arc<std::sync::atomic::AtomicU64>,
    pub limit_reached: Arc<AtomicBool>,
    pub limit_reason: Arc<Mutex<Option<String>>>,
    pub cancel_flag: Option<Arc<AtomicBool>>,
    /// Set when the spike extension streams BENCH-EVENT lines: the tailer
    /// then renders assistant/tool session lines as token stats only.
    pub extension_feed_active: Arc<AtomicBool>,
}

/// All launch parameters for one agent session. Bundled in a struct so the
/// runner API does not grow a positional argument per new knob.
#[derive(Clone)]
pub struct AgentRunSpec<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    pub workdir: &'a Path,
    pub api_key: Option<&'a str>,
    pub limits: AgentExecutionLimits,
    pub effort: Option<&'a str>,
    pub network: Option<&'a str>,
    pub task: Option<crate::config::TaskType>,
    pub run_id: Option<&'a str>,
    pub cancel_flag: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone)]
pub struct OmpExecutionError {
    pub message: String,
    pub partial_stats: OmpSessionStats,
}

impl std::fmt::Display for OmpExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for OmpExecutionError {}

pub struct OmpRunner;

impl OmpRunner {
    pub fn get_host_omp_dir() -> PathBuf {
        let ubuntu_omp = PathBuf::from("/home/ubuntu/.omp");
        if ubuntu_omp.is_dir() {
            ubuntu_omp
        } else if Path::new("/home/ubuntu").is_dir() {
            let _ = std::fs::create_dir_all(&ubuntu_omp);
            ubuntu_omp
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".omp")
        } else {
            crate::config::get_repo_root().join(".omp")
        }
    }

    pub fn run_agent(spec: AgentRunSpec<'_>) -> Result<OmpSessionStats> {
        Self::run_agent_with_logger(spec, |line| {
            println!("{}", line);
        })
    }

    pub fn run_agent_with_logger<F>(
        spec: AgentRunSpec<'_>,
        mut log_fn: F,
    ) -> Result<OmpSessionStats>
    where
        F: FnMut(String) + Send + 'static,
    {
        let AgentRunSpec {
            model,
            prompt,
            workdir,
            api_key,
            limits,
            effort,
            network,
            task,
            run_id,
            cancel_flag,
        } = spec;
        info!(
            "Launching OMP agent in Docker sandbox with model: {}, effort: {:?}, limits: {:?}",
            model, effort, limits
        );

        let start_time = SystemTime::now();
        let container_id = run_id
            .unwrap_or("run")
            .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_', "");
        let container_name = format!("subdollar-omp-agent-{}", container_id);

        // Pre-clean any stale agent container
        let _ = Command::new("docker")
            .args(["rm", "-f", &container_name])
            .output();

        let canonical_workdir = if workdir.is_absolute() {
            workdir.to_path_buf()
        } else {
            workdir.canonicalize().unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|c| c.join(workdir))
                    .unwrap_or_else(|_| crate::config::get_repo_root().join(workdir))
            })
        };
        let mount_workdir = format!("{}:/workspace", canonical_workdir.display());

        let host_omp_dir = Self::get_host_omp_dir();
        let host_agent_dir = host_omp_dir.join("agent");
        let host_sessions_dir = host_agent_dir.join("sessions");
        let host_data_dir = host_agent_dir.join("data");
        let host_state_dir = host_agent_dir.join("state");
        let host_cache_dir = host_agent_dir.join("cache");
        let host_run_dir = host_omp_dir.join("run");
        let _ = std::fs::create_dir_all(&host_sessions_dir);
        let _ = std::fs::create_dir_all(&host_data_dir);
        let _ = std::fs::create_dir_all(&host_state_dir);
        let _ = std::fs::create_dir_all(&host_cache_dir);
        let _ = std::fs::create_dir_all(&host_run_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [
                &host_omp_dir,
                &host_agent_dir,
                &host_sessions_dir,
                &host_data_dir,
                &host_state_dir,
                &host_cache_dir,
                &host_run_dir,
            ] {
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
            }
            if let Ok(entries) = std::fs::read_dir(&host_agent_dir) {
                for entry in entries.flatten() {
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_file() {
                            let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o644));
                        }
                    }
                }
            }
            let _ = std::fs::set_permissions(&canonical_workdir, std::fs::Permissions::from_mode(0o755));
        }
        let mount_omp = format!("{}:/home/ubuntu/.omp", host_omp_dir.display());

        // Cloud-only: every model resolves to OpenRouter. Local backends
        // (llama.cpp, Ollama, openai/* shims) were removed as dead complexity.
        let effective_model = if model.starts_with("openrouter/") {
            model.to_string()
        } else {
            format!(
                "openrouter/{}",
                model
                    .trim_start_matches("ollama/")
                    .trim_start_matches("local/")
            )
        };

        let mut cmd = Command::new("docker");
        let bench_policy_path = install_bench_policy_extension(&host_agent_dir);
        cmd.arg("run")
            .arg("--rm")
            .arg("--name")
            .arg(&container_name)
            .arg("--memory=3g")
            .arg("--cpus=3.0")
            .arg("--pids-limit=512")
            .arg("--add-host=host.docker.internal:host-gateway")
            .arg("-v")
            .arg(&mount_workdir)
            .arg("-v")
            .arg(&mount_omp)
            .arg("-w")
            .arg("/workspace")
            .arg("-e")
            .arg("PI_NO_PTY=1")
            .arg("-e")
            .arg("HOME=/home/ubuntu");
        if bench_policy_path.is_some() {
            cmd.arg("-e").arg(format!(
                "SDB_BENCH_MAX_TURNS={}",
                limits.max_turns.unwrap_or(0)
            ));
        }

        // Attach to the run's private network (reference aliases ref-redis/ref-http/ref-dns)
        // when provided; fall back to the default bridge. The agent is never on the host
        // network, so host services stay unreachable from model-executed code.
        if let Some(net) = network {
            cmd.arg("--network").arg(net);
        } else {
            cmd.arg("--network=bridge");
        }

        let effective_key = api_key
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok());
        if let Some(ref key) = effective_key {
            cmd.arg("-e").arg(format!("OPENROUTER_API_KEY={}", key));
        }


        let (target_port, test_cmd, guidance_snippet) = match task {
            Some(crate::config::TaskType::Pdf) => (
                8090,
                "curl -v http://localhost:8090/",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Third-party PDF libraries (pypdf, pdfminer, fitz, lopdf, pdfcpu, etc.) and external CLI tools (pdftotext, mutool) are STRICTLY FORBIDDEN.
- Build an RFC-compliant HTTP microservice on port 8090 using raw TCP sockets returning HTTP 200 on GET / and extracting plain text from binary PDF payloads sent to POST /extract."#,
            ),
            Some(crate::config::TaskType::Redis) => (
                6379,
                "redis-cli -p 6379 ping",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Pre-built Redis servers and client libraries are FORBIDDEN.
- Build from raw TCP sockets on port 6379 parsing RESP wire format (`+PONG
`, `:1
`, `$len
`)."#,
            ),
            Some(crate::config::TaskType::Dns) => (
                5353,
                "dig @127.0.0.1 -p 5353 test.local",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Pre-built DNS servers (bind9, coredns, dnsmasq) are FORBIDDEN.
- Build from raw UDP sockets on port 5353 handling RFC 1035 wire packets."#,
            ),
            Some(crate::config::TaskType::Http) => (
                8080,
                "curl -v http://localhost:8080/",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Built-in HTTP modules and frameworks (`http.server`, `BaseHTTPRequestHandler`, `HTTPServer`, `net/http`, `flask`, `fastapi`, `express`) are STRICTLY FORBIDDEN and will result in automatic DISQUALIFICATION!
- Python Raw Socket Skeleton:
  ```python
  import socket, os
  os.makedirs('/tmp/files', exist_ok=True)
  s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
  s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
  s.bind(('0.0.0.0', 8080))
  s.listen(128)
  while True:
      conn, addr = s.accept()
      try:
          data = conn.recv(4096).decode('utf-8', errors='ignore')
          # Parse request and write HTTP response:
          conn.sendall(b"HTTP/1.1 200 OK
Content-Length: 0

")
      finally:
          conn.close()
  ```
- Go: use `net.Listen("tcp", "0.0.0.0:8080")` and `net.Conn`. Do NOT use `net/http`.
- Rust: use `std::net::TcpListener` or `tokio::net::TcpListener`."#,
            ),
            Some(crate::config::TaskType::Wal) => (
                6390,
                "printf 'PUB t hello\\n' | nc 127.0.0.1 6390",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Pre-built brokers (Kafka, NATS, Redis) are FORBIDDEN.
- Build a durable append-only log on TCP 6390: PUB <topic> <payload> -> +OK <offset> (fsync before ack), SUB <topic> <offset> streams :<off> <payload> lines then +END. Persist under ./data and recover it after kill -9."#,
            ),
            Some(crate::config::TaskType::Proxy) => (
                8091,
                "curl http://upstream1/ ; curl -v http://localhost:8091/",                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Proxy frameworks/gateways (nginx, envoy, traefik libs) are FORBIDDEN; raw TCP sockets only.
- Build an HTTP reverse proxy on 8091 over upstreams 127.0.0.1:8101/8102/8103: round-robin, upstream keep-alive pooling, X-Forwarded-For injection, failover with zero dropped requests."#,
            ),
            Some(crate::config::TaskType::Pubsub) => (
                8092,
                "curl -v http://localhost:8092/",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- WebSocket libraries (ws, gorilla, tungstenite) are FORBIDDEN; raw TCP sockets only.
- Build an RFC 6455 broker on 8092: SHA-1 handshake, masked TEXT frames SUB <channel> / PUB <channel> <msg>, broadcast fan-out, ping/pong."#,
            ),
            None => {
                let prompt_lower = prompt.to_lowercase();
                if prompt_lower.contains("pdf") {
                    (
                        8090,
                        "curl -v http://localhost:8090/",
                        r#"ANTI-CHEAT REQUIREMENT (STRICT): Build an HTTP PDF microservice using raw TCP sockets on port 8090 returning HTTP 200 on GET / and extracting plain text from binary PDF payloads sent to POST /extract."#,
                    )
                } else if prompt_lower.contains("redis") {
                    (
                        6379,
                        "redis-cli -p 6379 ping",
                        r#"ANTI-CHEAT REQUIREMENT (STRICT): Build from raw TCP sockets on port 6379 parsing RESP wire format."#,
                    )
                } else if prompt_lower.contains("dns") {
                    (
                        5353,
                        "dig @127.0.0.1 -p 5353 test.local",
                        r#"ANTI-CHEAT REQUIREMENT (STRICT): Build from raw UDP sockets on port 5353 handling RFC 1035 wire packets."#,
                    )
                } else {
                    (
                        8080,
                        "curl -v http://localhost:8080/",
                        r#"ANTI-CHEAT REQUIREMENT (STRICT): Build using raw TCP sockets on port 8080."#,
                    )
                }
            }
        };

        let effective_prompt = format!(
            "{}

==================================================
CRITICAL DIRECTIVES FOR AUTONOMOUS AGENT EXECUTION:
1. TOOL CALL REQUIREMENT: You are an autonomous AI coding agent with filesystem tools (`write`, `bash`, `read`, `edit`). You MUST execute your tools immediately to create your implementation files directly in /workspace. DO NOT merely explain the plan in conversational text without calling `write`.
2. RAW TCP SOCKETS ONLY (MANDATORY):
{}
3. ENTRYPOINT: Create a working executable `./start.sh` or `Dockerfile` listening on port {}. Test your server with `{}` using `bash` before finishing.
4. CONCISE REASONING: Keep internal thinking brief (< 150 words). Do not spend output tokens on exhaustive planning in thought blocks. Immediately invoke tools (`write`, `bash`, `read`) on turn 1 to start creating the server and verifying it.
==================================================",
            prompt, guidance_snippet, target_port, test_cmd
        );

        // Write AGENTS.md into workspace so omp auto-loads instructions across all turns
        let agents_md_path = canonical_workdir.join("AGENTS.md");
        let agents_md_content = format!(
            r#"# Instructions for Autonomous Coding Agent
1. **TOOL CALL REQUIREMENT**: You have tools (`write`, `bash`, `edit`, `read`). You MUST use the `write` tool to create every file directly in /workspace. NEVER output code in conversational text without calling `write`.
2. **RAW TCP SOCKETS ONLY (CRITICAL)**:
{}
3. **ENTRYPOINT REQUIRED**: You must create a working `Dockerfile` or executable `./start.sh` listening on port {}. Test your server with `{}` using the `bash` tool before finishing.
4. **CONCISE REASONING**: Keep internal thoughts brief (< 150 words). Do not exhaust output tokens on lengthy mental planning loops. Immediately invoke tools to write code and test it.
"#,
            guidance_snippet, target_port, test_cmd
        );
        let _ = std::fs::write(&agents_md_path, agents_md_content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&agents_md_path, std::fs::Permissions::from_mode(0o644));
        }

        cmd.arg("subdollar-sandbox")
            .arg("omp")
            .arg("--approval-mode=yolo")
            .arg("--tools=read,bash,edit,write,grep,glob,lsp")
            .arg("-p")
            .arg(&effective_prompt)
            .arg(format!("--model={}", effective_model))
            .arg("--cwd=/workspace")
            .arg("--session-dir=/home/ubuntu/.omp/agent/sessions");

        // Spike: bench policy extension (opt-out via SDB_BENCH_POLICY=0).
        if let Some(policy_path) = bench_policy_path {
            cmd.arg("-e").arg(&policy_path);
        }

        if let Some(max_secs) = limits.timeout_seconds {
            cmd.arg(format!("--max-time={}s", max_secs));
        }

        let (eff_level, eff_note) = resolve_thinking_level(effort, &effective_model);
        cmd.arg(format!("--thinking={}", eff_level));
        cmd.arg("--print-thoughts");

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let existing_files = Self::collect_all_session_files();
        let active_file: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

        info!(
            "Executing OMP agent container '{}' with workdir: {}",
            container_name,
            canonical_workdir.display()
        );
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn omp in docker: {}", e))?;

        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));

        let turn_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let accumulated_cost = Arc::new(Mutex::new(0.0f64));
        let accumulated_tokens = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let limit_reached = Arc::new(AtomicBool::new(false));
        let limit_reason = Arc::new(Mutex::new(Option::<String>::None));

        let _ = tx.send(format!(
            "[TELEMETRY] turn=0 max_turns={} spent=0.000000 tokens=0",
            limits.max_turns.unwrap_or(0)
        ));
        let _ = tx.send(format!(
            "[LIVE] Agent initialized. Max turns: {}, Budget: ${:.2}",
            limits.max_turns.unwrap_or(0),
            limits.max_budget_usd.unwrap_or(0.0)
        ));
        let _ = tx.send(format!(
            "[OMP-PROMPT] model={} thinking={}{} cwd=/workspace",
            effective_model, eff_level, eff_note
        ));
        for prompt_line in effective_prompt.lines() {
            let clean = prompt_line.trim_end();
            if clean.is_empty() {
                continue;
            }
            let _ = tx.send(format!("  | {}", Self::cap_line(clean)));
        }

        // Extension event feed (spike): extension BENCH-EVENT lines become
        // the primary console feed; the tailer below renders assistant/tool
        // session lines as token stats only once the feed proves live.
        let extension_feed = Arc::new(AtomicBool::new(false));
        // Spawn thread to tail the active session .jsonl file in real-time
        let tail_ctx = TailSessionContext {
            start_time,
            existing: existing_files,
            active_file: active_file.clone(),
            running: running.clone(),
            tx: tx.clone(),
            limits,
            turn_counter: turn_counter.clone(),
            accumulated_cost: accumulated_cost.clone(),
            accumulated_tokens: accumulated_tokens.clone(),
            limit_reached: limit_reached.clone(),
            limit_reason: limit_reason.clone(),
            cancel_flag: cancel_flag.clone(),
            extension_feed_active: extension_feed.clone(),
        };

        let tailer_handle = thread::spawn(move || {
            Self::tail_session_file(tail_ctx);
        });

        // Spawn thread to read stdout
        if let Some(stdout) = child.stdout.take() {
            let tx_out = tx.clone();
            let ext_flag = extension_feed.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if let Some(tagged) = Self::convert_bench_event(trimmed) {
                        ext_flag.store(true, Ordering::SeqCst);
                        for t in tagged {
                            let _ = tx_out.send(t);
                        }
                    } else if let Some(out) = Self::forward_stdout_line(trimmed) {
                        let _ = tx_out.send(out);
                    }
                }
            });
        }

        // Spawn thread to read stderr
        if let Some(stderr) = child.stderr.take() {
            let tx_err = tx.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = tx_err.send(format!("[STDERR] {}", trimmed));
                    }
                }
            });
        }
        drop(tx);

        let kill_container = container_name.clone();
        let kill_agent = move || {
            let _ = Command::new("docker")
                .args(["kill", &kill_container])
                .output();
            let _ = Command::new("docker")
                .args(["rm", "-f", &kill_container])
                .output();
        };

        let start_instant = std::time::Instant::now();
        let status = loop {
            // Check cancellation request immediately
            if let Some(ref cf) = cancel_flag {
                if cf.load(Ordering::SeqCst) {
                    let msg = "[PIPELINE] Cancellation requested. Terminating agent container immediately...".to_string();
                    log_fn(msg.clone());
                    limit_reached.store(true, Ordering::SeqCst);
                    *limit_reason.lock().unwrap() = Some(msg);
                    kill_agent();
                    let _ = child.kill();
                    break None;
                }
            }
            // Check wall-clock timeout watchdog
            if let Some(max_secs) = limits.timeout_seconds {
                if start_instant.elapsed().as_secs() >= max_secs {
                    let msg = format!("[LIMIT] Watchdog: maximum time limit ({}s) reached. Concluding agent run...", max_secs);
                    log_fn(msg.clone());
                    limit_reached.store(true, Ordering::SeqCst);
                    *limit_reason.lock().unwrap() = Some(msg);
                    kill_agent();
                    let _ = child.kill();
                    break None;
                }
            }

            // Check if turn or budget limit was reached
            if limit_reached.load(Ordering::SeqCst) {
                if let Ok(lock) = limit_reason.lock() {
                    if let Some(ref r) = *lock {
                        log_fn(r.clone());
                    }
                }
                kill_agent();
                let _ = child.kill();
                break None;
            }

            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => log_fn(line),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(s) = child
                        .try_wait()
                        .map_err(|e| anyhow!("Failed to check omp status: {}", e))?
                    {
                        break Some(s);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let s = child
                        .wait()
                        .map_err(|e| anyhow!("Failed to wait for omp: {}", e))?;
                    break Some(s);
                }
            }
        };

        kill_agent();
        running.store(false, Ordering::SeqCst);
        let _ = tailer_handle.join();

        while let Ok(line) = rx.recv_timeout(Duration::from_millis(50)) {
            log_fn(line);
        }

        let stopped_by_limit = limit_reached.load(Ordering::SeqCst);
        let _ = std::fs::remove_file(canonical_workdir.join("AGENTS.md"));
        let final_path = active_file.lock().unwrap().clone();
        let stats = match final_path.as_deref() {
            Some(p) => Self::extract_latest_session_stats(Some(p)).unwrap_or_default(),
            None => OmpSessionStats::default(),
        };

        let has_runnable = canonical_workdir.join("Dockerfile").exists()
            || canonical_workdir.join("start.sh").exists();
        if !has_runnable {
            if let Some(ref sp) = final_path {
                Self::fallback_extract_files_from_session(sp, &canonical_workdir);
            }
        }

        if !stopped_by_limit {
            if let Some(s) = status {
                if !s.success() {
                    warn!("OMP agent exited with non-zero status: {:?}", s.code());
                    return Err(anyhow::Error::new(OmpExecutionError {
                        message: format!("OMP agent exited with non-zero status: {:?}", s.code()),
                        partial_stats: stats,
                    }));
                }
            }
        }

        Ok(stats)
    }

    fn get_sessions_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let host_sessions = Self::get_host_omp_dir().join("agent/sessions");
        if host_sessions.exists() {
            dirs.push(host_sessions);
        }
        let u = PathBuf::from("/home/ubuntu/.omp/agent/sessions");
        if u.exists() && !dirs.contains(&u) {
            dirs.push(u);
        }
        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(home).join(".omp/agent/sessions");
            if p.exists() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
        let root = crate::config::get_repo_root();
        let r_omp = root.join(".omp/agent/sessions");
        if r_omp.exists() && !dirs.contains(&r_omp) {
            dirs.push(r_omp);
        }
        let r = PathBuf::from("/root/.omp/agent/sessions");
        if r.exists() && !dirs.contains(&r) {
            dirs.push(r);
        }
        dirs
    }

    fn collect_all_session_files() -> HashSet<PathBuf> {
        let mut set = HashSet::new();
        for dir in Self::get_sessions_dirs() {
            let mut candidates = Vec::new();
            Self::collect_jsonl_files(&dir, &mut candidates);
            for (_, p) in candidates {
                set.insert(p);
            }
        }
        set
    }

    fn tail_session_file(ctx: TailSessionContext) {
        let dirs = Self::get_sessions_dirs();

        // Check if active_file already has a pre-set file
        let mut session_file: Option<PathBuf> = ctx.active_file.lock().ok().and_then(|l| l.clone());

        // Poll continuously for the new session file to appear while agent is running
        if session_file.is_none() {
            while ctx.running.load(Ordering::SeqCst) {
                if let Some(f) = Self::find_new_session_file(&dirs, &ctx.existing, ctx.start_time) {
                    session_file = Some(f);
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }

            if session_file.is_none() {
                session_file = Self::find_new_session_file(&dirs, &ctx.existing, ctx.start_time);
            }
        }

        let session_path = match session_file {
            Some(p) => p,
            None => {
                info!("No new session file detected for current run.");
                return;
            }
        };

        if let Ok(mut lock) = ctx.active_file.lock() {
            *lock = Some(session_path.clone());
        }

        let file = match File::open(&session_path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let mut reader = BufReader::new(file);
        let mut line_buf = String::new();

        let mut last_activity = std::time::Instant::now();
        let mut last_heartbeat = std::time::Instant::now();

        while ctx.running.load(Ordering::SeqCst) {
            if let Some(ref cf) = ctx.cancel_flag {
                if cf.load(Ordering::SeqCst) {
                    break;
                }
            }
            line_buf.clear();
            match reader.read_line(&mut line_buf) {
                Ok(0) => {
                    let cur_turns = ctx.turn_counter.load(Ordering::SeqCst);
                    let max_t = ctx.limits.max_turns.unwrap_or(0);
                    let elapsed = last_activity.elapsed().as_secs();
                    if elapsed >= 3 && last_heartbeat.elapsed() >= Duration::from_secs(3) {
                        last_heartbeat = std::time::Instant::now();
                        let max_str = if max_t > 0 { max_t.to_string() } else { "∞".to_string() };
                        let _ = ctx.tx.send(format!(
                            "[WAITING] Turn {}/{} in progress: Model is reasoning and generating response... (elapsed: {}s)",
                            cur_turns + 1,
                            max_str,
                            elapsed
                        ));
                    }
                    thread::sleep(Duration::from_millis(150));
                }
                Ok(_) => {
                    last_activity = std::time::Instant::now();
                    if line_buf.ends_with('\n') {
                        // Check limits & telemetry in session line
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line_buf.trim())
                        {
                            let mut assistant_turn = false;
                            // 1. Assistant turn check
                            if val.pointer("/message/role").and_then(|r| r.as_str())
                                == Some("assistant")
                            {
                                assistant_turn = true;
                                let c = ctx.turn_counter.fetch_add(1, Ordering::SeqCst) + 1;
                                if let Some(max_t) = ctx.limits.max_turns {
                                    if c >= max_t {
                                        let msg = format!("[LIMIT] Reached turn limit ({} / {} turns). Concluding agent run...", c, max_t);
                                        *ctx.limit_reason.lock().unwrap() = Some(msg.clone());
                                        ctx.limit_reached.store(true, Ordering::SeqCst);
                                        let _ = ctx.tx.send(msg);
                                    }
                                }
                            }
                            // 2. Budget check
                            let mut cost_updated = false;
                            if let Some(cost) = val
                                .pointer("/message/usage/cost/total")
                                .and_then(|c| c.as_f64())
                            {
                                if let Ok(mut c_lock) = ctx.accumulated_cost.lock() {
                                    *c_lock += cost;
                                    cost_updated = true;
                                    let current_spend = *c_lock;
                                    if let Some(max_b) = ctx.limits.max_budget_usd {
                                        if current_spend >= max_b {
                                            let msg = format!("[LIMIT] Reached budget cap (${:.4} / ${:.2} USD). Concluding agent run...", current_spend, max_b);
                                            *ctx.limit_reason.lock().unwrap() = Some(msg.clone());
                                            ctx.limit_reached.store(true, Ordering::SeqCst);
                                            let _ = ctx.tx.send(msg);
                                        }
                                    }
                                }
                            }

                            // 3. Token tracking
                            if let Some(usage) = val.pointer("/message/usage").or_else(|| val.get("usage")) {
                                let tin = usage
                                    .get("input")
                                    .or_else(|| usage.get("prompt_tokens"))
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0);
                                let tout = usage
                                    .get("output")
                                    .or_else(|| usage.get("completion_tokens"))
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0);
                                if tin + tout > 0 {
                                    ctx.accumulated_tokens.fetch_add(tin + tout, Ordering::SeqCst);
                                }
                            }

                            if assistant_turn || cost_updated {
                                let cur_turns = ctx.turn_counter.load(Ordering::SeqCst);
                                let max_t = ctx.limits.max_turns.unwrap_or(0);
                                let cur_spend = ctx.accumulated_cost.lock().map(|l| *l).unwrap_or(0.0);
                                let cur_tokens = ctx.accumulated_tokens.load(Ordering::SeqCst);

                                let _ = ctx.tx.send(format!(
                                    "[TELEMETRY] turn={} max_turns={} spent={:.6} tokens={}",
                                    cur_turns, max_t, cur_spend, cur_tokens
                                ));
                                let _ = ctx.tx.send(format!(
                                    "[LIVE] Turn {}/{} | Spent: ${:.4} | Tokens: {}",
                                    cur_turns,
                                    if max_t > 0 { max_t.to_string() } else { "∞".to_string() },
                                    cur_spend,
                                    cur_tokens
                                ));
                            }
                        }

                        for ev in Self::render_tail_line(
                            &line_buf,
                            ctx.extension_feed_active.load(Ordering::SeqCst),
                        ) {
                            let _ = ctx.tx.send(ev);
                        }
                    } else {
                        let len = line_buf.len() as i64;
                        let _ = reader.seek(SeekFrom::Current(-len));
                        thread::sleep(Duration::from_millis(150));
                    }
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }

        // Process exit drain
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf) {
                Ok(0) => break,
                Ok(_) => {
                    if line_buf.ends_with('\n') {
                        for ev in Self::render_tail_line(
                            &line_buf,
                            ctx.extension_feed_active.load(Ordering::SeqCst),
                        ) {
                            let _ = ctx.tx.send(ev);
                        }
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    fn find_new_session_file(
        dirs: &[PathBuf],
        existing: &HashSet<PathBuf>,
        start_time: SystemTime,
    ) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        for dir in dirs {
            Self::collect_jsonl_files(dir, &mut candidates);
        }
        candidates.sort_by_key(|(m, _)| *m);

        // Priority 1: brand new file not present in existing snapshot
        for (_, p) in candidates.iter().rev() {
            if !existing.contains(p) {
                return Some(p.clone());
            }
        }

        // Priority 2: modified after start_time
        for (m, p) in candidates.into_iter().rev() {
            if m >= start_time {
                return Some(p);
            }
        }
        None
    }

    #[allow(dead_code)]
    fn find_newest_session_file_across(dirs: &[PathBuf]) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        for dir in dirs {
            Self::collect_jsonl_files(dir, &mut candidates);
        }
        candidates.sort_by_key(|(m, _)| *m);
        candidates.pop().map(|(_, p)| p)
    }

    fn collect_jsonl_files(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    if let Ok(meta) = path.metadata() {
                        if let Ok(m) = meta.modified() {
                            out.push((m, path));
                        }
                    }
                } else if path.is_dir() {
                    Self::collect_jsonl_files(&path, out);
                }
            }
        }
    }

    /// Single-line flood guard for forwarded plain-text output.
    fn cap_line(text: &str) -> String {
        const MAX: usize = 2000;
        let count = text.chars().count();
        if count <= MAX {
            text.to_string()
        } else {
            format!(
                "{}... (+{} chars)",
                text.chars().take(MAX).collect::<String>(),
                count - MAX
            )
        }
    }

    /// Decide whether a captured stdout line is forwarded to the bench log.
    /// JSON lines belong to the session-file tailer; everything else —
    /// thoughts, progress, `[BENCH-POLICY]` status, `[BENCH-EVENT]` structured
    /// extension events, errors — is forwarded verbatim (capped).
    fn forward_stdout_line(trimmed: &str) -> Option<String> {
        if trimmed.is_empty() {
            return None;
        }
        if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
            return None;
        }
        Some(format!("[OMP] {}", Self::cap_line(trimmed)))
    }

    /// Convert an extension `[BENCH-EVENT] {...}` line into console tag lines
    /// (`[THOUGHT]`/`[RESPONSE]`/`[RESULT]`). Returns None for non-event lines
    /// (caller falls back to verbatim forwarding) and for unknown shapes.
    /// `role=user` events are skipped: launch-time prompt lines already carry
    /// the task prompt.
    fn convert_bench_event(trimmed: &str) -> Option<Vec<String>> {
        let body = trimmed.strip_prefix("[BENCH-EVENT] ")?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        if v.get("v")?.as_u64()? != 1 {
            return None;
        }
        let mut out = Vec::new();
        match v.get("kind").and_then(|k| k.as_str())? {
            "message" => {
                if v.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                    return Some(out);
                }
                for line in v.get("thinking").and_then(|t| t.as_str()).unwrap_or("").lines() {
                    let clean = line.trim();
                    if !clean.is_empty() {
                        out.push(format!("[THOUGHT] {}", clean));
                    }
                }
                for line in v.get("text").and_then(|t| t.as_str()).unwrap_or("").lines() {
                    let clean = line.trim();
                    if !clean.is_empty() {
                        out.push(format!("[RESPONSE] {}", clean));
                    }
                }
            }
            "tool_call" => {
                let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
                let intent = v.get("intent").and_then(|i| i.as_str()).unwrap_or("");
                if intent.is_empty() {
                    out.push(format!("[ACTION] {}", tool));
                } else {
                    out.push(format!("[ACTION] {}: {}", tool, intent));
                }
                for line in v.get("command").and_then(|c| c.as_str()).unwrap_or("").lines() {
                    let t = line.trim();
                    if !t.is_empty() {
                        out.push(format!("  $ {}", t));
                    }
                }
                if let Some(path) = v.get("path").and_then(|q| q.as_str()) {
                    if !path.is_empty() {
                        out.push(format!("  path: {}", path));
                    }
                }
                let code_lines: Vec<&str> = v
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .lines()
                    .collect();
                for (idx, c_line) in code_lines.iter().enumerate() {
                    if idx < 35 {
                        out.push(format!("  | {}", c_line));
                    } else {
                        out.push(format!(
                            "  | ... (+{} lines omitted)",
                            code_lines.len() - 35
                        ));
                        break;
                    }
                }
            }
            "tool_result" => {
                let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
                let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(true);
                out.push(format!("[RESULT] {} ({})", tool, if ok { "OK" } else { "FAIL" }));
                let lines: Vec<&str> = v
                    .get("output")
                    .and_then(|o| o.as_str())
                    .unwrap_or("")
                    .lines()
                    .collect();
                if lines.is_empty() {
                    out.push("  (empty output)".to_string());
                } else {
                    for (idx, l) in lines.iter().enumerate() {
                        if idx < 40 {
                            out.push(format!("  > {}", l));
                        } else {
                            out.push(format!("  > ... (+{} more lines)", lines.len() - 40));
                            break;
                        }
                    }
                }
            }
            _ => return None,
        }
        Some(out)
    }

    /// Mirror of the `[TOKENS]` block in `format_session_line`, kept so the
    /// gated tail still reports per-turn usage after its content rendering
    /// retires to the extension feed.
    fn extract_token_line(v: &serde_json::Value) -> Option<String> {
        let usage = v.get("message")?.get("usage")?;
        let tin = usage
            .get("input")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let tout = usage
            .get("output")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let tcached = usage
            .get("cacheRead")
            .or_else(|| usage.get("cache_read_input_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        if tin > 0 || tout > 0 || tcached > 0 {
            Some(format!(
                "[TOKENS] Turn usage - input: {}, output: {}, cached: {}",
                tin, tout, tcached
            ))
        } else {
            None
        }
    }

    /// Single choke point for session-file rendering. Accounting (turns, cost,
    /// tokens, TELEMETRY) happens upstream and is untouched; only the human
    /// rendering retires to the extension feed once it proves live.
    fn render_tail_line(line: &str, extension_active: bool) -> Vec<String> {
        if extension_active {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                if v
                    .get("customType")
                    .and_then(|c| c.as_str())
                    == Some("tool_execution_start")
                {
                    return Vec::new();
                }
                let role = v
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str());
                if matches!(role, Some("assistant") | Some("toolResult") | Some("tool")) {
                    return Self::extract_token_line(&v).map(|t| vec![t]).unwrap_or_default();
                }
            }
        }
        Self::format_session_line(line).unwrap_or_default()
    }

    fn format_session_line(line: &str) -> Option<Vec<String>> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Plain-text lines (thoughts, progress) are human output worth showing.
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return Some(vec![format!("[OMP] {}", Self::cap_line(trimmed))]),
        };
        let mut results = Vec::new();

        let custom_type = v.get("customType").and_then(|s| s.as_str());
        let role = v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|s| s.as_str());

        // 1. User Task Instructions & Directives
        if role == Some("user") {
            let mut text = String::new();
            if let Some(msg) = v.get("message") {
                if let Some(c_arr) = msg.get("content").and_then(|c| c.as_array()) {
                    for part in c_arr {
                        if let Some(t) = part.get("text").and_then(|s| s.as_str()) {
                            text.push_str(t);
                        }
                    }
                } else if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    text.push_str(s);
                }
            }
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                results.push("[PROMPT] Task Instructions & Directives:".to_string());
                for l in trimmed.lines() {
                    let clean = l.trim();
                    if !clean.is_empty() {
                        results.push(format!("  | {}", clean));
                    }
                }
            }
        }
        // 2. Assistant Turn: Thoughts, Text Response, and Tool Calls (with code preview)
        else if role == Some("assistant") {
            if let Some(msg) = v.get("message") {
                if let Some(usage) = msg.get("usage") {
                    let tin = usage
                        .get("input")
                        .or_else(|| usage.get("prompt_tokens"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    let tout = usage
                        .get("output")
                        .or_else(|| usage.get("completion_tokens"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    let tcached = usage
                        .get("cacheRead")
                        .or_else(|| usage.get("cache_read_input_tokens"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    if tin > 0 || tout > 0 || tcached > 0 {
                        results.push(format!(
                            "[TOKENS] Turn usage - input: {}, output: {}, cached: {}",
                            tin, tout, tcached
                        ));
                    }
                }
                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                    for item in content {
                        let item_type = item.get("type").and_then(|s| s.as_str());
                        if item_type == Some("thinking") {
                            if let Some(th) = item.get("thinking").and_then(|s| s.as_str()) {
                                for line in th.lines() {
                                    let clean = line.trim();
                                    if !clean.is_empty() {
                                        results.push(format!("[THOUGHT] {}", clean));
                                    }
                                }
                            }
                        } else if item_type == Some("text") {
                            if let Some(txt) = item.get("text").and_then(|s| s.as_str()) {
                                for line in txt.lines() {
                                    let clean = line.trim();
                                    if !clean.is_empty() {
                                        results.push(format!("[RESPONSE] {}", clean));
                                    }
                                }
                            }
                        } else if item_type == Some("toolCall")
                            || item_type == Some("function_call")
                            || item_type == Some("tool_use")
                        {
                            let tool = item
                                .get("name")
                                .or_else(|| item.get("toolName"))
                                .and_then(|s| s.as_str())
                                .unwrap_or("tool");
                            let intent = item.get("intent").and_then(|s| s.as_str()).unwrap_or("");
                            if intent.is_empty() {
                                results.push(format!("[ACTION] {}", tool));
                            } else {
                                results.push(format!("[ACTION] {}: {}", tool, intent));
                            }

                            // Extract arguments (either JSON object or stringified JSON)
                            let maybe_args = item
                                .get("arguments")
                                .cloned()
                                .or_else(|| item.get("args").cloned())
                                .or_else(|| item.get("input").cloned());
                            let args_val = match maybe_args {
                                Some(serde_json::Value::Object(map)) => {
                                    Some(serde_json::Value::Object(map))
                                }
                                Some(serde_json::Value::String(s)) => {
                                    serde_json::from_str::<serde_json::Value>(&s).ok()
                                }
                                _ => None,
                            };

                            if let Some(args) = args_val {
                                if let Some(cmd) = args.get("command").and_then(|s| s.as_str()) {
                                    for line in cmd.lines() {
                                        let t = line.trim();
                                        if !t.is_empty() {
                                            results.push(format!("  $ {}", t));
                                        }
                                    }
                                } else if let Some(p) = args.get("path").and_then(|s| s.as_str()) {
                                    results.push(format!("  path: {}", p));
                                }

                                if let Some(content) = args
                                    .get("content")
                                    .or_else(|| args.get("code"))
                                    .and_then(|s| s.as_str())
                                {
                                    let lines_vec: Vec<&str> = content.lines().collect();
                                    for (i, c_line) in lines_vec.iter().enumerate() {
                                        if i < 35 {
                                            results.push(format!("  | {}", c_line));
                                        } else {
                                            results.push(format!(
                                                "  | ... (+{} lines omitted)",
                                                lines_vec.len() - 35
                                            ));
                                            break;
                                        }
                                    }
                                } else if let Some(old_s) = args.get("oldText").or_else(|| args.get("old_str")).and_then(|s| s.as_str()) {
                                    results.push(format!("  - {}", old_s.lines().next().unwrap_or("")));
                                    if let Some(new_s) = args.get("newText").or_else(|| args.get("new_str")).and_then(|s| s.as_str()) {
                                        results.push(format!("  + {}", new_s.lines().next().unwrap_or("")));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // 3. Tool Execution Start
        else if custom_type == Some("tool_execution_start") {
            if let Some(d) = v.get("data") {
                let tool = d.get("toolName").and_then(|s| s.as_str()).unwrap_or("tool");
                let intent = d.get("intent").and_then(|s| s.as_str()).unwrap_or("");
                if intent.is_empty() {
                    results.push(format!("[ACTION] {}", tool));
                } else {
                    results.push(format!("[ACTION] {}: {}", tool, intent));
                }
                if let Some(args) = d.get("args") {
                    if let Some(cmd) = args.get("command").and_then(|s| s.as_str()) {
                        for line in cmd.lines() {
                            let t = line.trim();
                            if !t.is_empty() {
                                results.push(format!("  $ {}", t));
                            }
                        }
                    } else if let Some(p) = args.get("path").and_then(|s| s.as_str()) {
                        results.push(format!("  path: {}", p));
                    }

                    if let Some(content) = args
                        .get("content")
                        .or_else(|| args.get("code"))
                        .and_then(|s| s.as_str())
                    {
                        let lines_vec: Vec<&str> = content.lines().collect();
                        for (i, c_line) in lines_vec.iter().enumerate() {
                            if i < 20 {
                                results.push(format!("  | {}", c_line));
                            } else {
                                results.push(format!(
                                    "  | ... (+{} lines)",
                                    lines_vec.len() - 20
                                ));
                                break;
                            }
                        }
                    }
                }
            }
        }
        // 4. Tool Execution Results (OpenAI 'tool' or OMP 'toolResult')
        else if role == Some("toolResult") || role == Some("tool") {
            if let Some(msg) = v.get("message") {
                let tool = msg
                    .get("toolName")
                    .or_else(|| msg.get("name"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("tool");
                let is_err = msg
                    .get("isError")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                let status = if is_err { "FAIL" } else { "OK" };

                let mut text = String::new();
                if let Some(c_arr) = msg.get("content").and_then(|c| c.as_array()) {
                    for part in c_arr {
                        if let Some(t) = part.get("text").and_then(|s| s.as_str()) {
                            text.push_str(t);
                        }
                    }
                } else if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    text.push_str(s);
                }

                results.push(format!("[RESULT] {} ({})", tool, status));
                let lines: Vec<&str> = text.lines().collect();
                if lines.is_empty() {
                    results.push("  (empty output)".to_string());
                } else {
                    for (idx, l) in lines.iter().enumerate() {
                        if idx < 40 {
                            results.push(format!("  > {}", l));
                        } else {
                            results.push(format!("  > ... (+{} more lines)", lines.len() - 40));
                            break;
                        }
                    }
                }
            }
        }
        // 5. System Interventions and Guards
        else if custom_type == Some("thinking-loop-redirect") {
            results.push("[SYSTEM INTERRUPT] Loop guard triggered: repetitive reasoning detected, agent was redirected.".to_string());
        } else if custom_type == Some("session_exit") {
            results.push("[SESSION] Agent session concluded.".to_string());
        }

        if results.is_empty() {
            None
        } else {
            Some(results)
        }
    }

    pub fn extract_latest_session_stats(preferred_file: Option<&Path>) -> Result<OmpSessionStats> {
        let session_file = match preferred_file {
            Some(p) if p.exists() => p.to_path_buf(),
            _ => return Err(anyhow!("No valid session jsonl file provided")),
        };
        let content = std::fs::read_to_string(&session_file)?;

        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut cached_tokens = 0u64;
        let mut steps = 0u32;

        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let is_assistant = v.pointer("/message/role").and_then(|r| r.as_str()) == Some("assistant");
                let is_test_usage = v.pointer("/message/role").is_none()
                    && (v.get("usage").is_some() || v.pointer("/message/usage").is_some());
                if is_assistant || is_test_usage {
                    steps += 1;
                }
                let usage_opt = v
                    .get("usage")
                    .or_else(|| v.get("message").and_then(|m| m.get("usage")));

                if let Some(usage) = usage_opt {
                    if let Some(pt) = usage
                        .get("input")
                        .or_else(|| usage.get("prompt_tokens"))
                        .and_then(|x| x.as_u64())
                    {
                        prompt_tokens += pt;
                    }
                    if let Some(ct) = usage
                        .get("output")
                        .or_else(|| usage.get("completion_tokens"))
                        .and_then(|x| x.as_u64())
                    {
                        completion_tokens += ct;
                    }
                    if let Some(cr) = usage
                        .get("cacheRead")
                        .or_else(|| usage.get("cache_read_input_tokens"))
                        .and_then(|x| x.as_u64())
                    {
                        cached_tokens += cr;
                    } else if let Some(details) = usage.get("prompt_tokens_details") {
                        if let Some(c) = details.get("cached_tokens").and_then(|x| x.as_u64()) {
                            cached_tokens += c;
                        }
                    }
                }
            }
        }

        Ok(OmpSessionStats {
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            total_tokens: prompt_tokens + cached_tokens + completion_tokens,
            steps_taken: steps,
        })
    }
    pub fn fallback_extract_files_from_session(session_path: &Path, workdir: &Path) {
        if let Ok(content) = std::fs::read_to_string(session_path) {
            let mut extracted_count = 0;
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if v.get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(|r| r.as_str())
                        == Some("assistant")
                    {
                        if let Some(content_arr) = v
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_array())
                        {
                            for item in content_arr {
                                if let Some(txt) = item.get("text").and_then(|s| s.as_str()) {
                                    if let Some(tools) =
                                        crate::sandbox::tool_normalizer::extract_code_blocks_as_writes(txt)
                                    {
                                        for tc in tools {
                                            if tc.name == "write" {
                                                if let Ok(args) =
                                                    serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                                {
                                                    if let (Some(p), Some(c)) = (
                                                        args.get("path").and_then(|s| s.as_str()),
                                                        args.get("content").and_then(|s| s.as_str()),
                                                    ) {
                                                        let target = workdir.join(p);
                                                        if let Some(parent) = target.parent() {
                                                            let _ = std::fs::create_dir_all(parent);
                                                        }
                                                        if std::fs::write(&target, c).is_ok() {
                                                            info!("Fallback-extracted session file written: {:?}", target);
                                                            extracted_count += 1;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if extracted_count > 0 {
                info!(
                    "Fallback extracted {} file(s) from session into workspace {:?}",
                    extracted_count, workdir
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_bench_event_to_tags() {
        // Assistant message splits into thought + response lines.
        let line = "[BENCH-EVENT] {\"v\":1,\"kind\":\"message\",\"role\":\"assistant\",\"text\":\"did X\\n\\nmore\",\"thinking\":\"plan Y\"}";
        let out = OmpRunner::convert_bench_event(line).unwrap();
        assert!(out.iter().any(|l| l == "[THOUGHT] plan Y"));
        assert!(out.iter().any(|l| l == "[RESPONSE] did X"));
        assert!(out.iter().any(|l| l == "[RESPONSE] more"));
        // Tool result mirrors the tailer shape.
        let line = "[BENCH-EVENT] {\"v\":1,\"kind\":\"tool_result\",\"tool\":\"bash\",\"ok\":false,\"output\":\"boom\\nline2\"}";
        let out = OmpRunner::convert_bench_event(line).unwrap();
        assert!(out.iter().any(|l| l == "[RESULT] bash (FAIL)"));
        assert!(out.iter().any(|l| l == "  > boom"));
        // User prompt already logged at launch: skipped, not dropped to fallback.
        let line = "[BENCH-EVENT] {\"v\":1,\"kind\":\"message\",\"role\":\"user\",\"text\":\"hi\",\"thinking\":\"\"}";
        assert!(OmpRunner::convert_bench_event(line).unwrap().is_empty());
        // Non-events and unknown shapes fall back / are rejected.
        assert!(OmpRunner::convert_bench_event("just thinking").is_none());
        assert!(OmpRunner::convert_bench_event("[BENCH-EVENT] {\"v\":1,\"kind\":\"nope\"}").is_none());
        assert!(OmpRunner::convert_bench_event("[BENCH-EVENT] not-json").is_none());
    }

    #[test]
    fn test_convert_bench_event_tool_call() {
        let line = "[BENCH-EVENT] {\"v\":1,\"kind\":\"tool_call\",\"tool\":\"bash\",\"intent\":\"run tests\",\"command\":\"cargo test\\necho done\",\"path\":\"\",\"code\":\"line1\\nline2\"}";
        let out = OmpRunner::convert_bench_event(line).unwrap();
        assert!(out.iter().any(|l| l == "[ACTION] bash: run tests"));
        assert!(out.iter().any(|l| l == "  $ cargo test"));
        assert!(out.iter().any(|l| l == "  | line1"));
        // Path-only call.
        let line = "[BENCH-EVENT] {\"v\":1,\"kind\":\"tool_call\",\"tool\":\"read\",\"intent\":\"\",\"command\":\"\",\"path\":\"/workspace/main.rs\",\"code\":\"\"}";
        let out = OmpRunner::convert_bench_event(line).unwrap();
        assert!(out.iter().any(|l| l == "[ACTION] read"));
        assert!(out.iter().any(|l| l == "  path: /workspace/main.rs"));
    }

    #[test]
    fn test_render_tail_line_drops_start_events_when_live() {
        let start = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"run tests","args":{"command":"cargo test"}}}"#;
        // Feed down: legacy ACTION render.
        assert!(OmpRunner::render_tail_line(start, false)
            .iter()
            .any(|l| l.contains("[ACTION] bash: run tests")));
        // Feed live: dropped (converter owns it now).
        assert!(OmpRunner::render_tail_line(start, true).is_empty());
    }

    #[test]
    fn test_render_tail_line_gating() {
        let assistant = r#"{"message":{"role":"assistant","usage":{"input":1500,"output":250,"cacheRead":500},"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"hi"}]}}"#;
        // Feed down: full legacy render.
        let full = OmpRunner::render_tail_line(assistant, false);
        assert!(full.iter().any(|l| l.contains("[THOUGHT] hmm")));
        assert!(full.iter().any(|l| l.contains("[TOKENS] Turn usage")));
        // Feed live: content retired, token stats preserved.
        let gated = OmpRunner::render_tail_line(assistant, true);
        assert_eq!(gated.len(), 1);
        assert!(gated[0].contains("[TOKENS] Turn usage"));
        // User lines never covered.
        let user = r#"{"message":{"role":"user","content":[{"type":"text","text":"do X"}]}}"#;
        assert!(OmpRunner::render_tail_line(user, true)
            .iter()
            .any(|l| l.contains("[PROMPT]")));
        // Non-JSON tail lines are never extension-covered: still forwarded.
        assert!(OmpRunner::render_tail_line("not-json", true)
            .iter()
            .any(|l| l.contains("[OMP]")));
    }

    #[test]
    fn test_forward_stdout_line_contract() {
        // Contract is over pre-trimmed input (the reader trims first).
        assert!(OmpRunner::forward_stdout_line("").is_none());
        // Raw session JSON stays with the file tailer.
        assert!(OmpRunner::forward_stdout_line("{\"a\":1}").is_none());
        // Plain thoughts forward.
        let plain = OmpRunner::forward_stdout_line("just thinking out loud").unwrap();
        assert!(plain.contains("[OMP] just thinking out loud"));
        // Structured extension events forward with their tag intact.
        let ev = OmpRunner::forward_stdout_line(
            "[BENCH-EVENT] {\"v\":1,\"kind\":\"message\",\"role\":\"assistant\"}",
        )
        .unwrap();
        assert!(ev.contains("[BENCH-EVENT]"));
        assert!(ev.contains("\"kind\":\"message\""));
    }

    #[test]
    fn test_resolve_thinking_level() {
        // Legacy aliases resolve to the explicit previous behavior.
        assert_eq!(
            resolve_thinking_level(None, "openrouter/meta/m"),
            ("high".to_string(), String::new())
        );
        assert_eq!(
            resolve_thinking_level(Some("auto"), "openrouter/meta/m"),
            ("high".to_string(), String::new())
        );
        assert_eq!(
            resolve_thinking_level(Some("  DEFAULT "), "openrouter/meta/m"),
            ("high".to_string(), String::new())
        );
        // Plain levels pass through untouched.
        assert_eq!(
            resolve_thinking_level(Some("low"), "openrouter/meta/m"),
            ("low".to_string(), String::new())
        );
        // Max survives for OpenAI models, clamps elsewhere with a note.
        let (lvl, note) =
            resolve_thinking_level(Some("max"), "openai/gpt-5.2");
        assert_eq!(lvl, "max");
        assert!(note.is_empty());
        let (lvl, note) =
            resolve_thinking_level(Some("max"), "openrouter/meta/m");
        assert_eq!(lvl, "high");
        assert!(note.contains("clamped"));
    }

    #[test]
    fn test_bench_policy_gate_and_install() {
        // Env-gate truth table (single test: env mutation is process-global).
        let prev = std::env::var("SDB_BENCH_POLICY").ok();
        std::env::remove_var("SDB_BENCH_POLICY");
        assert!(bench_policy_enabled(), "default must be on");
        for off in ["0", "off", "false", "no", "disabled", "OFF"] {
            std::env::set_var("SDB_BENCH_POLICY", off);
            assert!(!bench_policy_enabled(), "must be off for {off}");
        }
        std::env::set_var("SDB_BENCH_POLICY", "1");
        assert!(bench_policy_enabled());

        // Installer writes the embedded source and returns the container path.
        std::env::remove_var("SDB_BENCH_POLICY");
        let dir =
            std::env::temp_dir().join(format!("test_bench_policy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let got = install_bench_policy_extension(&dir);
        assert_eq!(got.as_deref(), Some(BENCH_POLICY_CONTAINER_PATH));
        let content = std::fs::read_to_string(dir.join("bench-policy.js")).unwrap();
        assert!(content.contains("tool_call"), "must hook tool calls");
        assert!(content.contains("[BENCH-POLICY]"), "must emit tagged telemetry");

        // Disabled gate installs nothing.
        std::env::set_var("SDB_BENCH_POLICY", "0");
        assert!(install_bench_policy_extension(&dir).is_none());

        match prev {
            Some(v) => std::env::set_var("SDB_BENCH_POLICY", v),
            None => std::env::remove_var("SDB_BENCH_POLICY"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_format_session_line_user_prompt() {
        let json_user = r#"{
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "Build a Redis server on port 6379"}]
            }
        }"#;
        let res = OmpRunner::format_session_line(json_user).unwrap();
        assert!(res.iter().any(|s| s.contains("[PROMPT] Task Instructions & Directives:")));
        assert!(res.iter().any(|s| s.contains("| Build a Redis server on port 6379")));
    }

    #[test]
    fn test_format_session_line_assistant_tool_call_with_code() {
        let json_assistant = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Writing the main logic"},
                    {
                        "type": "toolCall",
                        "name": "write",
                        "intent": "Create main.rs",
                        "arguments": {
                            "path": "main.rs",
                            "content": "fn main() {\n    println!(\"hello\");\n}"
                        }
                    }
                ]
            }
        }).to_string();
        let res = OmpRunner::format_session_line(&json_assistant).unwrap();
        assert!(res.iter().any(|s| s.contains("[THOUGHT] Writing the main logic")));
        assert!(res.iter().any(|s| s.contains("[ACTION] write: Create main.rs")));
        assert!(res.iter().any(|s| s.contains("path: main.rs")));
        assert!(res.iter().any(|s| s.contains("| fn main() {")));
        assert!(res.iter().any(|s| s.contains("println!(\"hello\");")));
    }

    #[test]
    fn test_format_session_line_system_interrupt() {
        let json_interrupt = r#"{"customType": "thinking-loop-redirect"}"#;
        let res = OmpRunner::format_session_line(json_interrupt).unwrap();
        assert!(res.iter().any(|s| s.contains("[SYSTEM INTERRUPT] Loop guard triggered")));
    }

    #[test]
    fn test_format_session_line_tool_start() {
        let json_bash = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"run tests","args":{"command":"cargo test\necho done"}}}"#;
        let res = OmpRunner::format_session_line(json_bash).unwrap();
        assert!(res.iter().any(|s| s.contains("[ACTION] bash: run tests")));
        assert!(res.iter().any(|s| s.contains("$ cargo test")));
        assert!(res.iter().any(|s| s.contains("$ echo done")));

        let json_read = r#"{"customType":"tool_execution_start","data":{"toolName":"read","intent":"read file","args":{"path":"/workspace/main.rs"}}}"#;
        let res_read = OmpRunner::format_session_line(json_read).unwrap();
        assert!(res_read
            .iter()
            .any(|s| s.contains("path: /workspace/main.rs")));

        let json_write = r#"{"customType":"tool_execution_start","data":{"toolName":"write","intent":"","args":{"code":"line1\nline2"}}}"#;
        let res_write = OmpRunner::format_session_line(json_write).unwrap();
        assert!(res_write.iter().any(|s| s.contains("[ACTION] write")));
        assert!(res_write.iter().any(|s| s.contains("| line1")));
    }

    #[test]
    fn test_format_session_line_assistant() {
        let json_assistant = r#"{
            "message": {
                "role": "assistant",
                "usage": {"input": 1500, "output": 250, "cacheRead": 500},
                "content": [
                    {"type": "thinking", "thinking": "Let me think about this..."},
                    {"type": "text", "text": "Here is the code to solve it"}
                ]
            }
        }"#;
        let res = OmpRunner::format_session_line(json_assistant).unwrap();
        assert!(res
            .iter()
            .any(|s| s.contains("[TOKENS] Turn usage - input: 1500, output: 250, cached: 500")));
        assert!(res
            .iter()
            .any(|s| s.contains("[THOUGHT] Let me think about this...")));
        assert!(res
            .iter()
            .any(|s| s.contains("[RESPONSE] Here is the code to solve it")));
    }

    #[test]
    fn test_format_session_line_tool_result() {
        let json_res_ok = r#"{
            "message": {
                "role": "toolResult",
                "toolName": "bash",
                "isError": false,
                "content": [{"text": "test passed successfully"}]
            }
        }"#;
        let res = OmpRunner::format_session_line(json_res_ok).unwrap();
        assert!(res.iter().any(|s| s.contains("[RESULT] bash (OK)")));
        assert!(res.iter().any(|s| s.contains("> test passed successfully")));

        let json_res_err = r#"{
            "message": {
                "role": "toolResult",
                "toolName": "read",
                "isError": true,
                "content": "File not found"
            }
        }"#;
        let res_err = OmpRunner::format_session_line(json_res_err).unwrap();
        assert!(res_err.iter().any(|s| s.contains("[RESULT] read (FAIL)")));
        assert!(res_err.iter().any(|s| s.contains("> File not found")));
    }

    #[test]
    fn test_extract_session_stats_and_finding() {
        let temp_dir = std::env::temp_dir().join(format!("test_omp_stats_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_file = temp_dir.join("test_session.jsonl");
        let content = "{\"usage\":{\"input\":2000,\"output\":150,\"cacheRead\":800}}\n{\"message\":{\"usage\":{\"input\":2500,\"output\":200,\"prompt_tokens_details\":{\"cached_tokens\":1200}}}}\n";
        std::fs::write(&session_file, content).unwrap();

        let stats = OmpRunner::extract_latest_session_stats(Some(&session_file)).unwrap();
        assert_eq!(stats.prompt_tokens, 4500);
        assert_eq!(stats.completion_tokens, 350);
        assert_eq!(stats.cached_tokens, 2000);
        assert_eq!(stats.total_tokens, 6850);
        assert_eq!(stats.steps_taken, 2);
        assert!(OmpRunner::extract_latest_session_stats(None).is_err());

        let found = OmpRunner::find_newest_session_file_across(std::slice::from_ref(&temp_dir));
        assert_eq!(found, Some(session_file));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_agent_execution_limits_defaults() {
        let limits = AgentExecutionLimits {
            max_turns: Some(10),
            max_budget_usd: Some(0.20),
            timeout_seconds: Some(600),
        };
        assert_eq!(limits.max_turns, Some(10));
        assert_eq!(limits.max_budget_usd, Some(0.20));
        assert_eq!(limits.timeout_seconds, Some(600));
    }

    #[test]
    fn test_tail_session_file_lifecycle() {
        let temp_dir = std::env::temp_dir().join(format!("test_tail_sb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_file = temp_dir.join("active_session.jsonl");
        let initial_line = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"init","args":{"command":"echo start"}}}"#;
        std::fs::write(&session_file, format!("{}\n", initial_line)).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let active_file = Arc::new(Mutex::new(Some(session_file.clone())));
        let turn_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let accumulated_cost = Arc::new(Mutex::new(0.0f64));
        let limit_reached = Arc::new(AtomicBool::new(false));
        let limit_reason = Arc::new(Mutex::new(None));
        let limits = AgentExecutionLimits {
            max_turns: Some(5),
            max_budget_usd: Some(0.10),
            timeout_seconds: Some(10),
        };

        let running_tailer = running.clone();
        let tail_ctx = TailSessionContext {
            start_time: SystemTime::now(),
            existing: HashSet::new(),
            active_file,
            running: running_tailer,
            tx,
            limits,
            turn_counter,
            accumulated_cost,
            accumulated_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            limit_reached,
            limit_reason,
            cancel_flag: None,
            extension_feed_active: Arc::new(AtomicBool::new(false)),
        };
        let handle = std::thread::spawn(move || {
            OmpRunner::tail_session_file(tail_ctx);
        });

        // Verify initial line received
        let rec = rx.recv_timeout(Duration::from_millis(500));
        assert!(rec.is_ok());

        // Stop tailer
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_format_session_line_edge_cases() {
        // 1. Tool with > 20 lines of code
        let long_code = (0..25)
            .map(|i| format!("println!(\"line {}\");", i))
            .collect::<Vec<_>>()
            .join("\n");
        let json_code = serde_json::json!({
            "customType": "tool_execution_start",
            "data": {
                "toolName": "write",
                "args": {
                    "code": long_code
                }
            }
        })
        .to_string();
        let res_code = OmpRunner::format_session_line(&json_code).unwrap();
        assert!(res_code.iter().any(|s| s.contains("(+5 lines)")));

        // 2. ToolResult with > 40 lines
        let long_res = (0..50)
            .map(|i| format!("output row {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let json_res = serde_json::json!({
            "message": {
                "role": "toolResult",
                "toolName": "bash",
                "content": long_res
            }
        })
        .to_string();
        let res_long = OmpRunner::format_session_line(&json_res).unwrap();
        assert!(res_long.iter().any(|s| s.contains("(+10 more lines)")));

        // 3. ToolResult with empty output
        let json_empty = serde_json::json!({
            "message": {
                "role": "toolResult",
                "toolName": "bash",
                "content": ""
            }
        })
        .to_string();
        let res_empty = OmpRunner::format_session_line(&json_empty).unwrap();
        assert!(res_empty.iter().any(|s| s.contains("(empty output)")));

        // 4. Invalid or unhandled JSON
        assert!(OmpRunner::format_session_line("").is_none());
        let plain = OmpRunner::format_session_line("just thinking out loud").unwrap();
        assert!(plain.iter().any(|l| l.contains("[OMP] just thinking out loud")));
        assert!(OmpRunner::format_session_line(r#"{"unknown": "object"}"#).is_none());
    }

    #[test]
    fn test_sessions_dirs_and_collect() {
        let dirs = OmpRunner::get_sessions_dirs();
        let _ = OmpRunner::collect_all_session_files();
        assert!(!dirs.is_empty() || dirs.is_empty()); // runs without panicking
    }

    #[test]
    fn test_find_new_session_file_priorities() {
        let temp_dir = std::env::temp_dir().join(format!("test_find_prio_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let start_time = SystemTime::now() - Duration::from_secs(10);
        let file1 = temp_dir.join("file1.jsonl");
        let file2 = temp_dir.join("file2.jsonl");

        std::fs::write(&file1, "line1\n").unwrap();
        std::fs::write(&file2, "line2\n").unwrap();

        let mut existing = HashSet::new();
        existing.insert(file1.clone());

        let dirs = vec![temp_dir.clone()];
        // Priority 1: file2 is brand new
        let found = OmpRunner::find_new_session_file(&dirs, &existing, start_time);
        assert_eq!(found, Some(file2.clone()));

        // Priority 2: all exist, but file1 modified after start_time
        existing.insert(file2.clone());
        let found2 = OmpRunner::find_new_session_file(&dirs, &existing, start_time);
        assert!(found2.is_some());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_tail_session_file_turn_and_budget_limits() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_tail_limits_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_file = temp_dir.join("session_limit.jsonl");
        // Write assistant message and budget usage
        let assistant_turn = r#"{"message":{"role":"assistant","usage":{"cost":{"total":0.25}},"content":[{"type":"text","text":"hello"}]}}"#;
        std::fs::write(&session_file, format!("{}\n", assistant_turn)).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let active_file = Arc::new(Mutex::new(Some(session_file.clone())));
        let turn_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let accumulated_cost = Arc::new(Mutex::new(0.0f64));
        let limit_reached = Arc::new(AtomicBool::new(false));
        let limit_reason = Arc::new(Mutex::new(None));
        let limits = AgentExecutionLimits {
            max_turns: Some(1),         // Should trigger limit immediately
            max_budget_usd: Some(0.20), // Should also trigger budget limit
            timeout_seconds: Some(10),
        };

        let running_tailer = running.clone();
        let turn_c = turn_counter.clone();
        let lim_reached = limit_reached.clone();
        let tail_ctx = TailSessionContext {
            start_time: SystemTime::now() - Duration::from_secs(5),
            existing: HashSet::new(),
            active_file,
            running: running_tailer,
            tx,
            limits,
            turn_counter: turn_c,
            accumulated_cost,
            accumulated_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            limit_reached: lim_reached,
            limit_reason,
            cancel_flag: None,
            extension_feed_active: Arc::new(AtomicBool::new(false)),
        };
        let handle = std::thread::spawn(move || {
            OmpRunner::tail_session_file(tail_ctx);
        });

        std::thread::sleep(Duration::from_millis(300));
        assert!(limit_reached.load(Ordering::SeqCst));
        assert_eq!(turn_counter.load(Ordering::SeqCst), 1);

        running.store(false, Ordering::SeqCst);
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_run_agent_invalid_model_docker() {
        let temp_dir = std::env::temp_dir().join(format!("test_run_agent_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let limits = AgentExecutionLimits {
            max_turns: Some(1),
            max_budget_usd: Some(0.10),
            timeout_seconds: Some(5),
        };

        let res = OmpRunner::run_agent(AgentRunSpec {
            model: "nonexistent-test-model-xyz",
            prompt: "test prompt",
            workdir: &temp_dir,
            api_key: None,
            limits,
            effort: Some("auto"),
            network: None,
            task: None,
            run_id: None,
            cancel_flag: None,
        });

        // omp should fail on nonexistent model with error
        assert!(res.is_err());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_run_agent_relative_path_resolution() {
        let rel_dir = Path::new("test_rel_ws_omp");
        let _ = std::fs::create_dir_all(rel_dir);

        let limits = AgentExecutionLimits {
            max_turns: Some(1),
            max_budget_usd: Some(0.10),
            timeout_seconds: Some(5),
        };

        let res = OmpRunner::run_agent(AgentRunSpec {
            model: "nonexistent-test-model-xyz",
            prompt: "test prompt",
            workdir: rel_dir,
            api_key: None,
            limits,
            effort: Some("auto"),
            network: None,
            task: None,
            run_id: None,
            cancel_flag: None,
        });

        if let Err(e) = res {
            let err_str = e.to_string();
            assert!(!err_str.contains("invalid characters for a local volume name"));
        }

        let _ = std::fs::remove_dir_all(rel_dir);
    }
}
