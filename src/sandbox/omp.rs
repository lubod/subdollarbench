use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
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
use std::time::{Duration, Instant, SystemTime};
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

/// R56 F1: one tool call that never returned. Recorded on the ticket's
/// manifest and in the live line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolStall {
    pub tool: String,
    pub command: String,
    pub started_at: String,
    pub ended_at: String,
}

/// R56 F1/R57 F2: classify a stopped session. The [STALL] marker wins over
/// any model text in the command (which may contain "turn limit").
fn stop_reason_from(reason: &str) -> &'static str {
    if reason.contains("[STALL]") {
        "wall"
    } else if reason.contains("turn limit") {
        "turns"
    } else if reason.contains("budget cap") || reason.contains("campaign cap's headroom") {
        "dollars"
    } else if reason.contains("Watchdog") {
        "wall"
    } else {
        "crash"
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct OmpSessionStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    /// R24 R3: new context written into the prompt cache; billed at
    /// cache_write_per_million, never at $0.
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub steps_taken: u32,
    /// Live-metered provider spend for this attempt (sum of every call).
    /// The campaign cap is enforced against this plus the compute floor.
    pub cost_usd: f64,
    /// R67 F2: the provider-billed total from each session message's
    /// `usage.cost.total`, kept beside the list-price `cost_usd`.
    pub billed_usd: f64,
    /// The largest single provider call in this attempt (the campaign cap's
    /// reserve).
    pub max_call_usd: f64,
    /// Effective effort as negotiated, harness-interpreted: OMP records the
    /// session's `thinking_level_change` value (CLI downgrades included);
    /// OpenCode records `#<variant>` or `prose-only`. Requested effort
    /// lives on the manifest/row `effort` field; this one says what ran.
    /// None = unknown (no session file, pre-field row).
    pub effort_effective: Option<String>,
    /// R23 D1/D5: why the session ended: dollars | turns | wall | ready |
    /// crash. None when the runner could not classify the end.
    pub stop_reason: Option<String>,
    /// R56 F1: the tool call that hung, when the stall guard ended the
    /// session.
    pub stall: Option<ToolStall>,
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
    // R67 F1: omp's `@<slug>` suffix sets OpenRouter's `provider.only` to
    // that endpoint, so every call goes to the pinned provider.
    let wire_model = match crate::cost::calculator::provider_for_model(model) {
        Some(provider) => format!("{model}@{provider}"),
        None => model.to_string(),
    };
    let mut args = vec![
        format!("--model={wire_model}"),
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

/// R68 F4: omp's model-resolution failure. omp 18.4.2 prints
/// `Model "..." not found` and then waits for the watchdog instead of
/// exiting, so this text is the only signal that the agent never started.
fn is_model_not_found(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("model") && lower.contains("not found")
}

/// R77 F1: the verdict for a provider error line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BillingSignal {
    /// 402/401, or a 429 that ends the session: pause the wave.
    Fault(&'static str),
    /// A 429 omp is still retrying (`Retrying (n/m)`, n < m): count it,
    /// never trip.
    RateLimited,
}

impl BillingSignal {
    pub fn reason(&self) -> &'static str {
        match self {
            BillingSignal::Fault(reason) => reason,
            BillingSignal::RateLimited => "provider rate-limited (HTTP 429), retrying",
        }
    }
}

/// Billing tripwire: classify provider session errors that must pause the
/// wave instead of scoring a 0% model failure. Matches HTTP billing/auth/
/// throttle statuses in session JSON or stderr text (402/401/429 + phrases).
/// Returns the fault or the transient rate limit, else None.
/// Structured provider-error match. Bare `401`/`402`/`429` digit tokens
/// appear routinely in free-form agent output (toolCallIds
/// `...c3d429b`, line numbers `401:`, token counts `"output":429`), so
/// they NEVER match here. A trip requires one of:
/// - a JSON error envelope with a numeric `code`/`status` field 401/402/429,
/// - an HTTP status line (`HTTP/1.1 402 ...`),
/// - a provider phrase (payment required / insufficient credit / rate limit /
///   unauthorized + key/token/auth), including OpenRouter's
///   `requires more credits` / `can only afford` / `more credits` +
///   `max_tokens`, and a `402 `-prefixed line holding one of them;
/// - OpenRouter's session envelope `errorStatus`/`errorId` 402/401/429 with
///   an `errorMessage`.
///
/// A blank line ends the list; the caller note below is ordinary prose, not a
/// continuation item.
///
/// Callers: session-jsonl lines are parsed as JSON first (structured arm);
/// raw stderr lines use the text arm (status-line + phrases only).
pub fn classify_billing_error(text: &str) -> Option<BillingSignal> {
    let low = text.to_lowercase();
    // Structured arm: JSON envelope with numeric error code.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        let code = v
            .pointer("/error/code")
            .or_else(|| v.pointer("/error/status"))
            // R65 F1: OpenRouter's session envelope puts the status on the
            // message (`errorStatus`/`errorId`), not under `error.code`.
            // Only 402 is new here; a session-level 401/429 keeps its
            // existing handling and must not become a billing trip.
            .or_else(|| {
                v.pointer("/message/errorStatus")
                    .or_else(|| v.pointer("/message/errorId"))
                    .filter(|c| c.as_u64() == Some(402))
            })
            .or_else(|| v.get("statusCode"))
            .or_else(|| v.get("code"))
            .and_then(|c| {
                c.as_u64()
                    .or_else(|| c.as_str().and_then(|s| s.parse::<u64>().ok()))
            });
        if let Some(c) = code {
            if c == 402 {
                return Some(BillingSignal::Fault(
                    "provider billing exhausted (HTTP 402)",
                ));
            }
            if c == 401 {
                return Some(BillingSignal::Fault("provider auth rejected (HTTP 401)"));
            }
            if c == 429 {
                return Some(rate_limit_signal(&low));
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
                return Some(rate_limit_signal(&low));
            }
            return Some(BillingSignal::Fault(
                "provider billing exhausted (HTTP 402)",
            ));
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
                return Some(BillingSignal::Fault(
                    "provider billing exhausted (HTTP 402)",
                ));
            }
            if msg.contains("rate limit")
                || msg.contains("rate_limit")
                || msg.contains("rate-limit")
                || msg.contains("rate-limited")
                || msg.contains("too many requests")
            {
                return Some(rate_limit_signal(&low));
            }
            if msg.contains("unauthorized")
                && (msg.contains("key") || msg.contains("token") || msg.contains("auth"))
            {
                return Some(BillingSignal::Fault("provider auth rejected (HTTP 401)"));
            }
        }
        // R65 F1: OpenRouter's session line carries the outage in
        // `message.errorMessage`. Only billing phrases count here, so a
        // rate-limit or auth message keeps its old handling.
        let provider_msg = v
            .pointer("/message/errorMessage")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_lowercase();
        if provider_msg.contains("payment required")
            || provider_msg.contains("insufficient credit")
            || provider_msg.contains("credit exhausted")
            || provider_msg.contains("out of credit")
            || provider_msg.contains("requires more credits")
            || provider_msg.contains("can only afford")
            || (provider_msg.contains("more credits") && provider_msg.contains("max_tokens"))
        {
            return Some(BillingSignal::Fault(
                "provider billing exhausted (HTTP 402)",
            ));
        }
        return None;
    }
    // Text arm (stderr): HTTP status line or provider phrase. Never digits.
    let status_line = low.contains("http/1.1 402")
        || low.contains("http/1.0 402")
        || low.contains("http/2 402")
        || low.contains("status 402")
        || low.contains("status: 402");
    // R65 F1: OpenRouter's 402 text. `requires more credits` and
    // `can only afford` are specific; a bare `more credits` only counts
    // with `max_tokens` (so a `credits` table or game-loop prose never
    // trips), and a `402 `-prefixed line still needs a billing phrase.
    let phrase = low.contains("payment required")
        || low.contains("insufficient credit")
        || low.contains("credit exhausted")
        || low.contains("out of credit");
    let openrouter = low.contains("requires more credits")
        || low.contains("can only afford")
        || (low.contains("more credits") && low.contains("max_tokens"));
    let prefixed_402 = low.trim_start().starts_with("402 ") && (openrouter || phrase);
    if status_line || phrase || openrouter || prefixed_402 {
        return Some(BillingSignal::Fault(
            "provider billing exhausted (HTTP 402)",
        ));
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
        return Some(BillingSignal::Fault("provider auth rejected (HTTP 401)"));
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
        return Some(rate_limit_signal(&low));
    }
    None
}

/// R77 F1: a 429 is transient while omp is still retrying it. The
/// `Retrying (n/m)` marker tells the two apart; without it, or with the
/// retries exhausted (n = m), the 429 ended the session and trips.
fn rate_limit_signal(low: &str) -> BillingSignal {
    match retry_counts(low) {
        Some((n, m)) if n > 0 && n < m => BillingSignal::RateLimited,
        _ => BillingSignal::Fault("provider rate-limited (HTTP 429)"),
    }
}

/// The `Retrying (n/m)` marker omp prints around a provider retry.
fn retry_counts(low: &str) -> Option<(u32, u32)> {
    let start = low.find("retrying (")? + "retrying (".len();
    let rest = &low[start..];
    let end = rest.find(')')?;
    let (n, m) = rest[..end].split_once('/')?;
    Some((n.trim().parse().ok()?, m.trim().parse().ok()?))
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
    /// R56 F1: a tool call with no result for this long ends the session.
    pub stall_after: Duration,
    /// R56 F1: the stall to record when the guard fires.
    pub stall: Arc<Mutex<Option<ToolStall>>>,
}

/// R56 F1: `SDB_TOOL_STALL_S`, default 600.
fn tool_stall_after() -> Duration {
    Duration::from_secs(
        std::env::var("SDB_TOOL_STALL_S")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(600),
    )
}

/// R56 F1/F2: the tool call currently in flight in a session.
struct InflightTool {
    id: String,
    tool: String,
    command: String,
    started_at: String,
    started: Instant,
}

/// One line, clipped for the live line.
fn clip_command(command: &str, max: usize) -> String {
    let one = command.replace(['\n', '\r'], " ");
    let one = one.trim();
    if one.chars().count() <= max {
        return one.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = one.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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
            .args(crate::grading::cpuset_args(
                &crate::grading::session_cpuset(),
            ))
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
        let effective_prompt = include_str!("../prompts/omp_effective.md")
            .replace("{{PROMPT}}", prompt)
            .replace("{{GUIDANCE}}", &guidance_snippet)
            .replace("{{PORT}}", &target_port.to_string())
            .replace("{{TEST}}", &test_cmd);

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
        let stall = Arc::new(Mutex::new(None::<ToolStall>));
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
            stall_after: tool_stall_after(),
            stall: stall.clone(),
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
        // R68 F4: omp 18.4.2 prints "Model ... not found" and then waits
        // for the watchdog instead of exiting. Keep the text so a run
        // that never started is a harness fault, not an Ok session.
        let model_not_found = Arc::new(AtomicBool::new(false));
        if let Some(stdout) = child.stdout.take() {
            let tx_out = tx.clone();
            let ext_flag = extension_feed.clone();
            let seen = stdout_seen.clone();
            let not_found_out = model_not_found.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        seen.store(true, Ordering::SeqCst);
                    }
                    if is_model_not_found(trimmed) {
                        not_found_out.store(true, Ordering::SeqCst);
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
        let not_found_err = model_not_found.clone();
        if let Some(stderr) = child.stderr.take() {
            let tx_err = tx.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        if matches!(
                            classify_billing_error(trimmed),
                            Some(BillingSignal::Fault(_))
                        ) {
                            billing_tripped_err.store(true, Ordering::SeqCst);
                        }
                        if is_model_not_found(trimmed) {
                            not_found_err.store(true, Ordering::SeqCst);
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
        let mut ready_seen = false;
        // R68 F4: only the wall-clock watchdog turns a never-started run
        // into a harness fault; a cancellation is an operator stop.
        let mut watchdog_stopped = false;
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
                    watchdog_stopped = true;
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
                ready_seen = true;
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
            stats.stop_reason = Some("crash".to_string());
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
        stats.stall = stall.lock().ok().and_then(|s| s.clone());

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
                    stats.stop_reason = Some("crash".to_string());
                    return Err(anyhow::Error::new(OmpExecutionError {
                        message: format!("OMP agent exited with non-zero status: {:?}", s.code()),
                        partial_stats: stats,
                        started,
                    }));
                }
            }
        }

        // R68 F4: a run that never started is a harness fault, never an Ok
        // session. omp 18.4.2 prints the model-not-found text and waits for
        // the watchdog; a watchdog stop with no session and no turn is the
        // same fault without the text.
        let never_started = final_path.is_none()
            && turn_counter.load(Ordering::SeqCst) == 0
            && stats.steps_taken == 0
            && stats.total_tokens == 0;
        if never_started && model_not_found.load(Ordering::SeqCst) {
            stats.stop_reason = Some("crash".to_string());
            return Err(anyhow::Error::new(OmpExecutionError {
                message: "OMP agent reported the model was not found".to_string(),
                partial_stats: stats,
                started: false,
            }));
        }
        if watchdog_stopped && never_started {
            stats.stop_reason = Some("crash".to_string());
            return Err(anyhow::Error::new(OmpExecutionError {
                message: "OMP agent never started before the watchdog".to_string(),
                partial_stats: stats,
                started: false,
            }));
        }

        // R23 D1/D5: record why the session ended. A budget/turn/watchdog
        // stop is a normal conclusion; a READY signal or a clean exit is
        // `ready`; a non-zero exit already returned `crash` above.
        let stop = if ready_seen {
            "ready"
        } else if stopped_by_limit {
            let reason = limit_reason
                .lock()
                .ok()
                .and_then(|r| r.clone())
                .unwrap_or_default();
            stop_reason_from(&reason)
        } else {
            "ready"
        };
        stats.stop_reason = Some(stop.to_string());
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
        let mut inflight: Option<InflightTool> = None;
        let mut stall_fired = false;

        while ctx.running.load(Ordering::SeqCst) {
            if let Some(ref cf) = ctx.cancel_flag {
                if cf.load(Ordering::SeqCst) {
                    break;
                }
            }
            // R56 F1: a tool call that never returns ends the session as
            // --max-time does, graded on the workspace as it stands.
            if !stall_fired {
                if let Some(tool) = inflight.as_ref() {
                    let elapsed = tool.started.elapsed();
                    if elapsed >= ctx.stall_after {
                        let command = clip_command(&tool.command, 120);
                        let msg = format!(
                            "[STALL] {} ran {} s without returning ({}); ending the session, graded on the workspace as is",
                            tool.tool,
                            elapsed.as_secs(),
                            command
                        );
                        let _ = ctx.tx.send(msg.clone());
                        *ctx.limit_reason.lock().unwrap() = Some(msg);
                        ctx.limit_reached.store(true, Ordering::SeqCst);
                        if let Ok(mut store) = ctx.stall.lock() {
                            *store = Some(ToolStall {
                                tool: tool.tool.clone(),
                                command: tool.command.clone(),
                                started_at: tool.started_at.clone(),
                                ended_at: chrono::Utc::now().to_rfc3339(),
                            });
                        }
                        stall_fired = true;
                    }
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
                    let idle = last_activity.elapsed().as_secs();
                    if idle >= 3 && last_heartbeat.elapsed() >= Duration::from_secs(3) {
                        last_heartbeat = std::time::Instant::now();
                        let max_str = if max_t > 0 {
                            max_t.to_string()
                        } else {
                            "∞".to_string()
                        };
                        // R56 F2: a heartbeat while a tool call is in
                        // flight names the tool and its command.
                        let waiting = match inflight.as_ref() {
                            Some(tool) => {
                                let command = clip_command(&tool.command, 100);
                                let what = if command.is_empty() {
                                    tool.tool.clone()
                                } else {
                                    format!("{}: {}", tool.tool, command)
                                };
                                format!(
                                    "[WAITING] Turn {}/{}: tool running ({what}) (elapsed: {}s)",
                                    cur_turns + 1,
                                    max_str,
                                    tool.started.elapsed().as_secs()
                                )
                            }
                            None => format!(
                                "[WAITING] Turn {}/{} in progress: Model is reasoning and generating response... (elapsed: {}s)",
                                cur_turns + 1,
                                max_str,
                                idle
                            ),
                        };
                        let _ = ctx.tx.send(waiting);
                    }
                    thread::sleep(Duration::from_millis(150));
                }
                Ok(_) => {
                    lines_seen += 1;
                    last_activity = std::time::Instant::now();
                    if line_buf.ends_with('\n') {
                        // Billing tripwire: provider 402/401/429 must pause the
                        // wave, never score a 0% model failure.
                        if let Some(signal) = crate::sandbox::omp::classify_billing_error(&line_buf)
                        {
                            if let BillingSignal::Fault(reason) = signal {
                                ctx.billing_tripped.store(true, Ordering::SeqCst);
                                let _ = ctx.tx.send(format!("[BILLING] {reason}; pausing queue."));
                            }
                        }
                        // Check limits & telemetry in session line
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line_buf.trim())
                        {
                            // R56 F1/F2: track the tool call in flight. A
                            // result clears it; a start replaces it.
                            if val.get("type").and_then(|v| v.as_str()) == Some("custom")
                                && val.get("customType").and_then(|v| v.as_str())
                                    == Some("tool_execution_start")
                            {
                                let d = &val["data"];
                                inflight = Some(InflightTool {
                                    id: d["toolCallId"].as_str().unwrap_or("").to_string(),
                                    tool: d["toolName"].as_str().unwrap_or("tool").to_string(),
                                    command: d
                                        .pointer("/args/command")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    started_at: d["startedAt"]
                                        .as_str()
                                        .map(str::to_string)
                                        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                                    started: Instant::now(),
                                });
                                stall_fired = false;
                            }
                            if val.pointer("/message/role").and_then(|r| r.as_str())
                                == Some("toolResult")
                            {
                                let id =
                                    val.pointer("/message/toolCallId").and_then(|v| v.as_str());
                                if inflight
                                    .as_ref()
                                    .map(|i| id.map(|x| i.id == x).unwrap_or(true))
                                    .unwrap_or(false)
                                {
                                    inflight = None;
                                }
                            }
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
                            // 2. Budget check: every event is priced from
                            // the committed snapshot. The provider's
                            // usage.cost.total is ignored; it is an actual
                            // charge, not the list price (R24 R2).
                            let mut cost_updated = false;
                            let usage_node =
                                val.pointer("/message/usage").or_else(|| val.get("usage"));
                            let delta_cost: Option<f64> = (|| {
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
                                let tcwrite = usage
                                    .get("cacheWrite")
                                    .or_else(|| usage.get("cache_write_input_tokens"))
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0);
                                if tin + tout + tcached + tcwrite == 0 {
                                    None
                                } else {
                                    let pricing = crate::cost::ModelPricing::for_model(&ctx.model);
                                    Some(
                                        pricing
                                            .compute_cost_with_cache_and_write(
                                                tin, tcached, tout, tcwrite,
                                            )
                                            .total_cost_usd,
                                    )
                                }
                            })();
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
                                    .or_else(|| usage.get("input_tokens"))
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0);
                                let tout = usage
                                    .get("output")
                                    .or_else(|| usage.get("completion_tokens"))
                                    .or_else(|| usage.get("output_tokens"))
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
                                    .or_else(|| {
                                        usage
                                            .get("input_tokens_details")
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
        let mut cache_write_tokens = 0u64;
        let mut steps = 0u32;
        let mut cost_usd = 0.0f64;
        let mut billed_usd = 0.0f64;
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
                        .or_else(|| usage.get("input_tokens"))
                        .and_then(|x| x.as_u64())
                    {
                        prompt_tokens += pt;
                    }
                    if let Some(ct) = usage
                        .get("output")
                        .or_else(|| usage.get("completion_tokens"))
                        .or_else(|| usage.get("output_tokens"))
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
                    } else if let Some(details) = usage
                        .get("prompt_tokens_details")
                        .or_else(|| usage.get("input_tokens_details"))
                    {
                        if let Some(c) = details.get("cached_tokens").and_then(|x| x.as_u64()) {
                            cached_tokens += c;
                        }
                    }
                    if let Some(cw) = usage
                        .get("cacheWrite")
                        .or_else(|| usage.get("cache_write_input_tokens"))
                        .or_else(|| usage.get("cache_creation_input_tokens"))
                        .and_then(|x| x.as_u64())
                    {
                        cache_write_tokens += cw;
                    }
                    if let Some(c) = usage
                        .get("cost")
                        .and_then(|c| c.get("total"))
                        .and_then(|x| x.as_f64())
                    {
                        cost_usd += c;
                        billed_usd += c;
                        max_call_usd = max_call_usd.max(c);
                    }
                }
            }
        }

        Ok(OmpSessionStats {
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
            total_tokens: prompt_tokens + cached_tokens + cache_write_tokens + completion_tokens,
            steps_taken: steps,
            cost_usd,
            billed_usd,
            max_call_usd,
            effort_effective: thinking_effective,
            stop_reason: None,
            stall: None,
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
        include_str!("../prompts/omp_agents.md")
            .replace("{{GUIDANCE}}", snippet)
            .replace("{{PORT}}", &port.to_string())
            .replace("{{TEST}}", test)
    }

    pub fn task_guidance(
        _task: Option<crate::config::TaskType>,
        _prompt: &str,
    ) -> (u16, &'static str, &'static str) {
        (
            5480,
            "printf 'SELECT 1\n' | nc 127.0.0.1 5480",
            r#"ANTI-CHEAT REQUIREMENT (STRICT):
        - Embedded database engines and SQL parser libraries are STRICTLY FORBIDDEN: no SQLite (Python `sqlite3`, `node:sqlite`, `bun:sqlite`, Go/Rust drivers), no duckdb, no `sqlite3`/`psql` binaries, no vendored engine sources. This includes test and helper files: the scanner checks every file.
        - Build the SQL server from raw TCP sockets on port 5480: one statement per line, `OK <n>` / `ROWS <ncols>` frames / `ERR <class> <text>` answers, tables strictly typed.
        - Launch: `./start.sh --port <p> --data <dir>` must work (defaults: 5480, ./data). A Dockerfile gets those arguments only with `ENTRYPOINT`; a `CMD`-only image must read `SDB_PORT`/`SDB_DATA_DIR` or use the defaults.
        - Ground truth: a reference server speaking the same protocol runs at host `ref-sql`, port 5480 (`printf 'SELECT 1\n' | nc ref-sql 5480`). Compare against it while developing; it is not reachable while you are graded.
        - `SPEC.md` in the workspace holds the full contract so far (harness-owned, rewritten every ticket)."#,
        )
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
mod tests;

#[cfg(test)]
mod billing_tripwire_tests {
    use super::{classify_billing_error, BillingSignal};

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
        // R65 F1: `more credits` alone is not billing (a credits table or
        // game-loop prose is ordinary text), and `max_tokens = 402` is not
        // a 402 status line.
        assert!(classify_billing_error("we need more credits in the game loop").is_none());
        assert!(classify_billing_error("max_tokens = 402").is_none());
        assert!(classify_billing_error("CREATE TABLE credits (id int); -- more credits").is_none());
        assert!(classify_billing_error("402 rows affected").is_none());
    }

    /// R65 F1: a bare `more credits`, `max_tokens = 402` or a `402 ` line
    /// without a billing phrase is ordinary text, never a trip.
    #[test]
    fn r65_f1_no_false_trip() {
        for text in [
            "we need more credits in the game loop",
            "max_tokens = 402",
            "CREATE TABLE credits (id int); -- more credits",
            "402 rows affected",
            // A session-level rate limit is not the OpenRouter outage.
            r#"{"type":"message","message":{"role":"assistant","stopReason":"error","errorStatus":429,"errorMessage":"rate limit exceeded"}}"#,
        ] {
            assert!(
                classify_billing_error(text).is_none(),
                "{text} must not trip"
            );
        }
    }

    /// R65 F1 (R63-3): the exact OpenRouter outage line, its max_tokens
    /// variant, and the session envelope that carries errorStatus 402.
    #[test]
    fn r65_f1_openrouter_402_trips() {
        let line = "402 This request requires more credits, or fewer max_tokens. You requested up to 65536 tokens, but can only afford 44804. To increase, visit https://openrouter.ai/workspaces/default/keys/REDACTED and adjust the key's total limit";
        assert_eq!(
            classify_billing_error(line),
            Some(BillingSignal::Fault(
                "provider billing exhausted (HTTP 402)"
            ))
        );
        let variant = "402 This request requires more credits, or fewer max_tokens. You requested up to 131072 tokens, but can only afford 47664. To increase, visit https://openrouter.ai/workspaces/default/keys/REDACTED and adjust the key's total limit";
        assert_eq!(
            classify_billing_error(variant),
            Some(BillingSignal::Fault(
                "provider billing exhausted (HTTP 402)"
            ))
        );
        // The real session jsonl shape: the status and message live on the
        // message, not under error.code/error.message.
        let session = r#"{"type":"message","message":{"role":"assistant","content":[],"usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0},"stopReason":"error","errorStatus":402,"errorId":402,"errorMessage":"402 This request requires more credits, or fewer max_tokens. You requested up to 65536 tokens, but can only afford 44804. To increase, visit https://openrouter.ai/workspaces/default/keys/REDACTED and adjust the key's total limit"}}"#;
        assert_eq!(
            classify_billing_error(session),
            Some(BillingSignal::Fault(
                "provider billing exhausted (HTTP 402)"
            ))
        );
    }

    /// R77 F1: the real ling stderr line is a transient retry, not a
    /// fault, in both the raw and the live.log shape.
    #[test]
    fn r77_f1_retried_429_is_transient() {
        let line = "Retrying (1/10): 429 Provider returned error retry-after-ms=5000 inclusionai/ling-3.1-flash is temporarily rate-limited upstream. Please retry shortly, or add your own provider key to use your own limits: https://openrouter.ai/workspaces/default/byok";
        assert_eq!(
            classify_billing_error(line),
            Some(BillingSignal::RateLimited)
        );
        assert_eq!(
            classify_billing_error(&format!("[STDERR] {line}")),
            Some(BillingSignal::RateLimited)
        );
    }

    /// R77 F1: exhausted retries and a final 429 still trip; 402/401 do.
    #[test]
    fn r77_f1_exhausted_429_trips() {
        let exhausted = "Retrying (10/10): 429 Provider returned error retry-after-ms=5000 inclusionai/ling-3.1-flash is temporarily rate-limited upstream.";
        assert_eq!(
            classify_billing_error(exhausted),
            Some(BillingSignal::Fault("provider rate-limited (HTTP 429)"))
        );
        assert_eq!(
            classify_billing_error("HTTP/1.1 429 Too Many Requests"),
            Some(BillingSignal::Fault("provider rate-limited (HTTP 429)"))
        );
        assert_eq!(
            classify_billing_error(r#"{"error":{"code":429,"message":"rate limit exceeded"}}"#),
            Some(BillingSignal::Fault("provider rate-limited (HTTP 429)"))
        );
        assert!(matches!(
            classify_billing_error("HTTP/1.1 402 Payment Required"),
            Some(BillingSignal::Fault(_))
        ));
        assert!(matches!(
            classify_billing_error("HTTP/1.1 401 Unauthorized: invalid api key"),
            Some(BillingSignal::Fault(_))
        ));
        // A retry marker never masks a 401 or 402 fault.
        assert!(matches!(
            classify_billing_error("Retrying (1/10): 402 Payment Required"),
            Some(BillingSignal::Fault(_))
        ));
        assert!(matches!(
            classify_billing_error("Retrying (1/10): 401 Unauthorized: invalid api key"),
            Some(BillingSignal::Fault(_))
        ));
    }
}
