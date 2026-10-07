use anyhow::{anyhow, Result};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::sandbox::omp::{
    AgentRunSpec, OmpBillingError, OmpExecutionError, OmpRunner, OmpSessionStats,
};

/// Pinned OpenCode version. Must match `ARG OPENCODE_VERSION` in
/// `Dockerfile.sandbox`; bump together (same discipline as OMP_VERSION).
pub const OPENCODE_VERSION: &str = "2.0.24";

/// Second harness adapter: runs the agent loop with the OpenCode CLI
/// (`opencode run`) inside the same `subdollar-sandbox` container family.
///
/// Verified spikes (2026-10-07, `opencode/ling-3.1-flash-free`):
/// - headless `run --format json --auto` exits 0 and writes files through
///   the host background service; stored Console auth works non-interactively.
///
/// Free-tier caveat (measured 2026-10-07): `opencode/*-free` models 403
/// (`FreeTierError: usable only from within OpenCode`) under `--standalone`,
/// which the sandbox requires. Prefer `openrouter/*` ids (env key) for
/// bench trials; free-tier models only work through the host background
/// service, unreachable from the sandbox.
/// - per-step `step_finish` events carry `{tokens:{input,output,reasoning,
///   cache:{read,write}}, cost}`, summed here into `OmpSessionStats`;
/// - sessions isolate by explicit `--session <run_id>` (+ per-run workdir);
/// - effort is request-only: `run` has no thinking flag, models report
///   `variant:"default"` — recorded + hinted in-prompt, never enforced.
pub struct OpenCodeRunner;

impl OpenCodeRunner {
    /// Stats mapping (documented, lossless):
    /// prompt = Σ input, completion = Σ (output + reasoning),
    /// cached = Σ cache.read, total = Σ (input + output + reasoning).
    /// Reasoning bills provider-side as completion, so it joins completion.
    fn add_step(stats: &mut OmpSessionStats, tokens: &serde_json::Value, cost: f64) {
        let u64f = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_u64()).unwrap_or(0);
        let input = u64f(tokens.get("input"));
        let output = u64f(tokens.get("output"));
        let reasoning = u64f(tokens.get("reasoning"));
        let cache_read = tokens
            .get("cache")
            .and_then(|c| c.get("read"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        stats.prompt_tokens += input;
        stats.completion_tokens += output + reasoning;
        stats.cached_tokens += cache_read;
        stats.total_tokens += input + output + reasoning;
        let _ = cost;
    }

    fn cap_line(s: &str) -> String {
        const MAX: usize = 1500;
        if s.len() > MAX {
            format!("{}…[truncated]", &s[..MAX])
        } else {
            s.to_string()
        }
    }

    /// Host dirs holding stored Console auth (opencode.db) + config.
    /// Mounted into the sandbox like OMP's `~/.omp` mount. Per-run
    /// `--session` ids keep concurrent runs apart in the shared db.
    fn host_opencode_dirs() -> (PathBuf, PathBuf) {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
        let base = PathBuf::from(home);
        (
            base.join(".config/opencode"),
            base.join(".local/share/opencode"),
        )
    }

    fn write_agents_md(
        workdir: &std::path::Path,
        snippet: &str,
        port: u16,
        test: &str,
        effort: &str,
    ) {
        let content = format!(
            r#"# Instructions for Autonomous Coding Agent
1. **TOOL CALL REQUIREMENT**: You have tools (`write`, `bash`, `edit`, `read`). You MUST use the `write` tool to create every file directly in /workspace. NEVER output code in conversational text without calling `write`.
2. **RAW TCP SOCKETS ONLY (CRITICAL)**:
{}
3. **ENTRYPOINT REQUIRED**: You must create a working `Dockerfile` or executable `./start.sh` listening on port {}. Test your server with `{}` using the `bash` tool before finishing.
4. **CONCISE REASONING**: Keep internal thoughts brief (< 150 words). Immediately invoke tools to write code and test it.
5. **REASONING EFFORT REQUESTED**: {} (best-effort: honored by some models, ignored by others).
"#,
            snippet, port, test, effort
        );
        let _ = std::fs::write(workdir.join("AGENTS.md"), content);
    }
}

impl crate::sandbox::AgentRunner for OpenCodeRunner {
    fn run_agent_with_logger<F>(spec: AgentRunSpec<'_>, mut log_fn: F) -> Result<OmpSessionStats>
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
            task: _,
            run_id,
            cancel_flag,
        } = spec;
        let effort_level = effort.unwrap_or("high").trim().to_string();

        let start_time = SystemTime::now();
        let container_id = run_id.unwrap_or("run").replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
            "",
        );
        let container_name = format!("subdollar-opencode-agent-{}", container_id);
        // opencode session IDs must start with "ses" (it validates the
        // `--session` value); prefix the sanitized run id so each trial
        // gets an explicit, traceable session. `--session` creates it when
        // absent, continues it when present.
        let session_id = format!("ses_{}", container_id);
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &canonical_workdir,
                std::fs::Permissions::from_mode(0o755),
            );
        }

        // Same task text + entrypoint contract as OMP (shared helper).
        let (target_port, test_cmd, guidance_snippet) = OmpRunner::task_guidance(spec.task, prompt);
        let effort_note = format!("reasoning effort requested: {}", effort_level);
        let effective_prompt = format!(
            "{}\n\n==================================================\nCRITICAL DIRECTIVES FOR AUTONOMOUS AGENT EXECUTION:\n1. TOOL CALL REQUIREMENT: You are an autonomous AI coding agent with filesystem tools (`write`, `bash`, `read`, `edit`). You MUST execute your tools immediately to create your implementation files directly in /workspace. DO NOT merely explain the plan in conversational text without calling `write`.\n2. RAW TCP SOCKETS ONLY (MANDATORY):\n{}\n3. ENTRYPOINT: Create a working executable `./start.sh` or `Dockerfile` listening on port {}. Test your server with `{}` using `bash` before finishing.\n4. CONCISE REASONING: Keep internal thinking brief (< 150 words). Do not spend output tokens on exhaustive planning in thought blocks. Immediately invoke tools (`write`, `bash`, `read`) on turn 1 to start creating the server and verifying it.\n5. {}.\n==================================================",
            prompt, guidance_snippet, target_port, test_cmd, effort_note
        );
        Self::write_agents_md(
            &canonical_workdir,
            guidance_snippet,
            target_port,
            test_cmd,
            &effort_level,
        );

        // opencode accepts both `opencode/*` (stored Console auth) and
        // `openrouter/*` (OPENROUTER_API_KEY env) ids: identity mapping.
        let effective_model = model.to_string();

        let mut cmd = Command::new("docker");
        cmd.arg("run")
            .arg("--rm")
            .arg("--label")
            .arg(format!("sdb.run={}", container_id))
            .arg("--label")
            .arg("sdb.role=opencode-agent")
            .arg("--name")
            .arg(&container_name)
            .arg("--memory=3g")
            .arg("--cpus=3.0")
            .arg("--pids-limit=512")
            .arg("--cap-drop=ALL")
            .arg("--security-opt=no-new-privileges")
            .arg("--add-host=host.docker.internal:host-gateway")
            .arg("-v")
            .arg(format!("{}:/workspace", canonical_workdir.display()))
            .arg("-w")
            .arg("/workspace")
            .arg("-e")
            .arg("HOME=/home/ubuntu");
        // Stored Console auth (opencode.db) + config, mirroring OMP's
        // `~/.omp` mount. Unique `--session` per run keeps trials apart.
        let (cfg_dir, data_dir) = Self::host_opencode_dirs();
        let _ = std::fs::create_dir_all(&cfg_dir);
        let _ = std::fs::create_dir_all(&data_dir);
        cmd.arg("-v").arg(format!(
            "{}:/home/ubuntu/.config/opencode",
            cfg_dir.display()
        ));
        cmd.arg("-v").arg(format!(
            "{}:/home/ubuntu/.local/share/opencode",
            data_dir.display()
        ));

        if let Some(net) = network {
            cmd.arg("--network").arg(net);
        } else {
            cmd.arg("--network=bridge");
        }
        // Egress posture identical to OMP: proxy env when filtered,
        // fail closed when the sidecar cannot start.
        let no_proxy = "ref-redis,ref-http,egress-proxy,localhost,127.0.0.1";
        let egress_on = crate::sandbox::docker::egress_filter_enabled();
        if let Some(proxy_url) = (if egress_on { network } else { None })
            .and_then(|net| {
                net.strip_prefix("subdollar-net-").map(|id| {
                    crate::sandbox::docker::SandboxManager::with_id(id).start_egress_proxy()
                })
            })
            .flatten()
        {
            for var in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                cmd.arg("-e").arg(format!("{var}={proxy_url}"));
            }
            for var in ["NO_PROXY", "no_proxy"] {
                cmd.arg("-e").arg(format!("{var}={no_proxy}"));
            }
        } else {
            for var in ["NO_PROXY", "no_proxy"] {
                cmd.arg("-e").arg(format!("{var}={no_proxy}"));
            }
            if egress_on {
                anyhow::bail!(
                    "egress filter is enabled but the allowlist proxy sidecar failed to start; \
                     refusing to run the agent unfiltered"
                );
            }
        }

        let effective_key = api_key
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok());
        if let Some(ref key) = effective_key {
            cmd.arg("-e").arg(format!("OPENROUTER_API_KEY={}", key));
        }
        if let Some(max_turns) = limits.max_turns {
            cmd.arg("-e")
                .arg(format!("SDB_BENCH_MAX_TURNS={}", max_turns));
        }

        cmd.arg("subdollar-sandbox")
            .arg("opencode")
            .arg("run")
            .arg("--standalone")
            .arg("--format")
            .arg("json")
            .arg("--session")
            .arg(&session_id)
            .arg("--title")
            .arg(format!("sdb-{}", container_id))
            .arg("-m")
            .arg(&effective_model)
            .arg("--auto")
            .arg(&effective_prompt);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let _ = tx.send(format!(
            "[TELEMETRY] turn=0 max_turns={} spent=0.000000 tokens=0",
            limits.max_turns.unwrap_or(0)
        ));
        let _ = tx.send(format!(
            "[LIVE] Agent initialized (opencode {}). Max turns: {}, Budget: ${:.2}",
            OPENCODE_VERSION,
            limits.max_turns.unwrap_or(0),
            limits.max_budget_usd.unwrap_or(0.0)
        ));
        let _ = tx.send(format!(
            "[OPENCODE-PROMPT] model={} effort={} (request-only) cwd=/workspace session={}",
            effective_model, effort_level, container_id
        ));

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn opencode in docker: {}", e))?;
        let child_id = child.id();

        // Reader: NDJSON `--format json` stream -> bench log protocol.
        // step_start = turn, tool_use = [ACTION], text = [RESPONSE],
        // step_finish = token/cost accumulation + [TELEMETRY].
        let stats = Arc::new(std::sync::Mutex::new(OmpSessionStats::default()));
        let turns = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let spent = Arc::new(std::sync::Mutex::new(0.0f64));
        let limit_hit = Arc::new(AtomicBool::new(false));
        let limit_msg = Arc::new(std::sync::Mutex::new(None::<String>));
        let billing_tripped = Arc::new(AtomicBool::new(false));
        if let Some(stdout) = child.stdout.take() {
            let tx_o = tx.clone();
            let stats_o = stats.clone();
            let turns_o = turns.clone();
            let spent_o = spent.clone();
            let limit_hit_o = limit_hit.clone();
            let limit_msg_o = limit_msg.clone();
            let billing_o = billing_tripped.clone();
            let running_o = running.clone();
            let max_turns = limits.max_turns.unwrap_or(0);
            let max_budget = limits.max_budget_usd.unwrap_or(f64::MAX);
            let cname = container_name.clone();
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if !running_o.load(Ordering::SeqCst) {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let v: serde_json::Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(_) => {
                            let _ = tx_o.send(format!("[LOG] {}", Self::cap_line(trimmed)));
                            continue;
                        }
                    };
                    let etype = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match etype {
                        "step_start" => {
                            let n = turns_o.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = tx_o.send(format!("[LIVE] Turn {} started", n));
                            if max_turns > 0 && n > max_turns {
                                *limit_msg_o.lock().unwrap() =
                                    Some(format!("max turns {} reached", max_turns));
                                limit_hit_o.store(true, Ordering::SeqCst);
                                running_o.store(false, Ordering::SeqCst);
                                let _ = Command::new("docker").args(["rm", "-f", &cname]).output();
                                break;
                            }
                        }
                        "tool_use" => {
                            let part = &v;
                            let tool = part
                                .pointer("/part/tool")
                                .or_else(|| part.get("tool"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("tool");
                            let _ =
                                tx_o.send(format!("[ACTION] {} {}", tool, Self::cap_line(trimmed)));
                        }
                        "text" => {
                            let text = v
                                .pointer("/part/text")
                                .or_else(|| v.get("text"))
                                .and_then(|t| t.as_str())
                                .unwrap_or(trimmed);
                            if let Some(reason) =
                                crate::sandbox::omp::classify_billing_error(trimmed)
                                    .or_else(|| crate::sandbox::omp::classify_billing_error(text))
                            {
                                let _ = tx_o.send(format!("[BILLING] {}", reason));
                                billing_o.store(true, Ordering::SeqCst);
                            }
                            let _ = tx_o.send(format!("[RESPONSE] {}", Self::cap_line(text)));
                        }
                        "step_finish" => {
                            let tokens = v
                                .pointer("/part/tokens")
                                .or_else(|| v.get("tokens"))
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let cost = v
                                .pointer("/part/cost")
                                .or_else(|| v.get("cost"))
                                .and_then(|c| c.as_f64())
                                .unwrap_or(0.0);
                            Self::add_step(&mut stats_o.lock().unwrap(), &tokens, cost);
                            *spent_o.lock().unwrap() += cost;
                            let st = stats_o.lock().unwrap().clone();
                            let n = turns_o.load(Ordering::SeqCst);
                            let total_tok =
                                st.prompt_tokens + st.completion_tokens + st.cached_tokens;
                            let _ = tx_o.send(format!(
                                "[TELEMETRY] turn={} max_turns={} spent={:.6} tokens={}",
                                n,
                                max_turns,
                                *spent_o.lock().unwrap(),
                                total_tok
                            ));
                            if *spent_o.lock().unwrap() > max_budget {
                                *limit_msg_o.lock().unwrap() =
                                    Some(format!("budget ${:.2} exceeded", max_budget));
                                limit_hit_o.store(true, Ordering::SeqCst);
                                running_o.store(false, Ordering::SeqCst);
                                let _ = Command::new("docker").args(["rm", "-f", &cname]).output();
                                break;
                            }
                        }
                        _ => {
                            let _ = tx_o.send(format!("[LOG] {}", Self::cap_line(trimmed)));
                        }
                    }
                }
            });
        }
        // Drain stderr for billing phrases (provider errors surface there).
        // Kept in full: the tail joins the failure message when the agent
        // exits non-zero, so a dead trial names its cause (auth, 429, …)
        // instead of the generic "exited non-zero" below.
        let stderr_tail = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let tx_e = tx.clone();
            let billing_e = billing_tripped.clone();
            let tail_e = stderr_tail.clone();
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    {
                        let mut tail = tail_e.lock().unwrap();
                        tail.push(trimmed.to_string());
                        if tail.len() > 20 {
                            tail.remove(0);
                        }
                    }
                    if let Some(reason) = crate::sandbox::omp::classify_billing_error(trimmed) {
                        let _ = tx_e.send(format!("[BILLING] {}", reason));
                        billing_e.store(true, Ordering::SeqCst);
                    }
                }
            });
        }

        // Logger pump + timeout/cancel supervision (mirrors OMP structure).
        // Drop our `tx`: reader threads hold their own clones, so the pump
        // ends when they finish. Holding it here deadlocks pump join below
        // (rx never closes while a sender lives).
        drop(tx);
        let timeout = limits
            .timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1800));
        let deadline = start_time + timeout;
        let pump_running = running.clone();
        let pump_cancel = cancel_flag.clone();
        let pump_handle = std::thread::spawn(move || {
            for line in rx {
                log_fn(line);
                if !pump_running.load(Ordering::SeqCst) {
                    break;
                }
                if SystemTime::now() > deadline
                    || pump_cancel
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                {
                    break;
                }
            }
        });

        // Wait with timeout; kill the container on breach.
        let waited = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    let timed_out = SystemTime::now() > deadline;
                    let cancelled = cancel_flag
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false);
                    if timed_out || cancelled || !running.load(Ordering::SeqCst) {
                        let _ = Command::new("docker")
                            .args(["rm", "-f", &container_name])
                            .output();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(_) => break None,
            }
        };
        running.store(false, Ordering::SeqCst);
        let _ = child.wait();
        let _ = pump_handle.join();
        let _ = Command::new("docker")
            .args(["rm", "-f", &container_name])
            .output();
        let _ = child_id;

        // AGENTS.md is runner-owned scaffolding, never candidate code.
        let _ = std::fs::remove_file(canonical_workdir.join("AGENTS.md"));

        if billing_tripped.load(Ordering::SeqCst) {
            let st = stats.lock().unwrap().clone();
            // Same envelope as OMP: queue.rs pauses the wave on "billing:".
            return Err(anyhow::Error::new(OmpBillingError {
                reason: "opencode provider auth/billing/rate-limit tripped mid-session (see [BILLING] log)"
                    .to_string(),
                partial_stats: st,
            }));
        }
        let limited = limit_hit.load(Ordering::SeqCst);
        let msg = limit_msg.lock().unwrap().clone().unwrap_or_default();
        let st = stats.lock().unwrap().clone();
        let tail = stderr_tail.lock().unwrap().join(" | ");
        match waited {
            Some(status) if status.success() && !limited => Ok(st),
            _ => Err(anyhow!(OmpExecutionError {
                message: if limited {
                    format!("opencode limit reached: {}", msg)
                } else if SystemTime::now() > deadline {
                    "opencode timeout reached".to_string()
                } else {
                    let code = waited.map(|s| s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string())).unwrap_or_else(|| "no-exit".to_string());
                    match tail.is_empty() {
                        true => format!("opencode agent exited (code {}) with no further detail", code),
                        false => format!("opencode agent exited (code {}): {}", code, Self::cap_line(&tail)),
                    }
                },
                partial_stats: st,
            })),
        }
    }
}
