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
    /// Effective-cost ceiling for this round (API + compute floor). The
    /// campaign cap's reserve-adjusted headroom, minus what this attempt has
    /// already spent (round-17 P0).
    pub max_effective_usd: Option<f64>,
    pub timeout_seconds: Option<u64>,
}

impl Default for AgentExecutionLimits {
    fn default() -> Self {
        Self {
            max_turns: Some(100),
            max_budget_usd: Some(0.50),
            max_effective_usd: None,
            timeout_seconds: Some(900), // 15 minutes default
        }
    }
}

/// Effective spend = metered API + the compute floor of elapsed time and
/// tokens. The campaign cap's headroom is checked against this, while the
/// per-round API budget is checked separately (round-17 P0).
pub fn cap_headroom_reason(
    cap_effective_usd: f64,
    api_spent_usd: f64,
    elapsed_seconds: f64,
    total_tokens: u64,
    cached_tokens: u64,
) -> Option<String> {
    let floor = crate::cost::calculator::ModelPricing::compute_cost(
        elapsed_seconds,
        total_tokens,
        cached_tokens,
    );
    let spend = api_spent_usd + floor;
    (spend >= cap_effective_usd).then(|| {
        format!(
            "[LIMIT] Reached the campaign cap's headroom (${spend:.4} / ${cap_effective_usd:.4} effective)"
        )
    })
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct OmpSessionStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
    pub steps_taken: u32,
    /// Live-metered provider spend for this attempt (sum of every call).
    /// The campaign cap is enforced against this plus the compute floor.
    pub cost_usd: f64,
    /// The largest single provider call in this attempt (the campaign cap's
    /// reserve).
    pub max_call_usd: f64,
    /// Effective effort as negotiated, harness-interpreted: OMP records the
    /// session's `thinking_level_change` value (CLI downgrades included);
    /// OpenCode records `#<variant>` or `prose-only`. Requested effort
    /// lives on the manifest/row `effort` field; this one says what ran.
    /// None = unknown (no session file, pre-field row).
    pub effort_effective: Option<String>,
}

/// The `omp` argv after its runner flags: every option, then `--`, then the
/// positional prompt (round-13 R1: a note starting with `---` was parsed as
/// an unknown flag, the agent ran 0 turns, and the verifier graded the
/// previous ticket's code). Pure so a unit test can pin the shape.
fn omp_task_args(
    prompt: &str,
    model: &str,
    session_dir: &str,
    policy: Option<&str>,
    timeout_seconds: Option<u64>,
    thinking: &str,
) -> Vec<String> {
    let mut args = vec![
        format!("--model={model}"),
        "--cwd=/workspace".to_string(),
        format!("--session-dir={session_dir}"),
    ];
    // Spike: bench policy extension (opt-out via SDB_BENCH_POLICY=0).
    if let Some(p) = policy {
        args.push("-e".to_string());
        args.push(p.to_string());
    }
    if let Some(secs) = timeout_seconds {
        args.push(format!("--max-time={secs}s"));
    }
    args.push(format!("--thinking={thinking}"));
    args.push("--print-thoughts".to_string());
    crate::sandbox::push_prompt_args(&mut args, prompt);
    args
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
/// `max` passes through verbatim for every provider: the omp CLI downgrades
/// unsupported levels itself (measured 2026-10-09: `--thinking=max` on
/// ling-3.1-flash records `thinkingLevel: "high"` in the session, exit 0,
/// no HTTP 400). The pre-clamp that rewrote max→high runner-side was
/// redundant with that downgrade and hid the requested level from the
/// record, so it was removed; the session's `thinking_level_change` event
/// (surfaced as [THINKING]) is the source of truth for what ran.
fn resolve_thinking_level(effort: Option<&str>, _effective_model: &str) -> (String, String) {
    let requested = effort.map(|e| e.trim().to_lowercase()).unwrap_or_default();
    let level = match requested.as_str() {
        "" | "default" | "auto" => "high".to_string(),
        other => other.to_string(),
    };
    (level, String::new())
}

/// Consume a model-signaled early-completion flag: true once when
/// `<workdir>/READY` exists, removing it so a stale flag can never fire
/// twice (or fire for a later trial reusing the dir). Both runners poll
/// this; the pipeline also clears it at setup and around each verify.
/// Missing dir or missing file reads as absent, never an error.
///
/// A directory named READY is deliberately not a signal (touch creates
/// files; a dir is pathological, and the old exists()-check would have
/// killed the agent loop over it).
pub(crate) fn take_ready_signal(workdir: &std::path::Path) -> bool {
    let p = workdir.join("READY");
    if !p.is_file() {
        return false;
    }
    std::fs::remove_file(&p).is_ok()
}

/// Billing tripwire: classify provider session errors that must pause the
/// wave instead of scoring a 0% model failure. Matches HTTP billing/auth/
/// throttle statuses in session JSON or stderr text (402/401/429 + phrases).
/// Returns a human reason when tripped, else None.
/// Structured provider-error match. Bare `401`/`402`/`429` digit tokens
/// appear routinely in free-form agent output (toolCallIds
/// `...c3d429b`, line numbers `401:`, token counts `"output":429`), so
/// they NEVER match here. A trip requires one of:
/// - a JSON error envelope with a numeric `code`/`status` field 401/402/429,
/// - an HTTP status line (`HTTP/1.1 402 ...`),
/// - a provider phrase (payment required / insufficient credit / rate limit /
///   unauthorized + key/token/auth).
///
/// A blank line ends the list; the caller note below is ordinary prose, not a
/// continuation item.
///
/// Callers: session-jsonl lines are parsed as JSON first (structured arm);
/// raw stderr lines use the text arm (status-line + phrases only).
pub fn classify_billing_error(text: &str) -> Option<String> {
    let low = text.to_lowercase();
    // Structured arm: JSON envelope with numeric error code.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        let code = v
            .pointer("/error/code")
            .or_else(|| v.pointer("/error/status"))
            .or_else(|| v.get("statusCode"))
            .or_else(|| v.get("code"))
            .and_then(|c| {
                c.as_u64()
                    .or_else(|| c.as_str().and_then(|s| s.parse::<u64>().ok()))
            });
        if let Some(c) = code {
            if c == 402 {
                return Some("provider billing exhausted (HTTP 402)".to_string());
            }
            if c == 401 {
                return Some("provider auth rejected (HTTP 401)".to_string());
            }
            if c == 429 {
                return Some("provider rate-limited (HTTP 429)".to_string());
            }
        }
        // Bare error slugs: {"error":"rate_limit_exceeded"} style.
        if v.get("error")
            .and_then(|e| e.as_str())
            .map(|e| {
                let l = e.to_lowercase();
                l.contains("rate_limit")
                    || l.contains("rate limit")
                    || l.contains("payment_required")
                    || l.contains("insufficient_credit")
            })
            .unwrap_or(false)
        {
            let l = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap()
                .to_lowercase();
            if l.contains("rate_limit") || l.contains("rate limit") {
                return Some("provider rate-limited (HTTP 429)".to_string());
            }
            return Some("provider billing exhausted (HTTP 402)".to_string());
        }
        // Phrase match inside error message fields only (not free text).
        let msg = v
            .pointer("/error/message")
            .or_else(|| v.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_lowercase();
        if !msg.is_empty() {
            if msg.contains("payment required")
                || msg.contains("insufficient credit")
                || msg.contains("credit exhausted")
                || msg.contains("out of credit")
            {
                return Some("provider billing exhausted (HTTP 402)".to_string());
            }
            if msg.contains("rate limit")
                || msg.contains("rate_limit")
                || msg.contains("rate-limit")
                || msg.contains("rate-limited")
                || msg.contains("too many requests")
            {
                return Some("provider rate-limited (HTTP 429)".to_string());
            }
            if msg.contains("unauthorized")
                && (msg.contains("key") || msg.contains("token") || msg.contains("auth"))
            {
                return Some("provider auth rejected (HTTP 401)".to_string());
            }
        }
        return None;
    }
    // Text arm (stderr): HTTP status line or provider phrase. Never digits.
    let status_line = low.contains("http/1.1 402")
        || low.contains("http/1.0 402")
        || low.contains("http/2 402")
        || low.contains("status 402")
        || low.contains("status: 402");
    if status_line
        || low.contains("payment required")
        || low.contains("insufficient credit")
        || low.contains("credit exhausted")
        || low.contains("out of credit")
    {
        return Some("provider billing exhausted (HTTP 402)".to_string());
    }
    let auth_line = low.contains("http/1.1 401")
        || low.contains("http/1.0 401")
        || low.contains("http/2 401")
        || low.contains("status 401")
        || low.contains("status: 401");
    if auth_line
        || (low.contains("unauthorized")
            && (low.contains("key") || low.contains("token") || low.contains("auth")))
    {
        return Some("provider auth rejected (HTTP 401)".to_string());
    }
    let limit_line = low.contains("http/1.1 429")
        || low.contains("http/1.0 429")
        || low.contains("http/2 429")
        || low.contains("status 429")
        || low.contains("status: 429");
    if limit_line
        || low.contains("rate limit")
        || low.contains("rate_limit")
        || low.contains("rate-limit")
        || low.contains("rate-limited")
        || low.contains("too many requests")
    {
        return Some("provider rate-limited (HTTP 429)".to_string());
    }
    None
}

pub struct TailSessionContext {
    pub start_time: SystemTime,
    /// This run's own session directory on the host. Never the shared parent:
    /// every concurrent run writes there.
    pub session_dir: PathBuf,
    pub existing: HashSet<PathBuf>,
    pub active_file: Arc<Mutex<Option<PathBuf>>>,
    pub running: Arc<AtomicBool>,
    pub tx: mpsc::Sender<String>,
    pub limits: AgentExecutionLimits,
    pub turn_counter: Arc<std::sync::atomic::AtomicU32>,
    pub accumulated_cost: Arc<Mutex<f64>>,
    pub max_call_usd: Arc<Mutex<f64>>,
    pub accumulated_tokens: Arc<std::sync::atomic::AtomicU64>,
    pub accumulated_cached: Arc<std::sync::atomic::AtomicU64>,
    pub limit_reached: Arc<AtomicBool>,
    pub limit_reason: Arc<Mutex<Option<String>>>,
    pub cancel_flag: Option<Arc<AtomicBool>>,
    /// Set when the spike extension streams BENCH-EVENT lines: the tailer
    /// then renders assistant/tool session lines as token stats only.
    pub extension_feed_active: Arc<AtomicBool>,
    /// Model id used to derive per-event cost from token counts when the
    /// session event carries no explicit `usage.cost.total`.
    pub model: String,
    /// Set when classify_billing_error trips on a session line: the runner
    /// pauses the queue instead of scoring the trial.
    pub billing_tripped: Arc<AtomicBool>,
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
    /// Parallel worker index (0 = no templating). Carried so runners can
    /// render worker-specific ports (AGENTS.md) identically to the prompt.
    pub worker: u8,
}

#[derive(Debug, Clone)]
pub struct OmpExecutionError {
    pub message: String,
    pub partial_stats: OmpSessionStats,
    /// The agent produced any sign of life (a session file, stdout, a turn,
    /// tokens). `false` means it died before its first turn — a harness
    /// fault, never a model verdict (round-14 N3: `steps_taken == 0` alone
    /// also matched a real run whose session file was lost).
    pub started: bool,
}

/// Provider billing/auth/throttle failure: pause the wave, never score.
/// Carries the human reason from classify_billing_error.
#[derive(Debug, Clone)]
pub struct OmpBillingError {
    pub reason: String,
    #[allow(dead_code)]
    pub partial_stats: OmpSessionStats,
}

impl std::fmt::Display for OmpBillingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "billing: {}", self.reason)
    }
}

impl std::error::Error for OmpBillingError {}

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
            worker,
        } = spec;
        info!(
            "Launching OMP agent in Docker sandbox with model: {}, effort: {:?}, limits: {:?}",
            model, effort, limits
        );

        let start_time = SystemTime::now();
        let container_id = run_id.unwrap_or("run").replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
            "",
        );
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
                            let _ = std::fs::set_permissions(
                                entry.path(),
                                std::fs::Permissions::from_mode(0o644),
                            );
                        }
                    }
                }
            }
            let _ = std::fs::set_permissions(
                &canonical_workdir,
                std::fs::Permissions::from_mode(0o755),
            );
        }
        // The agent persists sessions/state under ~/.omp, so this mount stays
        // read-write: omp writes sessions, data, state, and cache inside the
        // container (see --session-dir below). Read-only would break resume.
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
            .arg("--label")
            .arg(format!("sdb.run={}", container_id))
            .arg("--label")
            .arg("sdb.role=omp-agent")
            .arg("--name")
            .arg(&container_name)
            .arg("--memory=3g")
            .arg("--cpus=3.0")
            .arg("--pids-limit=512")
            // Drop all Linux capabilities: the agent compiles and runs
            // untrusted model code. No pcap, no raw sockets, no mounts.
            .arg("--cap-drop=ALL")
            // No new privileges: blocks setuid binaries from escalating
            // even if the image ships one.
            .arg("--security-opt=no-new-privileges")
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

        // Attach to the run's private network (reference aliases ref-redis/ref-http)
        // when provided; fall back to the default bridge. The agent is never on the host
        // network, so host services stay unreachable from model-executed code.
        if let Some(net) = network {
            cmd.arg("--network").arg(net);
        } else {
            cmd.arg("--network=bridge");
        }

        // Egress: per-run allowlist proxy sidecar. OFF by default
        // (SDB_EGRESS_FILTER=1 to enable), and it is NOT an enforcement
        // boundary.
        //
        // What it is: an HTTP(S)_PROXY env var plus a stdlib proxy filtering by
        // hostname, which only binds code that chooses to honour those vars.
        // The run network is `docker network create --subnet ...` with no
        // `--internal` and no iptables or DOCKER-USER rule, so it NATs to the
        // internet and a raw socket bypasses the proxy entirely.
        //
        // MEASURED, 2026-09-30, from inside a bridge-network container with a
        // reference attached to a live run network:
        //   - `getent ref-redis`                      -> resolution fails
        //   - TCP to 172.17.0.1:<host-published ref>  -> connection refused
        //     (the host publishes those on 127.0.0.1, not the bridge gateway)
        //   - scan of all 256 of 10.201.0.0/24 :6379   -> zero reachable
        // Candidates launch on the default bridge with no --network flag
        // (see `connect_candidate_to_network`), so for redis/http/pdf/binproto/
        // wal/pubsub the reference is unreachable: relaying to it is not
        // possible, and an earlier claim here that a socat forwarder would work
        // was wrong. Only proxy candidates join the run network, and
        // there the reference is a functional input - forwarding is the task.
        //
        // Closing it needs a network-level rule, not configuration: the run
        // network is created --internal while the filter is on (see
        // `ensure_network`), so this agent container has no route off its own
        // network and the allowlist proxy sidecar is its only path out. A
        // fetched or pip-installed server can no longer reach the image.
        // If the sidecar cannot be started the run aborts rather than coming
        // up unfiltered - see the bail below.
        //
        // NO_PROXY keeps run-network aliases + localhost direct so the agent's
        // own ref probing bypasses the proxy. Harmless for the candidate, which
        // cannot resolve these names in the first place.
        // Both upper- and lower-case variants: pip/npm/go honor either.
        let no_proxy = "ref-redis,ref-http,ref-sql,egress-proxy,localhost,127.0.0.1";
        let egress_on = crate::sandbox::docker::egress_filter_enabled();
        if let Some(proxy_url) = (if egress_on { network } else { None })
            .and_then(|net| {
                // Derive instance id from network name subdollar-net-<id>.
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
            // Fail closed. With the filter on, the run network is --internal,
            // so a proxy-less agent has no route to the model API and this run
            // is dead anyway. The reason to bail rather than let it run is the
            // other case: a sidecar that died against a network left NAT'd by
            // an earlier unfiltered run would otherwise start this agent with
            // unrestricted egress and quietly grade an unfiltered run.
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

        let (base_port, base_cmd, raw_guidance) = Self::task_guidance(task, prompt);
        let target_port = crate::web::server::worker_port(base_port, worker);
        let test_cmd = crate::config::template_ports_for_worker(base_cmd, base_port, worker);
        let guidance_snippet =
            crate::config::template_ports_for_worker(raw_guidance, base_port, worker);
        let effective_prompt = format!(
            "{}

==================================================
CRITICAL DIRECTIVES FOR AUTONOMOUS AGENT EXECUTION:
1. TOOL CALL REQUIREMENT: You are an autonomous AI coding agent with filesystem tools (`write`, `bash`, `read`, `edit`). You MUST execute your tools immediately to create your implementation files directly in /workspace. DO NOT merely explain the plan in conversational text without calling `write`.
2. RAW TCP SOCKETS ONLY (MANDATORY):
{}
3. ENTRYPOINT: Create a working executable `./start.sh` or `Dockerfile` listening on port {}. Test your server with `{}` using `bash` before finishing.
   Name your binary neutrally (`server`, `sdb-redis`): NEVER `redis-server`, `nginx`, `valkey`, etc. — the anti-cheat scanner disqualifies those names even for binaries you compiled yourself.
4. CONCISE REASONING: Keep internal thinking brief (< 150 words). Do not spend output tokens on exhaustive planning in thought blocks. Immediately invoke tools (`write`, `bash`, `read`) on turn 1 to start creating the server and verifying it.
==================================================",
            prompt, guidance_snippet, target_port, test_cmd
        );

        // Write AGENTS.md into workspace so omp auto-loads instructions across all turns
        let agents_md_path = canonical_workdir.join("AGENTS.md");
        let agents_md_content =
            Self::compose_omp_agents_md(&guidance_snippet, target_port, &test_cmd);
        let _ = std::fs::write(&agents_md_path, agents_md_content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&agents_md_path, std::fs::Permissions::from_mode(0o644));
        }

        cmd.arg("subdollar-sandbox")
            .arg("omp")
            .arg("--approval-mode=yolo")
            .arg("--tools=read,bash,edit,write,grep,glob,lsp")
            .arg("-p");

        let (eff_level, eff_note) = resolve_thinking_level(effort, &effective_model);
        // Scoped per run. Every concurrent run shares /home/ubuntu/.omp, so
        // a shared session dir means the tailer cannot tell whose session it
        // is reading - and takes another run's, importing its tokens.
        let session_dir = format!("/home/ubuntu/.omp/agent/sessions/{container_id}");
        let tail = omp_task_args(
            &effective_prompt,
            &effective_model,
            &session_dir,
            bench_policy_path.as_deref(),
            limits.timeout_seconds,
            &eff_level,
        );
        for a in tail {
            cmd.arg(a);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        // This run's session directory on the host. It sits inside the shared
        // ~/.omp mount so the container writes here and the host reads here,
        // but only this run's sessions are ever visible to this run.
        let session_dir = host_sessions_dir.join(&container_id);
        let _ = std::fs::create_dir_all(&session_dir);
        let existing_files = Self::collect_all_session_files_in(std::slice::from_ref(&session_dir));
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
        let max_call_usd = Arc::new(Mutex::new(0.0f64));
        let accumulated_tokens = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let accumulated_cached = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
        let billing_tripped = Arc::new(AtomicBool::new(false));
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
            session_dir,
            start_time,
            existing: existing_files,
            active_file: active_file.clone(),
            running: running.clone(),
            tx: tx.clone(),
            limits,
            turn_counter: turn_counter.clone(),
            accumulated_cost: accumulated_cost.clone(),
            max_call_usd: max_call_usd.clone(),
            accumulated_tokens: accumulated_tokens.clone(),
            accumulated_cached: accumulated_cached.clone(),
            limit_reached: limit_reached.clone(),
            limit_reason: limit_reason.clone(),
            cancel_flag: cancel_flag.clone(),
            extension_feed_active: extension_feed.clone(),
            model: effective_model.clone(),
            billing_tripped: billing_tripped.clone(),
        };

        let tailer_handle = thread::spawn(move || {
            Self::tail_session_file(tail_ctx);
        });

        // Spawn thread to read stdout. Any stdout at all is a sign of life:
        // an argv rejection (round-13 R1) prints only to stderr.
        let stdout_seen = Arc::new(AtomicBool::new(false));
        if let Some(stdout) = child.stdout.take() {
            let tx_out = tx.clone();
            let ext_flag = extension_feed.clone();
            let seen = stdout_seen.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        seen.store(true, Ordering::SeqCst);
                    }
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

        // Spawn thread to read stderr. Provider billing errors surface here
        // (not the session file), so classify each line for the tripwire.
        let billing_tripped_err = billing_tripped.clone();
        if let Some(stderr) = child.stderr.take() {
            let tx_err = tx.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        if classify_billing_error(trimmed).is_some() {
                            billing_tripped_err.store(true, Ordering::SeqCst);
                        }
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

            // Campaign cap headroom: effective spend includes the compute
            // floor, which grows between calls, so check here rather than
            // only when a session line arrives (round-17 P0).
            if let Some(max_eff) = limits.max_effective_usd {
                let api = accumulated_cost.lock().map(|l| *l).unwrap_or(0.0);
                let fresh = accumulated_tokens.load(Ordering::SeqCst);
                let cached = accumulated_cached.load(Ordering::SeqCst);
                if let Some(msg) = crate::sandbox::omp::cap_headroom_reason(
                    max_eff,
                    api,
                    start_instant.elapsed().as_secs_f64(),
                    fresh + cached,
                    cached,
                ) {
                    log_fn(msg.clone());
                    limit_reached.store(true, Ordering::SeqCst);
                    *limit_reason.lock().unwrap() = Some(msg);
                    kill_agent();
                    let _ = child.kill();
                    break None;
                }
            }

            // Check if model signaled completion early via /workspace/READY
            if take_ready_signal(&canonical_workdir) {
                log_fn("[LIVE] Model signaled completion via /workspace/READY! Triggering verification...".to_string());
                kill_agent();
                let _ = child.kill();
                break None;
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
        if billing_tripped.load(Ordering::SeqCst) {
            // Stats survive the trip: extract before returning so the
            // persisted trial keeps real turns/tokens (backfillable cost)
            // instead of zeros. Pass rates are verifier output, unaffected.
            let mut stats = active_file
                .lock()
                .ok()
                .and_then(|l| l.clone())
                .map(|p| Self::extract_latest_session_stats(Some(&p)).unwrap_or_default())
                .unwrap_or_default();
            stats.cost_usd = accumulated_cost.lock().map(|l| *l).unwrap_or(0.0);
            stats.max_call_usd = max_call_usd.lock().map(|l| *l).unwrap_or(0.0);
            let _ = std::fs::remove_file(canonical_workdir.join("AGENTS.md"));
            return Err(anyhow::Error::new(OmpBillingError {
                reason: "provider billing/auth/throttle tripped mid-session (see [BILLING] log)"
                    .to_string(),
                partial_stats: stats,
            }));
        }
        let _ = std::fs::remove_file(canonical_workdir.join("AGENTS.md"));
        let final_path = active_file.lock().unwrap().clone();
        let mut stats = match final_path.as_deref() {
            Some(p) => Self::extract_latest_session_stats(Some(p)).unwrap_or_default(),
            None => OmpSessionStats::default(),
        };
        stats.cost_usd = accumulated_cost.lock().map(|l| *l).unwrap_or(0.0);
        stats.max_call_usd = max_call_usd.lock().map(|l| *l).unwrap_or(0.0);

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
                    let started = final_path.is_some()
                        || stdout_seen.load(Ordering::SeqCst)
                        || turn_counter.load(Ordering::SeqCst) > 0
                        || stats.steps_taken > 0
                        || stats.total_tokens > 0;
                    return Err(anyhow::Error::new(OmpExecutionError {
                        message: format!("OMP agent exited with non-zero status: {:?}", s.code()),
                        partial_stats: stats,
                        started,
                    }));
                }
            }
        }

        Ok(stats)
    }

    fn collect_all_session_files_in(dirs: &[PathBuf]) -> HashSet<PathBuf> {
        let mut set = HashSet::new();
        for dir in dirs {
            let mut candidates = Vec::new();
            Self::collect_jsonl_files(dir, &mut candidates);
            for (_, p) in candidates {
                set.insert(p);
            }
        }
        set
    }

    /// True when the open session FD no longer tracks the path: the FD
    /// is unlinked (nlink 0 after an atomic rename) or the path now points
    /// at a different inode. Either means the agent rotated the file and
    /// further reads would EOF-spin on a dead inode.
    #[cfg(unix)]
    fn session_disconnected(fd: &File, path: &std::path::Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let fd_meta = match fd.metadata() {
            Ok(m) => m,
            Err(_) => return true,
        };
        if fd_meta.nlink() == 0 {
            return true;
        }
        match std::fs::metadata(path) {
            Ok(disk) => disk.ino() != fd_meta.ino(),
            Err(_) => true,
        }
    }

    fn tail_session_file(ctx: TailSessionContext) {
        // Only this run's own subdirectory. Scanning the shared parent lets a
        // concurrent run's session be adopted as this run's own.
        let dirs = vec![ctx.session_dir.clone()];

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
        let mut lines_seen: u64 = 0;

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
                    // OMP rotates the session file atomically (temp + rename):
                    // our already-open FD then points at an unlinked inode and
                    // reads EOF forever — turn counting stalls and max_turns
                    // never fires (measured: 167 turns vs a 100 cap). Reopen
                    // only on proven disconnect (nlink 0 or inode changed);
                    // transient emptiness keeps the old fast path.
                    #[cfg(unix)]
                    {
                        if Self::session_disconnected(reader.get_ref(), &session_path) {
                            if let Ok(new_file) = File::open(&session_path) {
                                let mut new_reader = BufReader::new(new_file);
                                let mut skipped: u64 = 0;
                                let mut skip_buf = String::new();
                                // Skip already-counted lines so a copy-rotate
                                // never double-counts turns (over-counting is
                                // still safer than a bypassed limit, and a
                                // compacted file simply yields fewer lines).
                                while skipped < lines_seen {
                                    skip_buf.clear();
                                    match new_reader.read_line(&mut skip_buf) {
                                        Ok(0) => break,
                                        Ok(_) => skipped += 1,
                                        Err(_) => break,
                                    }
                                }
                                reader = new_reader;
                                let _ = ctx.tx.send(format!(
                                    "[TAIL] session file rotated; resumed at line {}",
                                    skipped
                                ));
                                continue;
                            }
                        }
                    }
                    let cur_turns = ctx.turn_counter.load(Ordering::SeqCst);
                    let max_t = ctx.limits.max_turns.unwrap_or(0);
                    let elapsed = last_activity.elapsed().as_secs();
                    if elapsed >= 3 && last_heartbeat.elapsed() >= Duration::from_secs(3) {
                        last_heartbeat = std::time::Instant::now();
                        let max_str = if max_t > 0 {
                            max_t.to_string()
                        } else {
                            "∞".to_string()
                        };
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
                    lines_seen += 1;
                    last_activity = std::time::Instant::now();
                    if line_buf.ends_with('\n') {
                        // Billing tripwire: provider 402/401/429 must pause the
                        // wave, never score a 0% model failure.
                        if let Some(reason) = crate::sandbox::omp::classify_billing_error(&line_buf)
                        {
                            ctx.billing_tripped.store(true, Ordering::SeqCst);
                            let _ = ctx.tx.send(format!("[BILLING] {reason}; pausing queue."));
                        }
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
                            // 2. Budget check: explicit per-event cost wins; when
                            // `usage.cost.total` is absent, estimate from token counts x
                            // per-model rates so max_budget_usd still trips mid-session.
                            let mut cost_updated = false;
                            let usage_node =
                                val.pointer("/message/usage").or_else(|| val.get("usage"));
                            let delta_cost: Option<f64> = val
                                .pointer("/message/usage/cost/total")
                                .and_then(|c| c.as_f64())
                                .or_else(|| {
                                    let usage = usage_node?;
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
                                        .or_else(|| {
                                            usage
                                                .get("prompt_tokens_details")
                                                .and_then(|d| d.get("cached_tokens"))
                                                .and_then(|x| x.as_u64())
                                        })
                                        .unwrap_or(0);
                                    if tin + tout + tcached == 0 {
                                        None
                                    } else {
                                        let pricing =
                                            crate::cost::ModelPricing::for_model(&ctx.model);
                                        Some(
                                            pricing
                                                .compute_cost_with_cache(tin, tcached, tout)
                                                .total_cost_usd,
                                        )
                                    }
                                });
                            if let Some(cost) = delta_cost {
                                if let Ok(mut c_lock) = ctx.accumulated_cost.lock() {
                                    *c_lock += cost;
                                    cost_updated = true;
                                    if let Ok(mut m) = ctx.max_call_usd.lock() {
                                        *m = m.max(cost);
                                    }
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
                            if let Some(usage) =
                                val.pointer("/message/usage").or_else(|| val.get("usage"))
                            {
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
                                    .or_else(|| {
                                        usage
                                            .get("prompt_tokens_details")
                                            .and_then(|d| d.get("cached_tokens"))
                                            .and_then(|x| x.as_u64())
                                    })
                                    .unwrap_or(0);
                                if tin + tout > 0 {
                                    ctx.accumulated_tokens
                                        .fetch_add(tin + tout, Ordering::SeqCst);
                                }
                                if tcached > 0 {
                                    ctx.accumulated_cached.fetch_add(tcached, Ordering::SeqCst);
                                }
                            }

                            if assistant_turn || cost_updated {
                                let cur_turns = ctx.turn_counter.load(Ordering::SeqCst);
                                let max_t = ctx.limits.max_turns.unwrap_or(0);
                                let cur_spend =
                                    ctx.accumulated_cost.lock().map(|l| *l).unwrap_or(0.0);
                                let cur_tokens = ctx.accumulated_tokens.load(Ordering::SeqCst);

                                let _ = ctx.tx.send(format!(
                                    "[TELEMETRY] turn={} max_turns={} spent={:.6} tokens={}",
                                    cur_turns, max_t, cur_spend, cur_tokens
                                ));
                                let _ = ctx.tx.send(format!(
                                    "[LIVE] Turn {}/{} | Spent: ${:.4} | Tokens: {}",
                                    cur_turns,
                                    if max_t > 0 {
                                        max_t.to_string()
                                    } else {
                                        "∞".to_string()
                                    },
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
                for line in v
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .lines()
                {
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
                for line in v
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .lines()
                {
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
                out.push(format!(
                    "[RESULT] {} ({})",
                    tool,
                    if ok { "OK" } else { "FAIL" }
                ));
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
                if v.get("customType").and_then(|c| c.as_str()) == Some("tool_execution_start") {
                    return Vec::new();
                }
                let role = v
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str());
                if matches!(role, Some("assistant") | Some("toolResult") | Some("tool")) {
                    return Self::extract_token_line(&v)
                        .map(|t| vec![t])
                        .unwrap_or_default();
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
                                } else if let Some(old_s) = args
                                    .get("oldText")
                                    .or_else(|| args.get("old_str"))
                                    .and_then(|s| s.as_str())
                                {
                                    results.push(format!(
                                        "  - {}",
                                        old_s.lines().next().unwrap_or("")
                                    ));
                                    if let Some(new_s) = args
                                        .get("newText")
                                        .or_else(|| args.get("new_str"))
                                        .and_then(|s| s.as_str())
                                    {
                                        results.push(format!(
                                            "  + {}",
                                            new_s.lines().next().unwrap_or("")
                                        ));
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
                                results.push(format!("  | ... (+{} lines)", lines_vec.len() - 20));
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
        // Session-level thinking negotiation: the CLI records the EFFECTIVE
        // level here (it downgrades unsupported requested levels itself —
        // measured: --thinking=max → thinkingLevel "high", no error). This
        // line, next to the [OMP-PROMPT] requested level, makes any
        // downgrade visible instead of silent.
        else if v.get("type").and_then(|t| t.as_str()) == Some("thinking_level_change") {
            if let Some(lvl) = v.get("thinkingLevel").and_then(|l| l.as_str()) {
                results.push(format!("[THINKING] session thinking level: {}", lvl));
            }
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
        let mut cost_usd = 0.0f64;
        let mut max_call_usd = 0.0f64;
        // Last negotiated thinking level wins: the session records the
        // EFFECTIVE level (CLI downgrades included), which is what ran.
        let mut thinking_effective: Option<String> = None;

        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("thinking_level_change") {
                    if let Some(lvl) = v.get("thinkingLevel").and_then(|l| l.as_str()) {
                        thinking_effective = Some(lvl.to_string());
                    }
                }
                let is_assistant =
                    v.pointer("/message/role").and_then(|r| r.as_str()) == Some("assistant");
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
                    if let Some(c) = usage
                        .get("cost")
                        .and_then(|c| c.get("total"))
                        .and_then(|x| x.as_f64())
                    {
                        cost_usd += c;
                        max_call_usd = max_call_usd.max(c);
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
            cost_usd,
            max_call_usd,
            effort_effective: thinking_effective,
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

impl OmpRunner {
    /// Per-task port, smoke-test command, and anti-cheat guidance snippet.
    /// Shared by the OMP and OpenCode runners so both harnesses grade the
    /// same task text and entrypoint contract.
    /// AGENTS.md body (pure composer: byte-identical content, templated
    /// by the caller per worker). Split for unit tests.
    fn compose_omp_agents_md(snippet: &str, port: u16, test: &str) -> String {
        format!(
            r#"# Instructions for Autonomous Coding Agent
1. **TOOL CALL REQUIREMENT**: You have tools (`write`, `bash`, `edit`, `read`). You MUST use the `write` tool to create every file directly in /workspace. NEVER output code in conversational text without calling `write`.
2. **RAW TCP SOCKETS ONLY (CRITICAL)**:
{}
3. **ENTRYPOINT REQUIRED**: You must create a working `Dockerfile` or executable `./start.sh` listening on port {}. Test your server with `{}` using the `bash` tool before finishing. Name your binary neutrally (`server`, `sdb-redis`): NEVER `redis-server`, `nginx`, `valkey`, etc. — the anti-cheat scanner disqualifies those names even for binaries you compiled yourself.
4. **CONCISE REASONING**: Keep internal thoughts brief (< 150 words). Do not exhaust output tokens on lengthy mental planning loops. Immediately invoke tools to write code and test it.
"#,
            snippet, port, test
        )
    }

    pub fn task_guidance(
        task: Option<crate::config::TaskType>,
        prompt: &str,
    ) -> (u16, &'static str, &'static str) {
        match task {
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
                "curl http://upstream1/ ; curl -v http://localhost:8091/",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Proxy frameworks/gateways (nginx, envoy, traefik libs) are FORBIDDEN; raw TCP sockets only.
- Build an HTTP reverse proxy on 8091 over upstreams upstream1:80, upstream2:80, upstream3:80: round-robin, upstream keep-alive pooling, X-Forwarded-For injection, failover with zero dropped requests."#,
            ),
            Some(crate::config::TaskType::Pubsub) => (
                8092,
                "curl -v http://localhost:8092/",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- WebSocket libraries (ws, gorilla, tungstenite) are FORBIDDEN; raw TCP sockets only.
- Build an RFC 6455 broker on 8092: SHA-1 handshake, masked TEXT frames SUB <channel> / PUB <channel> <msg>, broadcast fan-out, ping/pong."#,
            ),
            Some(crate::config::TaskType::Queue) => (
                6385,
                "printf 'ENQ q hello\nLEASE q 5000\n' | nc 127.0.0.1 6385",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Pre-built queue systems and job libraries (RabbitMQ, Redis, Kafka, NATS, nsq, celery, bullmq, sidekiq, RQ, etc.) are FORBIDDEN.
- Build a job queue broker on TCP 6385 with LF-terminated commands: ENQ <queue> <payload> -> +OK <id> (fsync before ack), LEASE <queue> <lease_ms> -> +JOB <id> <attempts> <payload>|+NONE (visibility lease owned by the connection), ACK <id>/NACK <id> [delay_ms] -> +OK, 3 failed attempts -> <queue>.dead, REDRIVE <queue> <id>, +STATS counters, PING -> +PONG. Persist under ./data and recover after kill -9."#,
            ),
            Some(crate::config::TaskType::Sql) => (
                5480,
                "printf 'SELECT 1\n' | nc 127.0.0.1 5480",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- Embedded database engines and SQL parser libraries are STRICTLY FORBIDDEN: no SQLite (Python `sqlite3`, `node:sqlite`, `bun:sqlite`, Go/Rust drivers), no duckdb, no `sqlite3`/`psql` binaries, no vendored engine sources. This includes test and helper files: the scanner checks every file.
- Build the SQL server from raw TCP sockets on port 5480: one statement per line, `OK <n>` / `ROWS <ncols>` frames / `ERR <class> <text>` answers, tables strictly typed.
- Launch: `./start.sh --port <p> --data <dir>` must work (defaults: 5480, ./data). A Dockerfile gets those arguments only with `ENTRYPOINT`; a `CMD`-only image must read `SDB_PORT`/`SDB_DATA_DIR` or use the defaults.
- Ground truth: a reference server speaking the same protocol runs at host `ref-sql`, port 5480 (`printf 'SELECT 1\n' | nc ref-sql 5480`). Compare against it while developing; it is not reachable while you are graded.
- `SPEC.md` in the workspace holds the full contract so far (harness-owned, rewritten every ticket)."#,
            ),
            Some(crate::config::TaskType::Binproto) => (
                6395,
                "python3 -c \"import struct,zlib;print(zlib.crc32(b'x'))\"",
                r#"ANTI-CHEAT REQUIREMENT (STRICT):
- RPC frameworks (gRPC, Thrift, JSON-RPC libs) are FORBIDDEN; raw TCP sockets only.
- Build an SDB1 binary RPC server on 6395: 16-byte BE header (magic SDB1, u16 seq, u8 opcode, u8 status-reserved-0, u32 len, u32 CRC32), opcodes PING/ECHO/PUT/GET/STREAM, CRC-mismatch -> ERR_BAD_CHECKSUM open, unknown op -> ERR_UNKNOWN_OP open, bad magic -> close."#,
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
                } else {
                    (
                        8080,
                        "curl -v http://localhost:8080/",
                        r#"ANTI-CHEAT REQUIREMENT (STRICT): Build using raw TCP sockets on port 8080."#,
                    )
                }
            }
        }
    }
}

/// `AgentRunner` dispatch: the pipeline calls runners through this trait so
/// OMP and OpenCode stay interchangeable. Delegates to the inherent method
/// (inherent resolution takes precedence, no recursion).
impl crate::sandbox::AgentRunner for OmpRunner {
    fn run_agent_with_logger<F>(spec: AgentRunSpec<'_>, log_fn: F) -> Result<OmpSessionStats>
    where
        F: FnMut(String) + Send + 'static,
    {
        Self::run_agent_with_logger(spec, log_fn)
    }
}

#[cfg(test)]
mod session_disconnect_tests {
    use super::OmpRunner;
    use std::fs::File;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sdb_disc_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_connected_file_is_not_disconnected() {
        let p = tmp("live");
        std::fs::write(&p, "line\n").unwrap();
        let fd = File::open(&p).unwrap();
        assert!(!OmpRunner::session_disconnected(&fd, &p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_atomic_rename_registers_disconnect() {
        // OMP's rotate pattern: write temp, rename over the live path.
        let live = tmp("rot_live");
        let next = tmp("rot_next");
        std::fs::write(&live, "old\n").unwrap();
        let fd = File::open(&live).unwrap();
        std::fs::write(&next, "old\nnew\n").unwrap();
        std::fs::rename(&next, &live).unwrap();
        assert!(OmpRunner::session_disconnected(&fd, &live));
        let _ = std::fs::remove_file(&live);
    }

    #[test]
    fn test_unlinked_path_registers_disconnect() {
        let p = tmp("gone");
        std::fs::write(&p, "x\n").unwrap();
        let fd = File::open(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert!(OmpRunner::session_disconnected(&fd, &p));
    }
}
#[cfg(test)]
mod tests {
    /// Round-13 R1: the prompt is the last argv element, right after `--`.
    #[test]
    fn test_omp_task_args_keep_a_dash_leading_prompt_positional() {
        let prompt = "--- harness note (added by the benchmark, not the ticket) ---\nbody";
        let args = omp_task_args(
            prompt,
            "openrouter/x/y",
            "/tmp/sessions/c1",
            None,
            Some(600),
            "high",
        );
        assert_eq!(args.last().map(String::as_str), Some(prompt));
        assert_eq!(
            args[args.len() - 2],
            "--",
            "the separator precedes the prompt"
        );
        assert_eq!(
            args.iter().filter(|a| a.as_str() == prompt).count(),
            1,
            "the prompt is one value: {args:?}"
        );
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert!(
            args[..sep].iter().all(|a| a.starts_with('-')),
            "everything before `--` is an option: {args:?}"
        );
    }

    use super::*;

    #[test]
    fn test_take_ready_signal_fires_once_then_consumed() {
        let dir = std::env::temp_dir().join(format!("ready_sig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Absent (including wholly missing dir): silent no.
        assert!(!take_ready_signal(&dir));
        assert!(!take_ready_signal(&dir.join("nope")));
        std::fs::write(dir.join("READY"), "done").unwrap();
        assert!(take_ready_signal(&dir));
        // Consumed: file gone, second poll silent.
        assert!(!dir.join("READY").exists());
        assert!(!take_ready_signal(&dir));
        // A directory named READY is not a signal (touch creates files;
        // a dir is pathological, and the old exists()-check would have
        // killed the agent loop over it).
        std::fs::create_dir_all(dir.join("READY")).unwrap();
        assert!(!take_ready_signal(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No stale canonical ports outside alias contexts (the proxy 8091-vs-worker
    /// contradiction, measured live). Shared by both AGENTS.md render tests.
    fn assert_agents_ports_clean(content: &str, base: u16, task: &str, worker: u8) {
        let base_pat =
            ::regex::Regex::new(&format!(r"\b{}\b", ::regex::escape(&base.to_string()))).unwrap();
        let alias_pat =
            ::regex::Regex::new(r"(?i)(ref-redis|ref-http|ref-sql|upstream1|upstream2|upstream3)")
                .unwrap();
        for line in content.lines() {
            if base_pat.is_match(line) && !alias_pat.is_match(line) {
                panic!(
                    "stale canonical port {base} on non-alias line (task {task}, worker {worker}): {line}"
                );
            }
        }
    }

    fn all_live_tasks() -> Vec<crate::config::TaskType> {
        use crate::config::TaskType;
        vec![
            TaskType::Redis,
            TaskType::Http,
            TaskType::Pdf,
            TaskType::Proxy,
            TaskType::Pubsub,
            TaskType::Wal,
            TaskType::Binproto,
        ]
    }

    #[test]
    fn test_omp_agents_md_renders_worker_ports() {
        // Same audit on the OMP composer (pure fn, no disk): every task,
        // every worker. Both runners must agree with the brief.
        for task in all_live_tasks() {
            let base = task.default_port();
            let (port, test_cmd, snippet) = OmpRunner::task_guidance(Some(task), "");
            assert_eq!(port, base);
            for worker in [1u8, 2, 3] {
                let content = OmpRunner::compose_omp_agents_md(snippet, port, test_cmd);
                let content = crate::config::template_ports_for_worker(&content, port, worker);
                let off = crate::web::server::worker_port(base, worker).to_string();
                assert!(
                    content.contains(&off),
                    "worker port {off} missing (task {task}, worker {worker})"
                );
                assert_agents_ports_clean(&content, base, &task.to_string(), worker);
            }
        }
    }

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
        assert!(
            OmpRunner::convert_bench_event("[BENCH-EVENT] {\"v\":1,\"kind\":\"nope\"}").is_none()
        );
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
        // Max passes through verbatim for every provider: the omp CLI
        // downgrades unsupported levels itself (measured: --thinking=max on
        // ling records thinkingLevel "high", exit 0, no HTTP 400), and the
        // session's thinking_level_change event reports what actually ran.
        let (lvl, note) = resolve_thinking_level(Some("max"), "openai/gpt-5.2");
        assert_eq!(lvl, "max");
        assert!(note.is_empty());
        let (lvl, note) = resolve_thinking_level(Some("max"), "openrouter/meta/m");
        assert_eq!(lvl, "max");
        assert!(note.is_empty());
    }

    #[test]
    fn test_forward_thinking_level_change_reports_effective_level() {
        // The CLI's downgrade must be visible: requested level is logged at
        // launch ([OMP-PROMPT]), effective level arrives here ([THINKING]).
        let out = OmpRunner::format_session_line(
            r#"{"type":"thinking_level_change","id":"b185e3e7","thinkingLevel":"high","configured":null}"#,
        )
        .expect("thinking_level_change must render");
        assert!(
            out.iter()
                .any(|l| l.contains("[THINKING]") && l.contains("high")),
            "unexpected: {out:?}"
        );
        // Missing level degrades to silence, never a panic.
        assert!(OmpRunner::format_session_line(r#"{"type":"thinking_level_change"}"#).is_none());
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
        let dir = std::env::temp_dir().join(format!("test_bench_policy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let got = install_bench_policy_extension(&dir);
        assert_eq!(got.as_deref(), Some(BENCH_POLICY_CONTAINER_PATH));
        let content = std::fs::read_to_string(dir.join("bench-policy.js")).unwrap();
        assert!(content.contains("tool_call"), "must hook tool calls");
        assert!(
            content.contains("[BENCH-POLICY]"),
            "must emit tagged telemetry"
        );

        // Disabled gate installs nothing.
        std::env::set_var("SDB_BENCH_POLICY", "0");
        assert!(install_bench_policy_extension(&dir).is_none());

        // Write failure degrades to None, never a panic (bench must not fail here).
        std::env::remove_var("SDB_BENCH_POLICY");
        assert!(
            install_bench_policy_extension(Path::new("/nonexistent-covtest-dir")).is_none(),
            "unwritable agent dir must degrade to no-policy"
        );

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
        assert!(res
            .iter()
            .any(|s| s.contains("[PROMPT] Task Instructions & Directives:")));
        assert!(res
            .iter()
            .any(|s| s.contains("| Build a Redis server on port 6379")));
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
        })
        .to_string();
        let res = OmpRunner::format_session_line(&json_assistant).unwrap();
        assert!(res
            .iter()
            .any(|s| s.contains("[THOUGHT] Writing the main logic")));
        assert!(res
            .iter()
            .any(|s| s.contains("[ACTION] write: Create main.rs")));
        assert!(res.iter().any(|s| s.contains("path: main.rs")));
        assert!(res.iter().any(|s| s.contains("| fn main() {")));
        assert!(res.iter().any(|s| s.contains("println!(\"hello\");")));
    }

    #[test]
    fn test_format_session_line_system_interrupt() {
        let json_interrupt = r#"{"customType": "thinking-loop-redirect"}"#;
        let res = OmpRunner::format_session_line(json_interrupt).unwrap();
        assert!(res
            .iter()
            .any(|s| s.contains("[SYSTEM INTERRUPT] Loop guard triggered")));
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
        assert_eq!(stats.effort_effective, None);
        assert!(OmpRunner::extract_latest_session_stats(None).is_err());

        let found = OmpRunner::find_newest_session_file_across(std::slice::from_ref(&temp_dir));
        assert_eq!(found, Some(session_file));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_extract_session_stats_reads_negotiated_thinking_level() {
        // The session's thinking_level_change is the source of truth for
        // what ran (CLI downgrades included); last event wins.
        let temp_dir = std::env::temp_dir().join(format!("test_omp_think_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let session_file = temp_dir.join("s.jsonl");
        let content = "{\"type\":\"thinking_level_change\",\"thinkingLevel\":\"high\"}\n{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":10,\"output\":5}}}\n{\"type\":\"thinking_level_change\",\"thinkingLevel\":\"max\"}\n";
        std::fs::write(&session_file, content).unwrap();
        let stats = OmpRunner::extract_latest_session_stats(Some(&session_file)).unwrap();
        assert_eq!(stats.effort_effective.as_deref(), Some("max"));
        assert_eq!(stats.steps_taken, 1);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_agent_execution_limits_defaults() {
        let limits = AgentExecutionLimits {
            max_turns: Some(10),
            max_budget_usd: Some(0.20),
            timeout_seconds: Some(600),
            max_effective_usd: None,
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
            max_effective_usd: None,
        };

        let running_tailer = running.clone();
        let tail_ctx = TailSessionContext {
            session_dir: temp_dir.clone(),
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
            model: "test-model".to_string(),
            billing_tripped: Arc::new(AtomicBool::new(false)),
            max_call_usd: Arc::new(Mutex::new(0.0f64)),
            accumulated_cached: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

    /// Spawns a tailer with `active_file = None` so it must DISCOVER the
    /// session file by scanning `session_dir` - the path production takes and
    /// the one this fix changed. Every other tailer test pre-sets `active_file`
    /// and so returns early without ever scanning.
    fn tailer_ctx(
        session_dir: PathBuf,
        tx: mpsc::Sender<String>,
        running: Arc<AtomicBool>,
    ) -> (TailSessionContext, Arc<Mutex<Option<PathBuf>>>) {
        // The active_file Arc is handed back so a test can assert on what the
        // tailer actually adopted, instead of inferring it from whether a line
        // arrived before a timeout - which proves absence only if the machine
        // is fast enough for a failure to have shown up.
        let active_file = Arc::new(Mutex::new(None));
        (
            TailSessionContext {
                session_dir,
                start_time: SystemTime::now(),
                existing: HashSet::new(),
                active_file: active_file.clone(),
                running,
                tx,
                limits: AgentExecutionLimits {
                    max_turns: Some(5),
                    max_budget_usd: Some(0.10),
                    timeout_seconds: Some(10),
                    max_effective_usd: None,
                },
                turn_counter: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                accumulated_cost: Arc::new(Mutex::new(0.0f64)),
                accumulated_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                limit_reached: Arc::new(AtomicBool::new(false)),
                limit_reason: Arc::new(Mutex::new(None)),
                cancel_flag: None,
                extension_feed_active: Arc::new(AtomicBool::new(false)),
                model: "test-model".to_string(),
                billing_tripped: Arc::new(AtomicBool::new(false)),
                max_call_usd: Arc::new(Mutex::new(0.0f64)),
                accumulated_cached: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
            active_file,
        )
    }

    #[test]
    fn test_tailer_discovers_session_file_in_own_dir() {
        let base = std::env::temp_dir().join(format!("test_tail_disc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let session_dir = base.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (tail_ctx, active_file) = tailer_ctx(session_dir.clone(), tx, running.clone());
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(tail_ctx));

        // No sleep. The discovery loop polls; the file simply has to be absent
        // at the first scan and present by a later one. A sleep before the
        // write only widens the window in which a slow box loses the race
        // against the poll interval.
        let line = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"init","args":{"command":"echo discovered"}}}"#;
        std::fs::write(session_dir.join("live.jsonl"), format!("{}\n", line)).unwrap();

        let got = rx.recv_timeout(Duration::from_secs(5));
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();

        assert!(
            got.is_ok(),
            "tailer must discover a session file written into its own dir"
        );
        let adopted = active_file.lock().unwrap().clone();
        assert_eq!(
            adopted.as_deref(),
            Some(session_dir.join("live.jsonl").as_path()),
            "the adopted path must be this run's own file"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A concurrent run's session file must never be adopted. This is what let
    /// glm record muse's usage: both wrote into one shared directory and the
    /// tailer took whichever file appeared newest.
    ///
    /// Asserts on the adopted path directly rather than on a receive timeout -
    /// "nothing arrived in 1200ms" is not proof of absence on a loaded box.
    #[test]
    fn test_tailer_ignores_session_file_in_another_runs_dir() {
        let base = std::env::temp_dir().join(format!("test_tail_iso_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mine = base.join("run-a/sessions");
        let theirs = base.join("run-b/sessions");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (tail_ctx, active_file) = tailer_ctx(mine.clone(), tx, running.clone());
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(tail_ctx));

        let line = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"init","args":{"command":"echo other-run"}}}"#;
        std::fs::write(theirs.join("theirs.jsonl"), format!("{}\n", line)).unwrap();

        // Let several poll intervals elapse so a scan certainly happens.
        std::thread::sleep(Duration::from_millis(900));
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();

        assert!(
            active_file.lock().unwrap().is_none(),
            "a session file in another run's directory must not be adopted"
        );
        let _ = std::fs::remove_dir_all(&base);
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
        assert!(plain
            .iter()
            .any(|l| l.contains("[OMP] just thinking out loud")));
        assert!(OmpRunner::format_session_line(r#"{"unknown": "object"}"#).is_none());
    }

    #[test]
    fn test_collect_session_files_in_scoped_dir() {
        let dir = std::env::temp_dir().join(format!("test_collect_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jsonl"), "{}\n").unwrap();
        let found = OmpRunner::collect_all_session_files_in(std::slice::from_ref(&dir));
        assert_eq!(found.len(), 1, "the scoped dir's own session must be found");
        assert!(found.iter().all(|p| p.starts_with(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
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
            max_effective_usd: None,
        };

        let running_tailer = running.clone();
        let turn_c = turn_counter.clone();
        let lim_reached = limit_reached.clone();
        let tail_ctx = TailSessionContext {
            session_dir: temp_dir.clone(),
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
            model: "test-model".to_string(),
            billing_tripped: Arc::new(AtomicBool::new(false)),
            max_call_usd: Arc::new(Mutex::new(0.0f64)),
            accumulated_cached: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
            max_effective_usd: None,
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
            worker: 0,
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
            max_effective_usd: None,
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
            worker: 0,
        });

        if let Err(e) = res {
            let err_str = e.to_string();
            assert!(!err_str.contains("invalid characters for a local volume name"));
        }

        let _ = std::fs::remove_dir_all(rel_dir);
    }

    #[test]
    fn test_tail_budget_derived_from_tokens_without_cost_field() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_tail_derived_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // No `cost.total` — only raw token counts.
        let line = r#"{"message":{"role":"assistant","usage":{"input":1000000,"output":1000000},"content":[{"type":"text","text":"hi"}]}}"#;
        let session_file = temp_dir.join("session.jsonl");
        std::fs::write(&session_file, format!("{}\n", line)).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let active_file = Arc::new(Mutex::new(Some(session_file.clone())));
        let accumulated_cost = Arc::new(Mutex::new(0.0f64));
        let limit_reached = Arc::new(AtomicBool::new(false));
        let limits = AgentExecutionLimits {
            max_turns: Some(100),
            max_budget_usd: Some(0.20),
            timeout_seconds: Some(10),
            max_effective_usd: None,
        };
        let tail_ctx = TailSessionContext {
            session_dir: temp_dir.clone(),
            start_time: SystemTime::now() - Duration::from_secs(5),
            existing: HashSet::new(),
            active_file,
            running: running.clone(),
            tx,
            limits,
            turn_counter: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            accumulated_cost: accumulated_cost.clone(),
            accumulated_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            limit_reached: limit_reached.clone(),
            limit_reason: Arc::new(Mutex::new(None)),
            cancel_flag: None,
            extension_feed_active: Arc::new(AtomicBool::new(false)),
            model: "google/gemini-2.5-flash".to_string(),
            billing_tripped: Arc::new(AtomicBool::new(false)),
            max_call_usd: Arc::new(Mutex::new(0.0f64)),
            accumulated_cached: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let handle = std::thread::spawn(move || {
            OmpRunner::tail_session_file(tail_ctx);
        });

        std::thread::sleep(Duration::from_millis(500));
        // 1M in @ $0.15/M + 1M out @ $0.60/M = $0.75 >= $0.20 cap.
        assert!(limit_reached.load(Ordering::SeqCst));
        assert!(*accumulated_cost.lock().unwrap() >= 0.20);

        running.store(false, Ordering::SeqCst);
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------
    // Unit coverage for the deterministic helpers
    // -----------------------------------------------------------------

    #[test]
    fn test_agent_execution_limits_default_matches_documented_values() {
        let d = AgentExecutionLimits::default();
        assert_eq!(d.max_turns, Some(100));
        assert_eq!(d.max_budget_usd, Some(0.50));
        assert_eq!(d.timeout_seconds, Some(900));
    }

    #[test]
    fn test_omp_billing_error_display_prefixes_billing() {
        let e = OmpBillingError {
            reason: "provider rate-limited (HTTP 429)".to_string(),
            partial_stats: OmpSessionStats::default(),
        };
        assert_eq!(e.to_string(), "billing: provider rate-limited (HTTP 429)");
        let boxed: Box<dyn std::error::Error> = Box::new(e);
        assert!(boxed.to_string().starts_with("billing:"));
    }

    #[test]
    fn test_classify_billing_error_structured_variants() {
        use crate::sandbox::omp::classify_billing_error as c;
        assert!(c(r#"{"error":{"code":429}}"#).unwrap().contains("429"));
        assert!(c(r#"{"error":"payment_required"}"#)
            .unwrap()
            .contains("402"));
        assert!(c(r#"{"error":"insufficient_credit"}"#)
            .unwrap()
            .contains("402"));
        assert!(c(r#"{"message":"payment required"}"#)
            .unwrap()
            .contains("402"));
        assert!(c(r#"{"message":"credit exhausted"}"#)
            .unwrap()
            .contains("402"));
        assert!(c(r#"{"message":"out of credit"}"#).unwrap().contains("402"));
        assert!(c(r#"{"message":"rate limit exceeded"}"#)
            .unwrap()
            .contains("429"));
        assert!(c(r#"{"message":"too many requests"}"#)
            .unwrap()
            .contains("429"));
        assert!(c(r#"{"message":"unauthorized api key"}"#)
            .unwrap()
            .contains("401"));
        assert!(c(r#"{"message":"unauthorized token"}"#)
            .unwrap()
            .contains("401"));
        assert!(c("status: 401 bad token").unwrap().contains("401"));
        assert!(c("unauthorized token supplied").unwrap().contains("401"));
        assert!(c("status 402").unwrap().contains("402"));
        assert!(c("HTTP/2 429").unwrap().contains("429"));
        // Messages that merely mention numbers stay untouched.
        assert!(c(r#"{"message":"done, 401 tests passed"}"#).is_none());
        assert!(c(r#"{"message":""}"#).is_none());
    }

    #[test]
    fn test_extract_token_line_variants() {
        use OmpRunner as R;
        let v: serde_json::Value =
            serde_json::from_str(r#"{"message":{"usage":{"input":3,"output":4,"cacheRead":5}}}"#)
                .unwrap();
        assert_eq!(
            R::extract_token_line(&v).unwrap(),
            "[TOKENS] Turn usage - input: 3, output: 4, cached: 5"
        );
        // Alternate provider spellings.
        let v2: serde_json::Value = serde_json::from_str(
            r#"{"message":{"usage":{"prompt_tokens":7,"completion_tokens":8,"cache_read_input_tokens":9}}}"#,
        )
        .unwrap();
        assert_eq!(
            R::extract_token_line(&v2).unwrap(),
            "[TOKENS] Turn usage - input: 7, output: 8, cached: 9"
        );
        // All-zero / missing usage renders nothing.
        let v3: serde_json::Value = serde_json::from_str(r#"{"message":{"usage":{}}}"#).unwrap();
        assert!(R::extract_token_line(&v3).is_none());
        let v4: serde_json::Value = serde_json::from_str(r#"{"message":{}}"#).unwrap();
        assert!(R::extract_token_line(&v4).is_none());
    }

    #[test]
    fn test_cap_line_counts_chars_and_truncates() {
        assert_eq!(OmpRunner::cap_line("short"), "short");
        let exactly = "x".repeat(2000);
        assert_eq!(OmpRunner::cap_line(&exactly).chars().count(), 2000);
        let long = "y".repeat(2100);
        let capped = OmpRunner::cap_line(&long);
        assert!(capped.starts_with(&"y".repeat(2000)));
        assert!(capped.ends_with("... (+100 chars)"));
        // Multi-byte input never panics and truncates by char count.
        let cjk = "你".repeat(2100);
        let capped_cjk = OmpRunner::cap_line(&cjk);
        assert!(capped_cjk.ends_with("... (+100 chars)"));
    }

    #[test]
    fn test_collect_jsonl_files_walks_nested_dirs() {
        let dir = std::env::temp_dir().join(format!("sdb_collect_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested/deeper")).unwrap();
        std::fs::write(dir.join("a.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("nested/b.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("nested/deeper/c.txt"), "not jsonl").unwrap();
        let mut out = Vec::new();
        OmpRunner::collect_jsonl_files(&dir, &mut out);
        assert_eq!(out.len(), 2, "nested jsonl files must be found");
        assert!(out.iter().all(|(_, p)| p.extension().unwrap() == "jsonl"));
        // Missing dir is silently empty.
        let mut none = Vec::new();
        OmpRunner::collect_jsonl_files(&dir.join("nope"), &mut none);
        assert!(none.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_renamed_path_registers_disconnect() {
        // Rename keeps the FD's link count at 1 while the recorded path
        // points nowhere: still a disconnect.
        let p = std::env::temp_dir().join(format!("sdb_disc_renamed_{}", std::process::id()));
        let other =
            std::env::temp_dir().join(format!("sdb_disc_renamed_other_{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&other);
        std::fs::write(&p, "x\n").unwrap();
        let fd = File::open(&p).unwrap();
        std::fs::rename(&p, &other).unwrap();
        assert!(OmpRunner::session_disconnected(&fd, &p));
        let _ = std::fs::remove_file(&other);
    }

    #[test]
    fn test_convert_bench_event_rejects_other_versions_and_truncates() {
        assert!(OmpRunner::convert_bench_event(
            r#"[BENCH-EVENT] {"v":2,"kind":"message","role":"assistant","text":"x"}"#
        )
        .is_none());
        // >35 code lines in a tool call are truncated.
        let code = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let line = format!(
            r#"[BENCH-EVENT] {{"v":1,"kind":"tool_call","tool":"write","intent":"","command":"","path":"","code":"{}"}}"#,
            code
        );
        let out = OmpRunner::convert_bench_event(&line).unwrap();
        assert!(out.iter().any(|l| l.contains("(+5 lines omitted)")));
        assert!(!out.iter().any(|l| l.contains("line 39")));
        // Empty tool output and >40 output lines.
        let ev =
            r#"[BENCH-EVENT] {"v":1,"kind":"tool_result","tool":"bash","ok":true,"output":""}"#;
        let out = OmpRunner::convert_bench_event(ev).unwrap();
        assert!(out.iter().any(|l| l.contains("(empty output)")));
        let long_out = (0..45)
            .map(|i| format!("o{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let ev = format!(
            r#"[BENCH-EVENT] {{"v":1,"kind":"tool_result","tool":"bash","ok":true,"output":"{}"}}"#,
            long_out
        );
        let out = OmpRunner::convert_bench_event(&ev).unwrap();
        assert!(out.iter().any(|l| l.contains("(+5 more lines)")));
    }

    #[test]
    fn test_format_session_line_user_content_string() {
        let line = r#"{"message":{"role":"user","content":"plain text brief"}}"#;
        let out = OmpRunner::format_session_line(line).unwrap();
        assert!(out.iter().any(|l| l.contains("[PROMPT]")));
        assert!(out.iter().any(|l| l.contains("plain text brief")));
    }

    #[test]
    fn test_format_session_line_tool_variants_and_edits() {
        // tool_use item type + stringified arguments + multi-line command.
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "name": "bash", "arguments": "{\"command\":\"cargo build\\ncargo test\"}"}
            ]}
        })
        .to_string();
        let out = OmpRunner::format_session_line(&line).unwrap();
        assert!(out.iter().any(|l| l == "[ACTION] bash"));
        assert!(out.iter().any(|l| l.contains("$ cargo build")));
        assert!(out.iter().any(|l| l.contains("$ cargo test")));

        // function_call item type with toolName and empty intent.
        let line = r#"{"message":{"role":"assistant","content":[{"type":"function_call","toolName":"read","intent":"","args":{"path":"/workspace/a.rs"}}]}}"#;
        let out = OmpRunner::format_session_line(line).unwrap();
        assert!(out.iter().any(|l| l == "[ACTION] read"));
        assert!(out.iter().any(|l| l.contains("path: /workspace/a.rs")));

        // Edit-style args: first-line previews of old and new text.
        let line = r#"{"message":{"role":"assistant","content":[{"type":"toolCall","name":"edit","intent":"tweak","arguments":{"oldText":"before line\nold","newText":"after line\nnew"}}]}}"#;
        let out = OmpRunner::format_session_line(line).unwrap();
        assert!(out.iter().any(|l| l == "  - before line"));
        assert!(out.iter().any(|l| l == "  + after line"));

        // Long code in arguments truncates at 35 lines.
        let long = (0..40)
            .map(|i| format!("code {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": [
                {"type": "toolCall", "name": "write", "arguments": {"content": long}}
            ]}
        })
        .to_string();
        let out = OmpRunner::format_session_line(&line).unwrap();
        assert!(out.iter().any(|l| l.contains("(+5 lines omitted)")));
    }

    #[test]
    fn test_format_session_line_tool_start_long_code_and_session_exit() {
        let long = (0..30)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let line = serde_json::json!({
            "customType": "tool_execution_start",
            "data": {"toolName": "write", "args": {"code": long}}
        })
        .to_string();
        let out = OmpRunner::format_session_line(&line).unwrap();
        assert!(out.iter().any(|l| l.contains("(+10 lines)")));
        let out = OmpRunner::format_session_line(r#"{"customType":"session_exit"}"#).unwrap();
        assert_eq!(out, vec!["[SESSION] Agent session concluded.".to_string()]);
    }

    #[test]
    fn test_fallback_extract_files_from_session_writes_code_blocks() {
        let dir = std::env::temp_dir().join(format!("sdb_fallback_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("s.jsonl");
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "here is main\n```rust\nfn main() {}\n```"}
            ]}
        })
        .to_string();
        std::fs::write(&session, format!("{line}\n")).unwrap();
        let wd = dir.join("wd");
        std::fs::create_dir_all(&wd).unwrap();
        OmpRunner::fallback_extract_files_from_session(&session, &wd);
        assert!(
            wd.join("main.rs").exists(),
            "fenced write must be extracted"
        );
        assert!(std::fs::read_to_string(wd.join("main.rs"))
            .unwrap()
            .contains("fn main"));
        // Missing session file is a silent no-op.
        OmpRunner::fallback_extract_files_from_session(&dir.join("nope.jsonl"), &wd);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_task_guidance_prompt_sniffing() {
        let (port, cmd, snippet) = OmpRunner::task_guidance(None, "Build a PDF extractor service");
        assert_eq!(port, 8090);
        assert!(cmd.contains("8090"));
        assert!(snippet.contains("PDF"));
        let (port, cmd, _) = OmpRunner::task_guidance(None, "implement a redis clone");
        assert_eq!(port, 6379);
        assert!(cmd.contains("redis-cli"));
        let (port, cmd, _) = OmpRunner::task_guidance(None, "build a hello world server");
        assert_eq!(port, 8080);
        assert!(cmd.contains("8080"));
    }

    #[test]
    fn test_extract_latest_session_stats_rejects_missing_file() {
        assert!(OmpRunner::extract_latest_session_stats(Some(Path::new(
            "/nonexistent/covtest-session.jsonl"
        )))
        .is_err());
    }

    // -----------------------------------------------------------------
    // Tailer branch coverage (deterministic, temp-dir sessions)
    // -----------------------------------------------------------------

    #[test]
    fn test_tailer_stops_on_cancel_flag() {
        let dir = std::env::temp_dir().join(format!("sdb_tail_cancel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("cancel.jsonl");
        std::fs::write(&session, "{}\n").unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
        ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
        ctx.cancel_flag = Some(Arc::new(AtomicBool::new(true)));
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(ctx));
        handle.join().expect("tailer must exit on cancel");
        running.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tailer_trips_billing_and_reports() {
        let dir = std::env::temp_dir().join(format!("sdb_tail_bill_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("billing.jsonl");
        std::fs::write(
            &session,
            "{\"error\":{\"code\":402,\"message\":\"payment required\"}}\n",
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
        let billing = ctx.billing_tripped.clone();
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(ctx));
        let mut saw_billing = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) if line.contains("[BILLING]") => {
                    saw_billing = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => {
                    if billing.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
        assert!(billing.load(Ordering::SeqCst), "billing flag must trip");
        assert!(saw_billing, "the [BILLING] console line must be sent");
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tailer_zero_usage_assistant_reports_infinity() {
        let dir = std::env::temp_dir().join(format!("sdb_tail_zero_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("zero.jsonl");
        let line = r#"{"message":{"role":"assistant","usage":{},"content":[{"type":"text","text":"hi"}]}}"#;
        std::fs::write(&session, format!("{line}\n")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
        ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
        ctx.limits.max_turns = None;
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(ctx));
        let mut saw_live = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) if line.contains("[LIVE] Turn 1/∞") => {
                    saw_live = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => {}
            }
        }
        assert!(
            saw_live,
            "zero-usage assistant turn must still report [LIVE]"
        );
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tailer_resumes_after_session_rotation() {
        let dir = std::env::temp_dir().join(format!("sdb_tail_rot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("active.jsonl");
        let first = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"first","args":{"command":"echo A"}}}"#;
        std::fs::write(&session, format!("{first}\n")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
        ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(ctx));
        // Wait for the first line to render, so lines_seen is 1.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut saw_first = false;
        while std::time::Instant::now() < deadline && !saw_first {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(l) if l.contains("[ACTION] bash: first") => saw_first = true,
                _ => {}
            }
        }
        assert!(saw_first, "pre-rotation line must render");
        // Atomic rotate, OMP-style: temp file renamed over the live path.
        let second = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"second","args":{"command":"echo B"}}}"#;
        let tmp = dir.join("active.jsonl.tmp");
        std::fs::write(&tmp, format!("{first}\n{second}\n")).unwrap();
        std::fs::rename(&tmp, &session).unwrap();
        let mut saw_rotation = false;
        let mut saw_second = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline && !(saw_rotation && saw_second) {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(l) if l.contains("[TAIL] session file rotated") => saw_rotation = true,
                Ok(l) if l.contains("[ACTION] bash: second") => saw_second = true,
                _ => {}
            }
        }
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();
        assert!(saw_rotation, "rotation must be announced");
        assert!(
            saw_second,
            "post-rotation lines must render without re-counting the first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tailer_drains_remaining_lines_on_exit() {
        let dir = std::env::temp_dir().join(format!("sdb_tail_drain_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("drain.jsonl");
        let line = r#"{"customType":"tool_execution_start","data":{"toolName":"bash","intent":"drained","args":{"command":"echo done"}}}"#;
        std::fs::write(&session, format!("{line}\npartial without newline")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        // Stopped from the start: the drain loop owns the read.
        let running = Arc::new(AtomicBool::new(false));
        let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running);
        ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(ctx));
        let _ = handle.join();
        let mut saw = false;
        while let Ok(l) = rx.try_recv() {
            if l.contains("[ACTION] bash: drained") {
                saw = true;
            }
        }
        assert!(saw, "complete lines must be drained at process exit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tailer_reassembles_partial_line_seekback() {
        let dir = std::env::temp_dir().join(format!("sdb_tail_partial_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("partial.jsonl");
        std::fs::write(&session, "partial without newline").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
        ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(ctx));
        // Let the tailer hit the partial line and seek back before the rest
        // arrives.
        std::thread::sleep(Duration::from_millis(400));
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&session)
                .unwrap();
            f.write_all(b" and completed\n").unwrap();
        }
        let mut saw = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
                if l.contains("[OMP] partial without newline and completed") {
                    saw = true;
                    break;
                }
            }
        }
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();
        assert!(saw, "partial line must be reassembled after seek-back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tailer_emits_waiting_heartbeat_after_idle() {
        let dir = std::env::temp_dir().join(format!("sdb_tail_wait_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("idle.jsonl");
        std::fs::write(&session, "").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
        ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
        ctx.limits.max_turns = Some(2);
        let handle = std::thread::spawn(move || OmpRunner::tail_session_file(ctx));
        // The heartbeat fires on the first idle window past 3s.
        let mut saw_waiting = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(l) if l.contains("[WAITING] Turn 1/2") => {
                    saw_waiting = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => {}
            }
        }
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();
        assert!(saw_waiting, "idle tailer must heartbeat");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // Fake-docker subprocess harness
    // -----------------------------------------------------------------
    //
    // `run_agent_with_logger` resolves `docker` through PATH and spawns it,
    // so a temp-dir script named `docker` is a complete stand-in for the
    // container runtime. PATH is process-global, so the scenarios run in a
    // re-exec'd child of this test binary: the parent only writes the fake
    // binary and asserts the child's exit status, while the child owns every
    // env mutation (PATH/HOME/scenario). Each scenario uses a unique run id,
    // so it never sees the live ~/.omp session dirs and cleans up its own.
    const DRIVER_ENV: &str = "SDB_COVTEST_OMP_DRIVER";
    const BIN_ENV: &str = "SDB_COVTEST_OMP_BIN";
    const WORK_ENV: &str = "SDB_COVTEST_OMP_WORK";

    fn covtest_limits(max_turns: u32, max_budget: f64, timeout: u64) -> AgentExecutionLimits {
        AgentExecutionLimits {
            max_turns: Some(max_turns),
            max_budget_usd: Some(max_budget),
            max_effective_usd: None,
            timeout_seconds: Some(timeout),
        }
    }

    /// Deterministic fake `docker`. `rm`/`kill` touch a kill file while a
    /// scenario script is up (so polling scenarios die the instant the
    /// runner reaps them); `run` seeds the session dir with the scenario's
    /// session jsonl and/or emits stderr.
    const FAKE_DOCKER: &str = r#"#!/bin/sh
# covtest fake `docker`: no engine, no containers, deterministic output.
scenario="${SDB_COVTEST_SCENARIO:-}"
state="${SDB_COVTEST_STATE:-}"
started="${state}.started"
kill="${state}.kill"
sessions="${SDB_COVTEST_SESSIONDIR:-}"

seed_success_session() {
  if [ -n "$sessions" ]; then
    mkdir -p "$sessions"
    {
      printf '%s\n' '{"type":"thinking_level_change","thinkingLevel":"high"}'
      printf '%s\n' '{"message":{"role":"assistant","usage":{"input":11,"output":3,"cacheRead":2},"content":[{"type":"text","text":"here is the server\n```rust\nfn main() {}\n```"}]}}'
    } > "$sessions/session.jsonl"
  fi
}

seed_turn_session() {
  if [ -n "$sessions" ]; then
    mkdir -p "$sessions"
    printf '%s\n' '{"message":{"role":"assistant","usage":{"input":4,"output":1},"content":[{"type":"text","text":"working"}]}}' > "$sessions/session.jsonl"
  fi
}

wait_for_kill() {
  i=0
  while [ "$i" -lt 100 ]; do
    if [ -f "$kill" ]; then exit 0; fi
    sleep 0.1
    i=$((i + 1))
  done
  exit 0
}

case "$1" in
  rm|kill)
    if [ -n "$state" ] && [ -f "$started" ]; then : > "$kill"; fi
    exit 0
    ;;
  run)
    : > "$started"
    case "$scenario" in
      success|network|openrouter|relative)
        seed_success_session
        printf '%s\n' '[BENCH-EVENT] {"v":1,"kind":"message","role":"assistant","text":"hello from extension","thinking":"plan"}'
        printf '%s\n' 'plain progress line'
        sleep 0.6
        exit 0
        ;;
      nonzero)
        printf '%s\n' 'omp exploded: model not found' >&2
        exit 9
        ;;
      nonzero_live)
        printf '%s\n' 'plain progress line'
        sleep 0.3
        exit 9
        ;;
      ready)
        if [ -n "$SDB_COVTEST_WORKDIR" ]; then : > "$SDB_COVTEST_WORKDIR/READY"; fi
        wait_for_kill
        ;;
      timeout|cancel)
        wait_for_kill
        ;;
      max_turns)
        seed_turn_session
        wait_for_kill
        ;;
      cap)
        if [ -n "$sessions" ]; then
          mkdir -p "$sessions"
          {
            printf '%s\n' '{"message":{"role":"assistant","usage":{"input":0,"output":0,"cost":{"total":0.01}},"content":[{"type":"text","text":"one"}]}}'
            sleep 0.15
            printf '%s\n' '{"message":{"role":"assistant","usage":{"input":0,"output":0,"cost":{"total":0.03}},"content":[{"type":"text","text":"two"}]}}'
          } > "$sessions/session.jsonl"
        fi
        wait_for_kill
        ;;
      billing)
        printf '%s\n' 'HTTP/1.1 402 Payment Required' >&2
        sleep 0.5
        exit 0
        ;;
    esac
    exit 1
    ;;
esac
exit 1
"#;

    fn run_omp_scenario(
        name: &str,
        root: &Path,
        limits: AgentExecutionLimits,
        cancel: Option<Arc<AtomicBool>>,
    ) -> (
        Result<OmpSessionStats>,
        Vec<String>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        run_omp_scenario_opts(name, root, limits, cancel, "covtest-model", None, false)
    }

    fn run_omp_scenario_opts(
        name: &str,
        root: &Path,
        limits: AgentExecutionLimits,
        cancel: Option<Arc<AtomicBool>>,
        model: &str,
        network: Option<&str>,
        relative_workdir: bool,
    ) -> (
        Result<OmpSessionStats>,
        Vec<String>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let workdir = if relative_workdir {
            PathBuf::from(format!("ws-{name}"))
        } else {
            root.join(format!("ws-{name}"))
        };
        std::fs::create_dir_all(&workdir).unwrap();
        for suffix in ["started", "kill"] {
            let _ = std::fs::remove_file(root.join(format!("state-{name}.{suffix}")));
        }
        let host_omp = OmpRunner::get_host_omp_dir();
        let run_id = format!("covtest-{}-omp-{name}", std::process::id());
        let session_dir = host_omp.join("agent").join("sessions").join(&run_id);
        let _ = std::fs::remove_dir_all(&session_dir);
        std::env::set_var("SDB_COVTEST_SCENARIO", name);
        std::env::set_var("SDB_COVTEST_STATE", root.join(format!("state-{name}")));
        std::env::set_var("SDB_COVTEST_WORKDIR", &workdir);
        std::env::set_var("SDB_COVTEST_SESSIONDIR", &session_dir);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let logs_c = logs.clone();
        let res = <OmpRunner as crate::sandbox::AgentRunner>::run_agent_with_logger(
            AgentRunSpec {
                model,
                prompt: "covtest prompt",
                workdir: &workdir,
                api_key: Some("sk-covtest"),
                limits,
                effort: Some("high"),
                network,
                task: None,
                run_id: Some(&run_id),
                cancel_flag: cancel,
                worker: 0,
            },
            move |line| logs_c.lock().unwrap().push(line),
        );
        let _ = std::fs::remove_dir_all(&session_dir);
        let logs = logs.lock().unwrap().clone();
        (res, logs, workdir, session_dir)
    }

    // Everything below relies on the fake `docker` being first on PATH.
    fn omp_driver() {
        let bin = std::env::var(BIN_ENV).expect("SDB_COVTEST_OMP_BIN");
        let work = PathBuf::from(std::env::var(WORK_ENV).expect("SDB_COVTEST_OMP_WORK"));
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", bin, old_path));
        let home = work.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        // No shared-policy write; install coverage lives in its own test.
        std::env::set_var("SDB_BENCH_POLICY", "0");

        let root = work.join("scenarios");
        std::fs::create_dir_all(&root).unwrap();
        let mut passed = 0usize;

        // 1. Clean exit 0 with a session file: stats come from the session,
        //    and the fenced code block is fallback-extracted.
        {
            let (res, _logs, workdir, session_dir) =
                run_omp_scenario("success", &root, covtest_limits(10, 1.0, 10), None);
            let st = res.expect("clean exit 0 must be Ok");
            assert_eq!(st.steps_taken, 1, "assistant session line counts as a step");
            assert_eq!(st.prompt_tokens, 11);
            assert_eq!(st.completion_tokens, 3);
            assert_eq!(st.cached_tokens, 2);
            assert_eq!(st.total_tokens, 16);
            assert_eq!(st.effort_effective.as_deref(), Some("high"));
            assert!(
                workdir.join("main.rs").exists(),
                "session fallback must extract the fenced write"
            );
            assert!(!session_dir.exists(), "scenario cleans its own session dir");
            passed += 1;
        }

        // 2. Non-zero exit: typed execution error.
        {
            let (res, _logs, _, _) =
                run_omp_scenario("nonzero", &root, covtest_limits(10, 1.0, 10), None);
            let err = res.expect_err("exit 9 must be an error");
            let exec = err
                .downcast_ref::<OmpExecutionError>()
                .expect("OmpExecutionError");
            assert!(exec.message.contains("Some(9)"), "{}", exec.message);
            // Round-14 N3: stderr only, no session, no stdout - the R1 shape
            // (an argv rejection). The agent never started.
            assert!(!exec.started, "a stderr-only exit never started");
            passed += 1;
        }

        // 2b. Non-zero exit after real output (a lost session file reads as
        // 0 steps): it started, so the pipeline grades it, never retries.
        {
            let (res, _logs, _, _) =
                run_omp_scenario("nonzero_live", &root, covtest_limits(10, 1.0, 10), None);
            let err = res.expect_err("exit 9 must be an error");
            let exec = err
                .downcast_ref::<OmpExecutionError>()
                .expect("OmpExecutionError");
            assert!(exec.started, "stdout is a sign of life");
            passed += 1;
        }

        // 3. READY signal: reaped and scored like a normal exit.
        {
            let (res, logs, _, _) =
                run_omp_scenario("ready", &root, covtest_limits(10, 1.0, 10), None);
            res.expect("READY conclusion must be Ok");
            assert!(logs
                .iter()
                .any(|l| l.contains("signaled completion via /workspace/READY")));
            passed += 1;
        }

        // 4. Watchdog timeout: reaped and concluded (stopped_by_limit).
        {
            let (res, logs, _, _) =
                run_omp_scenario("timeout", &root, covtest_limits(10, 1.0, 1), None);
            res.expect("watchdog stop is a normal conclusion");
            assert!(logs.iter().any(|l| l.contains("Watchdog")));
            passed += 1;
        }

        // 5. Cancel flag: reaped at the first loop turn.
        {
            let cancel = Arc::new(AtomicBool::new(true));
            let (res, logs, _, _) =
                run_omp_scenario("cancel", &root, covtest_limits(10, 1.0, 10), Some(cancel));
            res.expect("cancel conclusion is a normal stop");
            assert!(logs.iter().any(|l| l.contains("Cancellation requested")));
            passed += 1;
        }

        // 6. Max turns: the tailer trips the limit and the run stops Ok.
        {
            let (res, logs, _, _) =
                run_omp_scenario("max_turns", &root, covtest_limits(1, 1.0, 10), None);
            res.expect("turn limit stop is a normal conclusion");
            assert!(logs.iter().any(|l| l.contains("Reached turn limit")));
            passed += 1;
        }

        // 7. Campaign cap headroom: two calls ($0.01, $0.03) with a $0.02
        // effective ceiling trip the cap line, never the $1.00 API budget,
        // and the attempt reports the metered sum and largest call.
        {
            let mut limits = covtest_limits(10, 1.0, 10);
            limits.max_effective_usd = Some(0.02);
            let (res, logs, _, _) = run_omp_scenario("cap", &root, limits, None);
            let st = res.expect("cap trip is a normal conclusion");
            assert!(logs
                .iter()
                .any(|l| l.contains("Reached the campaign cap's headroom")));
            assert!(!logs.iter().any(|l| l.contains("Reached budget cap")));
            assert!((st.cost_usd - 0.04).abs() < 1e-9, "cost {}", st.cost_usd);
            assert!(
                (st.max_call_usd - 0.03).abs() < 1e-9,
                "max {}",
                st.max_call_usd
            );
            passed += 1;
        }

        // 8. Billing phrase on stderr: pause envelope, never a score.
        {
            let (res, logs, _, _) =
                run_omp_scenario("billing", &root, covtest_limits(10, 1.0, 10), None);
            let err = res.expect_err("stderr 402 must trip");
            assert!(err.downcast_ref::<OmpBillingError>().is_some());
            assert!(logs.iter().any(|l| l.contains("[STDERR]")));
            passed += 1;
        }

        // 8. Model id already openrouter/-prefixed: passed through untouched.
        {
            let (res, _, _, _) = run_omp_scenario_opts(
                "openrouter",
                &root,
                covtest_limits(10, 1.0, 10),
                None,
                "openrouter/covtest/model",
                None,
                false,
            );
            res.expect("prefixed model id must run");
            passed += 1;
        }

        // 9. Explicit run network (egress filter off): forwarded to docker.
        {
            let (res, _, _, _) = run_omp_scenario_opts(
                "network",
                &root,
                covtest_limits(10, 1.0, 10),
                None,
                "covtest-model",
                Some("covtest-net"),
                false,
            );
            res.expect("explicit network must not change the run outcome");
            passed += 1;
        }

        // 10. Relative workdir resolution: canonicalized, run still Ok.
        {
            let rel_root = root.clone();
            std::env::set_current_dir(&rel_root).unwrap();
            let (res, _, workdir, _) = run_omp_scenario_opts(
                "relative",
                &rel_root,
                covtest_limits(10, 1.0, 10),
                None,
                "covtest-model",
                None,
                true,
            );
            res.expect("relative workdir must resolve and run");
            assert!(workdir.join("main.rs").exists());
            passed += 1;
        }

        println!("fake-docker omp scenarios passed ({passed})");
    }

    /// Re-exec'd child entry point: the parent sets DRIVER_ENV, so a normal
    /// suite run is a no-op.
    #[test]
    fn fake_docker_driver() {
        if std::env::var(DRIVER_ENV).ok().as_deref() != Some("1") {
            return;
        }
        omp_driver();
    }

    /// Parent-side scenario launcher: writes the fake `docker` and runs the
    /// driver in a child process so PATH/HOME mutations never leak into the
    /// parallel test suite.
    #[test]
    fn test_omp_runner_fake_docker_scenarios() {
        let root = std::env::temp_dir().join(format!("covtest-omp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let docker = root.join("bin/docker");
        std::fs::write(&docker, FAKE_DOCKER).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::omp::tests::fake_docker_driver",
                "--nocapture",
            ])
            .env(DRIVER_ENV, "1")
            .env(BIN_ENV, root.join("bin"))
            .env(WORK_ENV, &root)
            .output()
            .expect("spawn fake-docker driver child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "fake-docker omp driver failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("fake-docker omp scenarios passed"),
            "driver must report its scenarios; got:\n{stdout}\n{stderr}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod billing_tripwire_tests {
    use super::classify_billing_error;

    #[test]
    fn tripwire_fires_on_billing_signals() {
        // HTTP status lines (stderr shape).
        assert!(classify_billing_error("HTTP/1.1 402 Payment Required").is_some());
        assert!(classify_billing_error("HTTP/1.1 401 Unauthorized: invalid api key").is_some());
        assert!(classify_billing_error("HTTP/1.1 429 Too Many Requests").is_some());
        // Provider phrases (either arm).
        assert!(classify_billing_error("insufficient credit balance").is_some());
        assert!(classify_billing_error("credit exhausted, top up").is_some());
        assert!(classify_billing_error("too many requests, slow down").is_some());
        // JSON error envelopes (session-jsonl shape).
        assert!(classify_billing_error(
            "{\"error\":{\"code\":402,\"message\":\"payment required\"}}"
        )
        .is_some());
        assert!(
            classify_billing_error("{\"error\":{\"code\":401,\"message\":\"unauthorized\"}}")
                .is_some()
        );
        assert!(classify_billing_error("{\"error\":\"rate_limit_exceeded\"}").is_some());
    }

    #[test]
    fn tripwire_ignores_model_failures() {
        assert!(classify_billing_error("test failed: assertion left == right").is_none());
        assert!(classify_billing_error("Read timed out").is_none());
        assert!(classify_billing_error("exit code 1").is_none());
        // Bare digit tokens in free text never match: toolCallIds, line
        // numbers, token counts (the wave-v2 false-positive class).
        assert!(classify_billing_error("port 14029 in use").is_none());
        assert!(
            classify_billing_error("call_01a06b63cd207e008dc36f8fb57da56b|fc_tmp_wvrvxb058tr")
                .is_none()
        );
        assert!(classify_billing_error("401: if err != nil {").is_none());
        assert!(classify_billing_error("{\"usage\":{\"input\":5280,\"output\":429}}").is_none());
        assert!(
            classify_billing_error("[SUBRUN ERROR] Subrun 1/3 failed: OMP agent exited").is_none()
        );
        // Bare "Provider returned 429" without status-line/phrase shape.
        assert!(classify_billing_error("Provider returned 429").is_none());
    }
}
