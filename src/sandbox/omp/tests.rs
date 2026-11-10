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
    vec![TaskType::Sql]
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
            for banned in ["150 words", "sdb-redis", "redis-server"] {
                assert!(
                    !content.contains(banned),
                    "prompt still contains {banned:?} (task {task})"
                );
            }
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

/// R67 F1: the omp model argument carries the pinned endpoint suffix, and
/// an unpinned model is passed through unchanged.
#[test]
fn r67_f1_cell_pins_provider() {
    let pinned = omp_task_args(
        "p",
        "openrouter/openai/gpt-6-luna",
        "/tmp/s",
        None,
        None,
        "high",
    );
    assert!(
        pinned
            .iter()
            .any(|a| a == "--model=openrouter/openai/gpt-6-luna@openai"),
        "the paid cell must pin openai: {pinned:?}"
    );
    let plain = omp_task_args(
        "p",
        "openrouter/covtest/model",
        "/tmp/s",
        None,
        None,
        "high",
    );
    assert!(
        plain
            .iter()
            .any(|a| a == "--model=openrouter/covtest/model"),
        "an unpinned test model stays plain: {plain:?}"
    );
}

/// R67 F2: the session's per-message `usage.cost.total` sums into
/// `billed_usd`, beside the list-price `cost_usd`.
#[test]
fn r67_f2_billed_from_usage_cost() {
    let dir = std::env::temp_dir().join(format!("r67_billed_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.jsonl");
    let content = concat!(
        "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":10,\"output\":2,\"cost\":{\"total\":0.0004}}}}\n",
        "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":10,\"output\":2,\"cost\":{\"total\":0.0006}}}}\n"
    );
    std::fs::write(&path, content).unwrap();
    let stats = OmpRunner::extract_latest_session_stats(Some(&path)).unwrap();
    assert!(
        (stats.billed_usd - 0.001).abs() < 1e-12,
        "{}",
        stats.billed_usd
    );
    assert!((stats.cost_usd - 0.001).abs() < 1e-12, "{}", stats.cost_usd);
    let _ = std::fs::remove_dir_all(&dir);
}

/// R65 F5: prompt tokens come from the field each provider route sends:
/// deepseek/OpenAI chat (`prompt_tokens`), Anthropic (`input_tokens`), and
/// OpenAI's Responses shape (`input_tokens` + `input_tokens_details`).
#[test]
fn r65_f5_prompt_tokens_every_route() {
    let temp_dir = std::env::temp_dir().join(format!("r65_tokens_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    let cases = [
        (
            "deepseek",
            r#"{"message":{"role":"assistant","usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":40}}}}"#,
            100u64,
            40u64,
            20u64,
            0u64,
        ),
        (
            "anthropic",
            r#"{"message":{"role":"assistant","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":40,"cache_creation_input_tokens":10}}}"#,
            100,
            40,
            20,
            10,
        ),
        (
            "openai",
            r#"{"message":{"role":"assistant","usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":40}}}}"#,
            100,
            40,
            20,
            0,
        ),
    ];
    for (name, line, prompt, cached, completion, cache_write) in cases {
        let path = temp_dir.join(format!("{name}.jsonl"));
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let stats = OmpRunner::extract_latest_session_stats(Some(&path)).unwrap();
        assert_eq!(stats.prompt_tokens, prompt, "{name} prompt");
        assert_eq!(stats.cached_tokens, cached, "{name} cached");
        assert_eq!(stats.completion_tokens, completion, "{name} completion");
        assert_eq!(stats.cache_write_tokens, cache_write, "{name} cache write");
        assert_eq!(
            stats.total_tokens,
            prompt + cached + completion + cache_write,
            "{name} total"
        );
    }
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
        stall_after: tool_stall_after(),
        stall: Arc::new(Mutex::new(None)),
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
            stall_after: tool_stall_after(),
            stall: Arc::new(Mutex::new(None)),
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
    let temp_dir = std::env::temp_dir().join(format!("test_tail_limits_{}", std::process::id()));
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
        stall_after: tool_stall_after(),
        stall: Arc::new(Mutex::new(None)),
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
    let temp_dir = std::env::temp_dir().join(format!("test_tail_derived_{}", std::process::id()));
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
        stall_after: tool_stall_after(),
        stall: Arc::new(Mutex::new(None)),
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
    assert!(c(r#"{"error":{"code":429}}"#)
        .unwrap()
        .reason()
        .contains("429"));
    assert!(c(r#"{"error":"payment_required"}"#)
        .unwrap()
        .reason()
        .contains("402"));
    assert!(c(r#"{"error":"insufficient_credit"}"#)
        .unwrap()
        .reason()
        .contains("402"));
    assert!(c(r#"{"message":"payment required"}"#)
        .unwrap()
        .reason()
        .contains("402"));
    assert!(c(r#"{"message":"credit exhausted"}"#)
        .unwrap()
        .reason()
        .contains("402"));
    assert!(c(r#"{"message":"out of credit"}"#)
        .unwrap()
        .reason()
        .contains("402"));
    assert!(c(r#"{"message":"rate limit exceeded"}"#)
        .unwrap()
        .reason()
        .contains("429"));
    assert!(c(r#"{"message":"too many requests"}"#)
        .unwrap()
        .reason()
        .contains("429"));
    assert!(c(r#"{"message":"unauthorized api key"}"#)
        .unwrap()
        .reason()
        .contains("401"));
    assert!(c(r#"{"message":"unauthorized token"}"#)
        .unwrap()
        .reason()
        .contains("401"));
    assert!(c("status: 401 bad token").unwrap().reason().contains("401"));
    assert!(c("unauthorized token supplied")
        .unwrap()
        .reason()
        .contains("401"));
    assert!(c("status 402").unwrap().reason().contains("402"));
    assert!(c("HTTP/2 429").unwrap().reason().contains("429"));
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
    let other = std::env::temp_dir().join(format!("sdb_disc_renamed_other_{}", std::process::id()));
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
    let ev = r#"[BENCH-EVENT] {"v":1,"kind":"tool_result","tool":"bash","ok":true,"output":""}"#;
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
    let line =
        r#"{"message":{"role":"assistant","usage":{},"content":[{"type":"text","text":"hi"}]}}"#;
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

/// R56 F1: the hung fixture's tool start never gets a result; the
/// stall guard ends the session within 5 s and records the command.
#[test]
fn r56_f1_hung_tool_ends_session() {
    let dir = std::env::temp_dir().join(format!("r56_hung_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("hung.jsonl");
    std::fs::write(
        &session,
        include_str!("../fixtures/hung-tool-session-tail.jsonl"),
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
    ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
    ctx.stall_after = Duration::from_secs(2);
    let stall = ctx.stall.clone();
    let handle = thread::spawn(move || OmpRunner::tail_session_file(ctx));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut line = None;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(l) if l.contains("[STALL]") => {
                line = Some(l);
                break;
            }
            _ => {}
        }
    }
    running.store(false, Ordering::SeqCst);
    let _ = handle.join();
    let line = line.expect("stall line within 5 s");
    assert!(line.contains("bash"), "{line}");
    let record = stall.lock().unwrap().clone().expect("stall record");
    assert!(
        record
            .command
            .starts_with("cd /workspace && rm -rf /tmp/sdbdata"),
        "{}",
        record.command
    );
    assert!(!record.started_at.is_empty() && !record.ended_at.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// R56 F1: the capped fixture's tool result arrives; no stall is
/// recorded even with the same low threshold.
#[test]
fn r56_f1_returned_tool_is_not_a_stall() {
    let dir = std::env::temp_dir().join(format!("r56_capped_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("capped.jsonl");
    std::fs::write(
        &session,
        include_str!("../fixtures/capped-tool-session-excerpt.jsonl"),
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
    ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
    ctx.stall_after = Duration::from_secs(2);
    let stall = ctx.stall.clone();
    let handle = thread::spawn(move || OmpRunner::tail_session_file(ctx));
    std::thread::sleep(Duration::from_millis(2500));
    let mut saw_stall = false;
    while let Ok(l) = rx.recv_timeout(Duration::from_millis(50)) {
        if l.contains("[STALL]") {
            saw_stall = true;
        }
    }
    running.store(false, Ordering::SeqCst);
    let _ = handle.join();
    assert!(!saw_stall, "a returned tool must not stall");
    assert!(stall.lock().unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// R56 F2: the heartbeat names an in-flight tool, and returns to the
/// reasoning text after its toolResult.
#[test]
fn r56_f2_heartbeat_names_running_tool() {
    let dir = std::env::temp_dir().join(format!("r56_heartbeat_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("hung.jsonl");
    std::fs::write(
        &session,
        include_str!("../fixtures/hung-tool-session-tail.jsonl"),
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let (mut ctx, _active) = tailer_ctx(dir.clone(), tx, running.clone());
    ctx.active_file = Arc::new(Mutex::new(Some(session.clone())));
    ctx.stall_after = Duration::from_secs(600);
    let handle = thread::spawn(move || OmpRunner::tail_session_file(ctx));

    let mut saw_tool = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) if l.contains("[WAITING]") && l.contains("tool running (bash:") => {
                saw_tool = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_tool, "heartbeat must name the running tool");

    let id = "call_dc2a2e3f6cb3497586372f53cd42b46c|fc_tmp_nmbq1345ytb";
    let result = format!(
        r#"{{"type":"message","message":{{"role":"toolResult","toolCallId":"{id}","toolName":"bash","content":[{{"type":"text","text":"done"}}]}}}}"#
    );
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&session)
            .unwrap();
        writeln!(f, "{result}").unwrap();
    }
    let mut saw_reasoning = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) if l.contains("[WAITING]") && l.contains("reasoning") => {
                saw_reasoning = true;
                break;
            }
            _ => {}
        }
    }
    running.store(false, Ordering::SeqCst);
    let _ = handle.join();
    assert!(saw_reasoning, "heartbeat must return to reasoning");
    let _ = std::fs::remove_dir_all(&dir);
}

/// R57 F2: the [STALL] marker wins even when the command contains
/// the turn/budget limit words.
#[test]
fn r57_f2_stall_reason_ignores_command() {
    let reason =
            "[STALL] bash ran 600 s without returning (echo turn limit; echo budget cap); ending the session";
    assert_eq!(stop_reason_from(reason), "wall");
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
      timeout)
        seed_turn_session
        wait_for_kill
        ;;
      timeout_no_turn)
        wait_for_kill
        ;;
      cancel)
        wait_for_kill
        ;;
      model_not_found_wait)
        printf '%s\n' 'Model "openrouter/covtest-model" not found' >&2
        wait_for_kill
        ;;
      max_turns)
        seed_turn_session
        wait_for_kill
        ;;
      budget)
        if [ -n "$sessions" ]; then
          mkdir -p "$sessions"
          printf '%s\n' '{"message":{"role":"assistant","usage":{"input":250000,"output":0},"content":[{"type":"text","text":"over budget"}]}}' > "$sessions/session.jsonl"
        fi
        wait_for_kill
        ;;
      cap)
        if [ -n "$sessions" ]; then
          mkdir -p "$sessions"
          {
            printf '%s\n' '{"message":{"role":"assistant","usage":{"input":50000,"output":0},"content":[{"type":"text","text":"one"}]}}'
            sleep 0.15
            printf '%s\n' '{"message":{"role":"assistant","usage":{"input":150000,"output":0},"content":[{"type":"text","text":"two"}]}}'
          } > "$sessions/session.jsonl"
        fi
        wait_for_kill
        ;;
      billing)
        printf '%s\n' 'HTTP/1.1 402 Payment Required' >&2
        sleep 0.5
        exit 0
        ;;
      billing_openrouter)
        printf '%s\n' "402 This request requires more credits, or fewer max_tokens. You requested up to 65536 tokens, but can only afford 44804. To increase, visit https://openrouter.ai/workspaces/default/keys/REDACTED and adjust the key's total limit" >&2
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
        assert_eq!(st.stop_reason.as_deref(), Some("ready"));
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
        assert_eq!(exec.partial_stats.stop_reason.as_deref(), Some("crash"));
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
        assert_eq!(exec.partial_stats.stop_reason.as_deref(), Some("crash"));
        passed += 1;
    }

    // 3. READY signal: reaped and scored like a normal exit.
    {
        let (res, logs, _, _) = run_omp_scenario("ready", &root, covtest_limits(10, 1.0, 10), None);
        let st = res.expect("READY conclusion must be Ok");
        assert_eq!(st.stop_reason.as_deref(), Some("ready"));
        assert!(logs
            .iter()
            .any(|l| l.contains("signaled completion via /workspace/READY")));
        passed += 1;
    }

    // 4. Watchdog timeout after a turn: reaped and concluded
    //    (stopped_by_limit).
    {
        let (res, logs, _, _) =
            run_omp_scenario("timeout", &root, covtest_limits(10, 1.0, 1), None);
        let st = res.expect("watchdog stop after a turn is a normal conclusion");
        assert_eq!(st.stop_reason.as_deref(), Some("wall"));
        assert!(logs.iter().any(|l| l.contains("Watchdog")));
        passed += 1;
    }

    // 4a. R68 F4: a watchdog stop with no turn is a harness fault, never
    // an Ok session.
    {
        let (res, logs, _, _) =
            run_omp_scenario("timeout_no_turn", &root, covtest_limits(10, 1.0, 1), None);
        let err = res.expect_err("a watchdog stop with no turn must be an error");
        let exec = err
            .downcast_ref::<OmpExecutionError>()
            .expect("OmpExecutionError");
        assert!(!exec.started, "the agent never started");
        assert!(exec.message.contains("never started"), "{}", exec.message);
        assert!(logs.iter().any(|l| l.contains("Watchdog")));
        passed += 1;
    }

    // 4b. R68 F4: omp 18.4.2 prints the model-not-found text and then
    // waits for the watchdog. A run that never started is an error, not
    // an Ok watchdog stop.
    {
        let (res, logs, _, _) = run_omp_scenario(
            "model_not_found_wait",
            &root,
            covtest_limits(10, 1.0, 1),
            None,
        );
        let err = res.expect_err("a model-not-found wait must be an error");
        let exec = err
            .downcast_ref::<OmpExecutionError>()
            .expect("OmpExecutionError");
        assert!(!exec.started, "the agent never started");
        assert!(exec.message.contains("not found"), "{}", exec.message);
        assert_eq!(exec.partial_stats.stop_reason.as_deref(), Some("crash"));
        assert!(logs.iter().any(|l| l.contains("not found")));
        passed += 1;
    }

    // 5. Cancel flag: reaped at the first loop turn.
    {
        let cancel = Arc::new(AtomicBool::new(true));
        let (res, logs, _, _) =
            run_omp_scenario("cancel", &root, covtest_limits(10, 1.0, 10), Some(cancel));
        let st = res.expect("cancel conclusion is a normal stop");
        assert_eq!(st.stop_reason.as_deref(), Some("crash"));
        assert!(logs.iter().any(|l| l.contains("Cancellation requested")));
        passed += 1;
    }

    // 6. Max turns: the tailer trips the limit and the run stops Ok.
    {
        let (res, logs, _, _) =
            run_omp_scenario("max_turns", &root, covtest_limits(1, 1.0, 10), None);
        let st = res.expect("turn limit stop is a normal conclusion");
        assert_eq!(st.stop_reason.as_deref(), Some("turns"));
        assert!(logs.iter().any(|l| l.contains("Reached turn limit")));
        passed += 1;
    }

    // 7a. API budget cap: a normal stop, recorded as dollars.
    {
        let (res, logs, _, _) =
            run_omp_scenario("budget", &root, covtest_limits(10, 0.01, 10), None);
        let st = res.expect("budget stop is a normal conclusion");
        assert_eq!(st.stop_reason.as_deref(), Some("dollars"));
        assert!(logs.iter().any(|l| l.contains("Reached budget cap")));
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
        assert_eq!(st.stop_reason.as_deref(), Some("dollars"));
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

    // 8b. R65 F1/F2: OpenRouter's exact 402 text on stderr trips the wire
    // through the real runner, and the log names the billing reason.
    {
        let (res, logs, _, _) = run_omp_scenario(
            "billing_openrouter",
            &root,
            covtest_limits(10, 1.0, 10),
            None,
        );
        let err = res.expect_err("OpenRouter 402 must trip");
        assert!(
            err.downcast_ref::<OmpBillingError>().is_some(),
            "a billing trip is OmpBillingError"
        );
        assert!(
            logs.iter()
                .any(|l| l.contains("[STDERR] 402 This request requires more credits")),
            "the outage line reaches the log: {logs:?}"
        );
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
