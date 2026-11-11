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
/// - effort is machine-level first: benchmark effort maps to a model
///   `#variant` (`-m provider/model#variant`, defined per-trial via
///   `OPENCODE_CONFIG_CONTENT`) which the openrouter package forwards as
///   native `reasoning.effort`; prose effort line stays as fallback, and a
///   variant-resolution failure retries prose-only (today's behavior).
pub struct OpenCodeRunner;

impl OpenCodeRunner {
    /// Stats mapping (documented, lossless):
    /// prompt = Σ input, completion = Σ (output + reasoning),
    /// cached = Σ cache.read, total = Σ (input + output + reasoning).
    /// Reasoning bills provider-side as completion, so it joins completion.
    fn add_step(stats: &mut OmpSessionStats, tokens: &serde_json::Value, model: &str) -> f64 {
        let u64f = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_u64()).unwrap_or(0);
        let input = u64f(tokens.get("input"));
        let output = u64f(tokens.get("output"));
        let reasoning = u64f(tokens.get("reasoning"));
        let cache_read = tokens
            .get("cache")
            .and_then(|c| c.get("read"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let cache_write = tokens
            .get("cache")
            .and_then(|c| c.get("write"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        // R24 R2/R3: price the event from the committed snapshot, including
        // cache-write context; never from the provider's own `part.cost`.
        let cost = crate::cost::ModelPricing::for_model(model)
            .compute_cost_with_cache_and_write(input, cache_read, output + reasoning, cache_write)
            .total_cost_usd;
        stats.prompt_tokens += input;
        stats.completion_tokens += output + reasoning;
        stats.cached_tokens += cache_read;
        stats.cache_write_tokens += cache_write;
        stats.total_tokens += input + output + reasoning + cache_write;
        stats.cost_usd += cost;
        stats.max_call_usd = stats.max_call_usd.max(cost);
        cost
    }

    /// Provider rate-limit signal on one `--format json` event or raw line.
    /// Structured-only for digits: a bare 429 (token counts, toolCallIds)
    /// must never match — only `status`/`code`-shaped 429s, `rate-limit`
    /// type slugs, and explicit rate-limit phrases. Billing/auth (402/401)
    /// never match here; those keep the immediate wave-pause path.
    fn is_rate_limit_event(v: &serde_json::Value, raw: &str) -> bool {
        fn walk(x: &serde_json::Value) -> bool {
            match x {
                serde_json::Value::String(s) => {
                    let l = s.to_lowercase();
                    l.contains("rate-limit")
                        || l.contains("rate_limit")
                        || l.contains("temporarily rate-limited")
                }
                serde_json::Value::Array(a) => a.iter().any(walk),
                serde_json::Value::Object(m) => m.iter().any(|(k, val)| {
                    let kl = k.to_lowercase();
                    if (kl == "status" || kl == "code" || kl == "statuscode")
                        && val.as_u64() == Some(429)
                    {
                        return true;
                    }
                    if kl == "type"
                        && val
                            .as_str()
                            .map(|s| s.to_lowercase().contains("rate-limit"))
                            .unwrap_or(false)
                    {
                        return true;
                    }
                    walk(val)
                }),
                _ => false,
            }
        }
        if walk(v) {
            return true;
        }
        let l = raw.to_lowercase();
        l.contains("rate limit")
            || l.contains("rate-limit")
            || l.contains("rate_limit")
            || l.contains("too many requests")
    }

    /// Backoff before resume attempt `attempt` (0-based): 60s doubling,
    /// capped at 15min. Sleeps in small slices so cancel/deadline break it.
    fn backoff_sleep(attempt: u32, deadline: SystemTime, cancel: &Option<Arc<AtomicBool>>) -> bool {
        let secs = (60u64.saturating_mul(1u64 << attempt.min(4))).min(900);
        let slices = secs.saturating_mul(2);
        for _ in 0..slices {
            if SystemTime::now() > deadline {
                return false;
            }
            if cancel
                .as_ref()
                .map(|f| f.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                return false;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        true
    }

    /// A "clean exit" that never ran the model: the provider answered the
    /// attempt with a rate-limit/quota signal (per-attempt flag) and the
    /// whole session produced zero model tokens and zero tool calls. The
    /// process exit code says nothing about the model on a 429 wall —
    /// opencode's internal retry ladder can exit 0 — so this must never
    /// conclude as a model verdict. Live defect this fixes: the free-tier
    /// `free-models-per-day-high-balance` quota, 4 trials x ~50 min of
    /// backoff, each recorded as `failed_tests` with 0 tokens and ranked
    /// P@1 0% — the exact provider-fault-as-model-failure this repo's
    /// non-result doctrine (RULES 2026-10-13) exists to exclude.
    fn is_throttled_noop(rate_limited: bool, total_tokens: u64, tools: u32) -> bool {
        rate_limited && total_tokens == 0 && tools == 0
    }

    fn cap_line(s: &str) -> String {
        const MAX: usize = 1500;
        if s.len() > MAX {
            // Byte slicing can split a multi-byte char (CJK/emoji) and
            // panic the reader thread; floor to a char boundary instead.
            let idx = s.floor_char_boundary(MAX);
            format!("{}…[truncated]", &s[..idx])
        } else {
            s.to_string()
        }
    }

    /// OpenRouter-only policy: the bench standardizes on `openrouter/*` ids
    /// so spend, rate limits, and capped run keys behave identically across
    /// harnesses. Console-routed ids (`opencode/*`, incl. `-free`) fail here
    /// before a trial burns (free tier 403s headless under `--standalone`).
    fn resolve_model_id(model: &str) -> Result<String> {
        let m = model.trim();
        if m.starts_with("openrouter/") {
            Ok(m.to_string())
        } else {
            Err(anyhow!(
                "opencode harness is OpenRouter-only: model '{}' rejected                  (use openrouter/<provider>/<model>; opencode/* ids route via                  Console, whose free tier 403s headless runs)",
                m
            ))
        }
    }

    /// Snapshot stats for return: the reader thread counts turns in `turns`
    /// (one per `step_start` event) while token/cost accumulate in `stats`.
    /// Both must merge here — every exit path below returns this snapshot.
    /// Without the merge, `steps_taken` stays at its default 0 and every
    /// OpenCode manifest records `turns: 0` (measured 171/171 rows; OMP
    /// sets the field, so the defect is harness-correlated, not data).
    fn snapshot_stats(
        stats: &std::sync::Mutex<OmpSessionStats>,
        turns: &std::sync::atomic::AtomicU32,
        effort_effective: Option<String>,
    ) -> OmpSessionStats {
        let mut st = stats.lock().unwrap().clone();
        st.steps_taken = turns.load(Ordering::SeqCst);
        st.effort_effective = effort_effective;
        st
    }

    /// Effective effort label for the manifest: the `#variant` that ran, or
    /// `prose-only` when no variant mapped (other levels) or resolution
    /// fell back. Mirrors OMP's session-negotiated level on the other
    /// harness: requested effort lives on the row, this says what ran.
    fn resolve_effort_effective(variant_id: &Option<String>, disabled: bool) -> String {
        match (variant_id, disabled) {
            (Some(v), false) => format!("#{v}"),
            _ => "prose-only".to_string(),
        }
    }

    /// Map benchmark effort to an OpenCode model `#variant` (machine-level
    /// reasoning control, mirroring OMP's `--thinking` flag). Only the wave
    /// vocabulary (low/high/max, plus the empty/default/auto alias for
    /// high) gets variants; other levels stay prose-only (today's behavior)
    /// rather than risking unknown-variant failures on untested levels.
    fn variant_for_effort(effort: &str) -> Option<String> {
        match effort.trim().to_lowercase().as_str() {
            "low" | "high" | "max" => Some(effort.trim().to_lowercase()),
            "" | "default" | "auto" => Some("high".to_string()),
            _ => None,
        }
    }

    /// Inline `OPENCODE_CONFIG_CONTENT` defining low/high/max variants for
    /// one OpenRouter model. Default catalog variants do NOT cover our
    /// models (measured live: `#max` on ling-3.1-flash errors
    /// `provider.no-route: Variant unavailable`), so each trial carries its
    /// own definitions. Shape mirrors what the openrouter provider package
    /// generates internally for effort variants (`settings.reasoning.effort`
    /// = OpenRouter's native reasoning parameter). Env-passed so no config
    /// files pollute the candidate workspace.
    fn variant_config_json(model_bare: &str) -> String {
        // R79-4: the snapshot pin, sent through the OpenRouter provider
        // options (`provider.only`/`order`), the same endpoint the omp
        // check pins. This one function builds the per-trial config the
        // unit test reads.
        let mut entry = serde_json::json!({
            "variants": [
                {"id": "low", "settings": {"reasoning": {"effort": "low"}}},
                {"id": "high", "settings": {"reasoning": {"effort": "high"}}},
                {"id": "max", "settings": {"reasoning": {"effort": "max"}}}
            ]
        });
        if let Some(pin) = crate::cost::calculator::provider_for_model(model_bare) {
            entry["options"] = serde_json::json!({
                "provider": {"only": [pin], "order": [pin]}
            });
        }
        serde_json::json!({
            "providers": {
                "openrouter": {
                    "models": { model_bare: entry }
                }
            }
        })
        .to_string()
    }

    /// Client-side variant-resolution failure: instant, zero-spend, retryable
    /// without the suffix (prose-only effort = today's behavior). Real shape
    /// captured from a live probe — distinct from rate-limit (429 shapes)
    /// and billing (401/402) events, which must keep their own paths.
    fn is_variant_unavailable_event(v: &serde_json::Value) -> bool {
        let t = v
            .pointer("/error/type")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        t == "provider.no-route" && msg.contains("Variant unavailable")
    }

    /// The auth/billing reason of a provider error event, if any (round-13
    /// ops: opencode emits these on stdout as JSON and stderr stays empty, so
    /// the stderr classifier never sees them — a stale API key surfaced as
    /// "exited with no further detail" and burned a cell).
    fn provider_auth_billing_reason(v: &serde_json::Value) -> Option<String> {
        let t = v.pointer("/error/type").and_then(|t| t.as_str())?;
        if t != "provider.auth" && t != "provider.billing" {
            return None;
        }
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("the provider rejected the request");
        Some(format!(
            "opencode {t}: {msg} (check the API key: the repo .env is authoritative and scripts/sql-cell.sh exports it)"
        ))
    }

    /// Max cutoff recoveries per trial: cutoff → resume → cutoff again →
    /// accept the 0% as genuine model failure. Never loop on a model that
    /// won't act.
    const MAX_CUTOFF_RECOVERIES: u32 = 2;

    /// Decide cutoff recovery. Call ONLY on a clean exit-0 with limits
    /// unhit (the match arm guarantees both): zero tool calls this attempt
    /// plus an empty workspace means the turn died before acting (measured:
    /// 30k-token thought, `finish: length`, idle `succeeded`). A trial that
    /// acted (tools>0) or produced files never triggers, however it exited.
    /// Pure for unit tests; the loop feeds it live signals.
    fn should_recover_cutoff(
        exit_success: bool,
        limited: bool,
        tool_uses: u32,
        recoveries_used: u32,
        workspace_empty: bool,
    ) -> bool {
        exit_success
            && !limited
            && tool_uses == 0
            && workspace_empty
            && recoveries_used < Self::MAX_CUTOFF_RECOVERIES
    }

    /// Workspace holds candidate content: entrypoint files or anything else
    /// the agent wrote. AGENTS.md is ours (written pre-launch), never
    /// counts. Bounded walk so a pathological tree can't stall the check.
    pub(crate) fn workspace_has_content(dir: &std::path::Path) -> bool {
        if dir.join("start.sh").is_file() || dir.join("Dockerfile").is_file() {
            return true;
        }
        let mut stack = vec![dir.to_path_buf()];
        let mut seen = 0usize;
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                seen += 1;
                // Pathological: treat as content, never stall.
                if seen > 5000 {
                    return true;
                }
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if e.file_name().to_str() == Some("AGENTS.md") {
                    continue;
                }
                return true;
            }
        }
        false
    }

    /// Recovery nudge: short, tool-first. The model saw the full brief on
    /// turn 1 (then got cut off), so restate only the action contract.
    fn cutoff_nudge(port: u16) -> String {
        format!(
            "Your previous response was cut off before creating any files (no tool calls reached /workspace). Do NOT write a long plan. Immediately call the `write` and `bash` tools: create the implementation with entrypoint `./start.sh` or `Dockerfile` listening on port {port}, then test it. Brief thinking, then tools."
        )
    }

    /// R79 F1: the per-run config and data dirs, inside the run dir (the
    /// workspace's parent), never the host's home. The host's
    /// `~/.config/opencode` and `~/.local/share/opencode` hold the
    /// implementer's sessions and global config; a candidate must never
    /// see them.
    fn run_opencode_dirs(workdir: &std::path::Path) -> (PathBuf, PathBuf) {
        let run_dir = workdir.parent().unwrap_or(workdir);
        (
            run_dir.join("opencode-config"),
            run_dir.join("opencode-data"),
        )
    }

    fn prepare_run_dirs(workdir: &std::path::Path) -> (PathBuf, PathBuf) {
        let (config, data) = Self::run_opencode_dirs(workdir);
        let _ = std::fs::create_dir_all(&config);
        let _ = std::fs::create_dir_all(&data);
        (config, data)
    }

    /// The two `-v` args: the per-run dirs at the container's opencode
    /// paths. No path under the host's home appears here.
    fn opencode_mount_args(workdir: &std::path::Path) -> Vec<String> {
        let (config, data) = Self::run_opencode_dirs(workdir);
        vec![
            "-v".to_string(),
            format!("{}:/home/ubuntu/.config/opencode", config.display()),
            "-v".to_string(),
            format!("{}:/home/ubuntu/.local/share/opencode", data.display()),
        ]
    }

    fn write_agents_md(
        workdir: &std::path::Path,
        snippet: &str,
        port: u16,
        test: &str,
        effort: &str,
        worker: u8,
    ) {
        let content = include_str!("../prompts/opencode_agents.md")
            .replace("{{GUIDANCE}}", snippet)
            .replace("{{PORT}}", &port.to_string())
            .replace("{{TEST}}", test)
            .replace("{{EFFORT}}", effort);
        let content = crate::config::template_ports_for_worker(&content, port, worker);
        let _ = std::fs::write(workdir.join("AGENTS.md"), content);
    }
}

/// R79 F1: the per-run opencode dirs, removed when the run ends (every
/// return path, including errors and a cancelled trial).
struct RunDirs {
    config: PathBuf,
    data: PathBuf,
}

impl RunDirs {
    fn new(workdir: &std::path::Path) -> Self {
        let (config, data) = OpenCodeRunner::prepare_run_dirs(workdir);
        Self { config, data }
    }
}

impl Drop for RunDirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.config);
        let _ = std::fs::remove_dir_all(&self.data);
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
            worker,
        } = spec;
        let effort_level = effort.unwrap_or("high").trim().to_string();
        // Machine-level effort: `#variant` is active unless resolution fails
        // (variant_failed, set by the reader on provider.no-route), in which
        // case the loop disables it once and retries prose-only. Declared
        // here because make_cmd (below) already needs the disabled flag.
        let variant_id = Self::variant_for_effort(&effort_level);
        let variant_failed = Arc::new(AtomicBool::new(false));
        let variant_disabled = Arc::new(AtomicBool::new(variant_id.is_none()));
        // Cutoff recoveries used (cap Self::MAX_CUTOFF_RECOVERIES). Doubles
        // as the prompt selector: recovery attempts get the tool-first
        // nudge instead of the full brief.
        let cutoff_recoveries = Arc::new(std::sync::atomic::AtomicU32::new(0));
        // Fail fast on non-OpenRouter ids: no container, no spend, no session.
        let effective_model = Self::resolve_model_id(model)?;

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
        // R79 F1: fresh per-run config/data dirs; the guard removes them
        // when the run ends, so no session or key survives in the run dir.
        let _run_dirs = RunDirs::new(&canonical_workdir);

        // Same task text + entrypoint contract as OMP (shared helper).
        let (base_port, base_cmd, raw_guidance) = OmpRunner::task_guidance(spec.task, prompt);
        let target_port = crate::web::server::worker_port(base_port, worker);
        let test_cmd = crate::config::template_ports_for_worker(base_cmd, base_port, worker);
        let guidance_snippet =
            crate::config::template_ports_for_worker(raw_guidance, base_port, worker);
        let effort_note = format!("reasoning effort requested: {}", effort_level);
        let effective_prompt = include_str!("../prompts/opencode_effective.md")
            .replace("{{PROMPT}}", prompt)
            .replace("{{GUIDANCE}}", &guidance_snippet)
            .replace("{{PORT}}", &target_port.to_string())
            .replace("{{TEST}}", &test_cmd)
            .replace("{{EFFORT_NOTE}}", &effort_note);
        Self::write_agents_md(
            &canonical_workdir,
            &guidance_snippet,
            target_port,
            &test_cmd,
            &effort_level,
            worker,
        );
        // Recovery prompt: attempts after a cutoff get the tool-first nudge
        // (the model already saw the full brief on turn 1). Selected in
        // make_cmd by cutoff_recoveries.
        let recovery_nudge = Self::cutoff_nudge(target_port);

        // Model id validated above (OpenRouter-only); passed through
        // untouched so provider/model/variant strings keep working.

        // Rebuilt per attempt: `Command` is consumed by spawn, and a
        // resume re-launches the same session (same `--session` id makes
        // opencode continue where the rate-limited attempt died).
        // Egress posture identical to OMP: proxy env when filtered,
        // fail closed when the sidecar cannot start. Resolved once here so
        // resume attempts reuse the same sidecar instead of starting one
        // per attempt.
        let no_proxy = "ref-redis,ref-http,egress-proxy,localhost,127.0.0.1";
        let egress_on = crate::sandbox::docker::egress_filter_enabled();
        let proxy_url: Option<String> = (if egress_on { network } else { None })
            .and_then(|net| {
                net.strip_prefix("subdollar-net-").map(|id| {
                    crate::sandbox::docker::SandboxManager::with_id(id).start_egress_proxy()
                })
            })
            .flatten();
        if proxy_url.is_none() && egress_on {
            anyhow::bail!(
                "egress filter is enabled but the allowlist proxy sidecar failed to start; \
                 refusing to run the agent unfiltered"
            );
        }

        let make_cmd = || {
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
                .args(crate::grading::cpuset_args(
                    &crate::grading::session_cpuset(),
                ))
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
            // R79 F1: per-run config/data dirs inside the run dir; the
            // host's opencode dirs (sessions, plugins) are never mounted.
            // The key reaches the session through the environment only.
            cmd.args(Self::opencode_mount_args(&canonical_workdir));

            if let Some(net) = network {
                cmd.arg("--network").arg(net);
            } else {
                cmd.arg("--network=bridge");
            }
            if let Some(url) = proxy_url.clone() {
                for var in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                    cmd.arg("-e").arg(format!("{var}={url}"));
                }
            }
            for var in ["NO_PROXY", "no_proxy"] {
                cmd.arg("-e").arg(format!("{var}={no_proxy}"));
            }
            let effective_key = api_key
                .map(|s| s.to_string())
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok());
            if let Some(ref key) = effective_key {
                cmd.arg("-e").arg(format!("OPENROUTER_API_KEY={}", key));
            }
            // Per-trial variant definitions so `#variant` resolves for our
            // models (default catalog lacks them). Env-passed: no config
            // files in the candidate workspace.
            if variant_id.is_some() {
                let bare = effective_model
                    .strip_prefix("openrouter/")
                    .unwrap_or(&effective_model);
                cmd.arg("-e").arg(format!(
                    "OPENCODE_CONFIG_CONTENT={}",
                    Self::variant_config_json(bare)
                ));
            }
            if let Some(max_turns) = limits.max_turns {
                cmd.arg("-e")
                    .arg(format!("SDB_BENCH_MAX_TURNS={}", max_turns));
            }

            let model_arg = if variant_disabled.load(Ordering::SeqCst) {
                effective_model.clone()
            } else {
                match &variant_id {
                    Some(v) => format!("{effective_model}#{v}"),
                    None => effective_model.clone(),
                }
            };
            let prompt_arg: &str = if cutoff_recoveries.load(Ordering::SeqCst) > 0 {
                // Recovery attempt: same session, tool-first nudge.
                &recovery_nudge
            } else {
                &effective_prompt
            };
            let mut oc_args: Vec<String> = vec![
                "subdollar-sandbox".to_string(),
                "opencode".to_string(),
                "run".to_string(),
                "--standalone".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--session".to_string(),
                session_id.clone(),
                "--title".to_string(),
                format!("sdb-{container_id}"),
                "-m".to_string(),
                model_arg,
                "--auto".to_string(),
            ];
            // Round-13 R1: the prompt is positional; `--` keeps a prompt
            // that starts with `-` from being parsed as an option.
            crate::sandbox::push_prompt_args(&mut oc_args, prompt_arg);
            for a in oc_args {
                cmd.arg(a);
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd
        };

        // Resume loop: a provider rate-limit kills one `opencode run`
        // process but not the trial. Relaunching with the same `--session`
        // continues it; stats/turns/spend accumulate across attempts under
        // the global limits. Billing/auth trips and hard limits never resume.
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let timeout = limits
            .timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1800));
        let deadline = start_time + timeout;
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
            "[OPENCODE-PROMPT] model={} effort={} ({}) cwd=/workspace session={}",
            effective_model,
            effort_level,
            variant_id
                .as_ref()
                .map(|v| format!("machine variant #{v}"))
                .unwrap_or_else(|| "prose-only".to_string()),
            container_id
        ));

        // Stats accumulate across resume attempts under the global limits.
        let stats = Arc::new(std::sync::Mutex::new(OmpSessionStats::default()));
        let turns = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let spent = Arc::new(std::sync::Mutex::new(0.0f64));
        let limit_hit = Arc::new(AtomicBool::new(false));
        let limit_msg = Arc::new(std::sync::Mutex::new(None::<String>));
        let billing_tripped = Arc::new(AtomicBool::new(false));

        // Logger pump + timeout/cancel supervision (mirrors OMP structure).
        // One pump for all attempts; per-attempt readers hold tx clones.
        // The main `tx` stays alive for per-attempt clones and is dropped
        // at the very end: dropping it earlier deadlocks the final join
        // (rx never closes while a sender lives).
        let retry_tx = tx.clone();
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

        // Resume loop: a provider rate-limit kills one `opencode run`
        // process but not the trial. Relaunching with the same `--session`
        // continues it; stats/turns/spend accumulate across attempts under
        // the global limits. Billing/auth trips and hard limits never resume.
        const MAX_RESUMES: u32 = 5;
        let mut attempt: u32 = 0;
        let outcome: Result<OmpSessionStats> = loop {
            let rate_limited = Arc::new(AtomicBool::new(false));
            let mut ready_fired = false;
            let attempt_tail = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            // Tool calls THIS attempt (fresh per iteration: no reset races
            // with the previous reader). Feeds the cutoff-recovery trigger.
            let attempt_tools = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let mut child = make_cmd()
                .spawn()
                .map_err(|e| anyhow!("Failed to spawn opencode in docker: {}", e))?;

            // Reader: NDJSON `--format json` stream -> bench log protocol.
            // step_start = turn, tool_use = [ACTION], text = [RESPONSE],
            // step_finish = token/cost accumulation + [TELEMETRY].
            // A `type:"error"` event with a rate-limit shape sets the
            // resume flag (handled in the `_` arm below).
            let reader_handle = if let Some(stdout) = child.stdout.take() {
                let cost_model = effective_model.clone();
                let tx_o = tx.clone();
                let stats_o = stats.clone();
                let turns_o = turns.clone();
                let tools_o = attempt_tools.clone();
                let variant_failed_o = variant_failed.clone();
                let spent_o = spent.clone();
                let limit_hit_o = limit_hit.clone();
                let limit_msg_o = limit_msg.clone();
                let billing_o = billing_tripped.clone();
                let rate_o = rate_limited.clone();
                let running_o = running.clone();
                let max_turns = limits.max_turns.unwrap_or(0);
                let max_budget = limits.max_budget_usd.unwrap_or(f64::MAX);
                let cname = container_name.clone();
                Some(std::thread::spawn(move || {
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
                                if max_turns > 0 && n > max_turns {
                                    // Round-14 N4: the turn past the cap is
                                    // killed at its start; it never ran, so
                                    // it is not counted (it used to cost
                                    // the repair round one turn).
                                    turns_o.fetch_sub(1, Ordering::SeqCst);
                                    *limit_msg_o.lock().unwrap() =
                                        Some(format!("max turns {} reached", max_turns));
                                    limit_hit_o.store(true, Ordering::SeqCst);
                                    running_o.store(false, Ordering::SeqCst);
                                    let _ =
                                        Command::new("docker").args(["rm", "-f", &cname]).output();
                                    break;
                                }
                                let _ = tx_o.send(format!("[LIVE] Turn {} started", n));
                            }
                            "tool_use" => {
                                tools_o.fetch_add(1, Ordering::SeqCst);
                                let tool = v
                                    .pointer("/part/tool")
                                    .or_else(|| v.get("tool"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("tool");
                                let _ = tx_o.send(format!(
                                    "[ACTION] {} {}",
                                    tool,
                                    Self::cap_line(trimmed)
                                ));
                            }
                            "text" => {
                                let text = v
                                    .pointer("/part/text")
                                    .or_else(|| v.get("text"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or(trimmed);
                                // R77 F1: a retried 429 is transient; only a
                                // fault trips the billing stop.
                                if let Some(crate::sandbox::omp::BillingSignal::Fault(reason)) =
                                    crate::sandbox::omp::classify_billing_error(trimmed).or_else(
                                        || crate::sandbox::omp::classify_billing_error(text),
                                    )
                                {
                                    let _ = tx_o.send(format!("[BILLING] {}", reason));
                                    billing_o.store(true, Ordering::SeqCst);
                                }
                                if Self::is_rate_limit_event(&v, text) {
                                    rate_o.store(true, Ordering::SeqCst);
                                }
                                let _ = tx_o.send(format!("[RESPONSE] {}", Self::cap_line(text)));
                            }
                            "step_finish" => {
                                let tokens = v
                                    .pointer("/part/tokens")
                                    .or_else(|| v.get("tokens"))
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                let step_cost = Self::add_step(
                                    &mut stats_o.lock().unwrap(),
                                    &tokens,
                                    &cost_model,
                                );
                                *spent_o.lock().unwrap() += step_cost;
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
                                    let _ =
                                        Command::new("docker").args(["rm", "-f", &cname]).output();
                                    break;
                                }
                            }
                            _ => {
                                if Self::is_variant_unavailable_event(&v) {
                                    variant_failed_o.store(true, Ordering::SeqCst);
                                    let _ = tx_o.send(
                                        "[VARIANT] model variant unavailable; will retry prose-only"
                                            .to_string(),
                                    );
                                }
                                if Self::is_rate_limit_event(&v, trimmed) {
                                    rate_o.store(true, Ordering::SeqCst);
                                }
                                if let Some(reason) = Self::provider_auth_billing_reason(&v) {
                                    let _ = tx_o.send(format!("[BILLING] {reason}"));
                                    billing_o.store(true, Ordering::SeqCst);
                                }
                                let _ = tx_o.send(format!("[LOG] {}", Self::cap_line(trimmed)));
                            }
                        }
                    }
                }))
            } else {
                None
            };
            // Drain stderr for billing phrases (provider errors surface
            // there). The tail joins the failure message when the agent
            // exits non-zero, so a dead trial names its cause (auth, 429, …)
            // instead of a generic message.
            let stderr_handle = if let Some(stderr) = child.stderr.take() {
                let tx_e = tx.clone();
                let billing_e = billing_tripped.clone();
                let rate_e = rate_limited.clone();
                let tail_e = attempt_tail.clone();
                Some(std::thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        // R79 F2: the health counter reads the opencode
                        // stderr too (a retried 429 is counted, not a fault).
                        let _ = tx_e.send(format!("[STDERR] {}", trimmed));
                        {
                            let mut tail = tail_e.lock().unwrap();
                            tail.push(trimmed.to_string());
                            if tail.len() > 20 {
                                tail.remove(0);
                            }
                        }
                        // R77 F1: a retried 429 is transient; only a
                        // fault trips the billing stop.
                        if let Some(crate::sandbox::omp::BillingSignal::Fault(reason)) =
                            crate::sandbox::omp::classify_billing_error(trimmed)
                        {
                            let _ = tx_e.send(format!("[BILLING] {}", reason));
                            billing_e.store(true, Ordering::SeqCst);
                        }
                        if Self::is_rate_limit_event(&serde_json::Value::Null, trimmed) {
                            rate_e.store(true, Ordering::SeqCst);
                        }
                    }
                }))
            } else {
                None
            };

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
                        if crate::sandbox::omp::take_ready_signal(&canonical_workdir) {
                            let _ = tx.send("[LIVE] Model signaled completion via /workspace/READY! Triggering verification...".to_string());
                            running.store(false, Ordering::SeqCst);
                            ready_fired = true;
                            let _ = Command::new("docker")
                                .args(["rm", "-f", &container_name])
                                .output();
                            break None;
                        }
                        if timed_out || cancelled || !running.load(Ordering::SeqCst) {
                            let _ = Command::new("docker")
                                .args(["rm", "-f", &container_name])
                                .output();
                            break None;
                        }
                        // Campaign cap headroom: the compute floor grows
                        // between calls, so check it in the wait loop rather
                        // than only on a step event (round-17 P0).
                        if let Some(max_eff) = limits.max_effective_usd {
                            let st = Self::snapshot_stats(
                                &stats,
                                &turns,
                                Some(Self::resolve_effort_effective(
                                    &variant_id,
                                    variant_disabled.load(Ordering::SeqCst),
                                )),
                            );
                            let elapsed = SystemTime::now()
                                .duration_since(start_time)
                                .map(|d| d.as_secs_f64())
                                .unwrap_or(0.0);
                            let total = st.prompt_tokens + st.completion_tokens + st.cached_tokens;
                            if let Some(msg) = crate::sandbox::omp::cap_headroom_reason(
                                max_eff,
                                *spent.lock().unwrap(),
                                elapsed,
                                total,
                                st.cached_tokens,
                            ) {
                                let _ = tx.send(msg.clone());
                                *limit_msg.lock().unwrap() = Some(msg);
                                limit_hit.store(true, Ordering::SeqCst);
                                running.store(false, Ordering::SeqCst);
                                let _ = Command::new("docker")
                                    .args(["rm", "-f", &container_name])
                                    .output();
                                break None;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break None,
                }
            };
            let _ = child.wait();
            if let Some(h) = reader_handle {
                let _ = h.join();
            }
            if let Some(h) = stderr_handle {
                let _ = h.join();
            }
            // Name is freed for the relaunch (`--rm` auto-removes exited
            // containers, but a killed one can linger briefly).
            let _ = Command::new("docker")
                .args(["rm", "-f", &container_name])
                .output();
            if billing_tripped.load(Ordering::SeqCst) {
                let st = Self::snapshot_stats(
                    &stats,
                    &turns,
                    Some(Self::resolve_effort_effective(
                        &variant_id,
                        variant_disabled.load(Ordering::SeqCst),
                    )),
                );
                // Same envelope as OMP: queue.rs pauses the wave on "billing:".
                break Err(anyhow::Error::new(OmpBillingError {
                    reason:
                        "opencode provider auth/billing tripped mid-session (see [BILLING] log)"
                            .to_string(),
                    partial_stats: st,
                }));
            }
            let limited = limit_hit.load(Ordering::SeqCst);
            let timed_out = SystemTime::now() > deadline;
            let cancelled = cancel_flag
                .as_ref()
                .map(|f| f.load(Ordering::SeqCst))
                .unwrap_or(false);
            let msg = limit_msg.lock().unwrap().clone().unwrap_or_default();
            let mut st = Self::snapshot_stats(
                &stats,
                &turns,
                Some(Self::resolve_effort_effective(
                    &variant_id,
                    variant_disabled.load(Ordering::SeqCst),
                )),
            );
            let tail = attempt_tail.lock().unwrap().join(" | ");
            // Snapshot the throttle-wall signal once: fixed for this
            // iteration (per-attempt flag + whole-session token/tool counts).
            let throttled_noop = Self::is_throttled_noop(
                rate_limited.load(Ordering::SeqCst),
                st.prompt_tokens + st.completion_tokens + st.cached_tokens,
                attempt_tools.load(Ordering::SeqCst),
            );
            match waited {
                // Deadline/cancel first: when the wall clock ran out, that is
                // the operative fact — a simultaneously-set throttle flag
                // must not drive a resume whose backoff would be cut short.
                _ if timed_out || cancelled => {
                    st.stop_reason = Some(if cancelled { "crash" } else { "wall" }.to_string());
                    break Err(anyhow!(OmpExecutionError {
                        message: if cancelled {
                            "opencode run cancelled by user".to_string()
                        } else {
                            "opencode timeout reached".to_string()
                        },
                        started: st.steps_taken > 0 || st.total_tokens > 0,
                        partial_stats: st,
                    }))
                }
                // Provider throttle with zero model output is not a model
                // verdict, whatever the exit code: resume while attempts
                // remain, then fail loudly in the OMP billing envelope so
                // the trial is a non-result (neutral fault, excluded from
                // aggregates), never a ranked 0% that reads as a capability
                // result. Must be matched before the success arm: opencode's
                // retry ladder exits 0 on a quota wall.
                _ if throttled_noop && attempt < MAX_RESUMES => {
                    let wait_s = (60u64.saturating_mul(1u64 << attempt.min(4))).min(900);
                    let _ = retry_tx.send(format!(
                        "[RETRY] provider rate-limited with zero model output (attempt {}/{}); backing off {}s then resuming session {}",
                        attempt + 1,
                        MAX_RESUMES + 1,
                        wait_s,
                        session_id
                    ));
                    if !Self::backoff_sleep(attempt, deadline, &cancel_flag) {
                        break Err(anyhow!(OmpExecutionError {
                            message: "opencode backoff interrupted (timeout/cancel)".to_string(),
                            started: st.steps_taken > 0 || st.total_tokens > 0,
                            partial_stats: st,
                        }));
                    }
                    attempt += 1;
                    continue;
                }
                _ if throttled_noop => {
                    break Err(anyhow::Error::new(OmpBillingError {
                        reason: format!(
                            "opencode provider rate-limited for all {} attempts with zero model output (quota/throttle); trial recorded as a non-result",
                            MAX_RESUMES + 1
                        ),
                        partial_stats: st,
                    }))
                }
                Some(status) if status.success() && !limited => {
                    // Cutoff recovery: a clean exit that never acted (0
                    // tools, empty workspace) is a truncated turn, not a
                    // verdict — resume the same session with the tool-first
                    // nudge. Capped (MAX_CUTOFF_RECOVERIES); past the cap the
                    // 0% stands as genuine model failure. No new status, no
                    // rescoring path: recovered trials score normally.
                    if Self::should_recover_cutoff(
                        true,
                        false,
                        attempt_tools.load(Ordering::SeqCst),
                        cutoff_recoveries.load(Ordering::SeqCst),
                        !Self::workspace_has_content(&canonical_workdir),
                    ) {
                        let n = cutoff_recoveries.fetch_add(1, Ordering::SeqCst) + 1;
                        let _ = retry_tx.send(format!(
                            "[CUTOFF-RECOVER] exit 0 with 0 tool calls and empty workspace; resuming same session with tool-first nudge ({n}/{})",
                            Self::MAX_CUTOFF_RECOVERIES
                        ));
                        continue;
                    }
                    break Ok(st);
                }
                _ if limited => {
                    // Parity with OMP (stopped_by_limit returns Ok): hitting
                    // max turns/budget ends the run normally with partial
                    // stats. A 100% verified candidate then scores completed
                    // (see pipeline status chain); cost still prices the run.
                    let _ = msg;
                    break Ok(st);
                }
                _ if variant_failed.load(Ordering::SeqCst)
                    && !variant_disabled.load(Ordering::SeqCst) =>
                {
                    // Client-side variant-resolution failure: instant and
                    // zero-spend. Retry once without the `#suffix`
                    // (prose-only effort = today's behavior). The disabled
                    // flag bounds this to a single extra attempt; the retry
                    // is deterministic to proceed (plain `-m` resolves).
                    variant_disabled.store(true, Ordering::SeqCst);
                    let _ = retry_tx.send(
                        "[VARIANT] variant unavailable; resuming prose-only (no #suffix)"
                            .to_string(),
                    );
                    continue;
                }
                _ if rate_limited.load(Ordering::SeqCst) && attempt < MAX_RESUMES => {
                    let wait_s = (60u64.saturating_mul(1u64 << attempt.min(4))).min(900);
                    let _ = retry_tx.send(format!(
                        "[RETRY] provider rate-limited (attempt {}/{}); backing off {}s then resuming session {}",
                        attempt + 1,
                        MAX_RESUMES + 1,
                        wait_s,
                        session_id
                    ));
                    if !Self::backoff_sleep(attempt, deadline, &cancel_flag) {
                        break Err(anyhow!(OmpExecutionError {
                            message: "opencode backoff interrupted (timeout/cancel)".to_string(),
                            started: st.steps_taken > 0 || st.total_tokens > 0,
                            partial_stats: st,
                        }));
                    }
                    attempt += 1;
                    continue;
                }
                _ if ready_fired => {
                    // Model-signaled completion scores exactly like a normal
                    // exit (omp parity): verify as-is, so an empty workspace
                    // records failed_tests, not a harness fault. Placed after
                    // the rate-limit arm so a simultaneous throttle still
                    // resumes instead of concluding on possibly-cut output.
                    break Ok(st);
                }
                _ => {
                    break Err(anyhow!(OmpExecutionError {
                        message: {
                            let code = waited
                                .map(|s| {
                                    s.code()
                                        .map(|c| c.to_string())
                                        .unwrap_or_else(|| "signal".to_string())
                                })
                                .unwrap_or_else(|| "no-exit".to_string());
                            match tail.is_empty() {
                                true => format!(
                                    "opencode agent exited (code {}) with no further detail",
                                    code
                                ),
                                false => format!(
                                    "opencode agent exited (code {}): {}",
                                    code,
                                    Self::cap_line(&tail)
                                ),
                            }
                        },
                        started: st.steps_taken > 0 || st.total_tokens > 0,
                        partial_stats: st,
                    }))
                }
            }
        };
        running.store(false, Ordering::SeqCst);
        drop(tx);
        drop(retry_tx);
        let _ = pump_handle.join();
        let _ = Command::new("docker")
            .args(["rm", "-f", &container_name])
            .output();

        // AGENTS.md is runner-owned scaffolding, never candidate code.
        let _ = std::fs::remove_file(canonical_workdir.join("AGENTS.md"));

        // R23 D1/D5: classify the normal end; the error paths above set
        // wall/crash where they build their partial stats.
        outcome.map(|mut st| {
            if st.stop_reason.is_none() {
                st.stop_reason = Some(
                    if limit_hit.load(Ordering::SeqCst) {
                        let msg = limit_msg
                            .lock()
                            .ok()
                            .and_then(|m| m.clone())
                            .unwrap_or_default();
                        if msg.contains("max turns") {
                            "turns"
                        } else if msg.contains("budget") {
                            "dollars"
                        } else {
                            "crash"
                        }
                    } else {
                        "ready"
                    }
                    .to_string(),
                );
            }
            st
        })
    }
}

#[cfg(test)]
mod tests;
