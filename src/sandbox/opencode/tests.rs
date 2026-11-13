use super::*;

fn ev(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
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

fn r79_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "r79-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn r79_walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// R79 F1: the container mounts the per-run opencode dirs, never the
/// host's `~/.config/opencode` or `~/.local/share/opencode`.
#[test]
fn r79_f1_no_host_opencode_mount() {
    let base = r79_temp_dir("mount");
    let workdir = base.join("workspace");
    std::fs::create_dir_all(&workdir).unwrap();
    let args = OpenCodeRunner::opencode_mount_args(&workdir);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
    let host_cfg = PathBuf::from(&home).join(".config/opencode");
    let host_data = PathBuf::from(&home).join(".local/share/opencode");
    let mut mounts = 0;
    for a in &args {
        if a.starts_with('-') {
            continue;
        }
        let host = PathBuf::from(a.split(':').next().unwrap_or(""));
        assert!(
            !host.starts_with(&host_cfg),
            "the host config dir is mounted: {a}"
        );
        assert!(
            !host.starts_with(&host_data),
            "the host data dir is mounted: {a}"
        );
        assert!(
            host.starts_with(&base),
            "an opencode mount is outside the run dir: {a}"
        );
        mounts += 1;
    }
    assert_eq!(mounts, 2, "the config and data mounts: {args:?}");
    let _ = std::fs::remove_dir_all(&base);
}

/// R79 F1: the per-run dirs are inside the run dir, hold no key, and are
/// removed when the run ends.
#[test]
fn r79_f1_run_dir_holds_no_key() {
    let base = r79_temp_dir("nokey");
    let workdir = base.join("workspace");
    std::fs::create_dir_all(&workdir).unwrap();
    let key = "sk-r79-sentinel-key";
    // R79-11: the runner's real key argument is the only place the key
    // appears; the mount args never carry it.
    let env = OpenCodeRunner::key_env_args(key);
    assert_eq!(
        env,
        vec!["-e".to_string(), format!("OPENROUTER_API_KEY={key}")],
        "the key's only docker-run argument"
    );
    let args = OpenCodeRunner::opencode_mount_args(&workdir);
    assert!(
        !args.iter().any(|a| a.contains(key)),
        "a mount arg carries the key: {args:?}"
    );
    {
        let dirs = RunDirs::new(&workdir);
        assert!(dirs.config.starts_with(&base), "config outside the run dir");
        assert!(dirs.data.starts_with(&base), "data outside the run dir");
        std::fs::write(workdir.join("main.go"), "package main\n").unwrap();
        for path in r79_walk(&base) {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                !text.contains(key),
                "the key reached a run-dir file: {}",
                path.display()
            );
        }
    }
    assert!(
        !base.join("opencode-config").exists() && !base.join("opencode-data").exists(),
        "the per-run opencode dirs survived the run"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// R79 F1 (live, Docker): a sentinel in the host's opencode data dir is
/// not readable from the candidate's container through the per-run mount.
#[test]
fn r79_f1_host_sessions_invisible() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
    let host_data = PathBuf::from(&home).join(".local/share/opencode");
    std::fs::create_dir_all(&host_data).unwrap();
    let sentinel = host_data.join("sdb-r79-sentinel");
    std::fs::write(&sentinel, "host-session\n").unwrap();
    let base = r79_temp_dir("live");
    let workdir = base.join("workspace");
    std::fs::create_dir_all(&workdir).unwrap();
    let _dirs = OpenCodeRunner::prepare_run_dirs(&workdir);
    // The production mount args: on the old code these are the host dirs,
    // and the sentinel below is readable.
    let mut args: Vec<String> = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--entrypoint".to_string(),
        "/bin/bash".to_string(),
    ];
    args.extend(OpenCodeRunner::opencode_mount_args(&workdir));
    args.push("subdollar-sandbox".to_string());
    args.push("-c".to_string());
    args.push("test ! -r /home/ubuntu/.local/share/opencode/sdb-r79-sentinel".to_string());
    let status = std::process::Command::new("docker").args(&args).status();
    let _ = std::fs::remove_file(&sentinel);
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        status.map(|s| s.success()).unwrap_or(false),
        "the host's opencode session sentinel was readable from the container"
    );
}

/// R79 F3: the harness's opencode cost for the real 2026-11-11 probe
/// session (ling-3.1-flash, the ref-sql tool run) matches the snapshot
/// formula the bill is based on within 1.25x.
#[test]
fn r79_f3_opencode_cost_matches_bill() {
    // The probe's second step: input 125, cache read 5312, output 3,
    // reasoning 41; the opencode session recorded $5.9373e-05.
    let tokens = serde_json::json!({
        "input": 125,
        "output": 3,
        "reasoning": 41,
        "cache": {"read": 5312, "write": 0}
    });
    let mut stats = OmpSessionStats::default();
    let cost = OpenCodeRunner::add_step(&mut stats, &tokens, "inclusionai/ling-3.1-flash");
    let expected = crate::cost::ModelPricing::for_model("inclusionai/ling-3.1-flash")
        .compute_cost_with_cache_and_write(125, 5312, 44, 0)
        .total_cost_usd;
    assert!(
        (cost - expected).abs() < 1e-12,
        "harness {cost} vs snapshot {expected}"
    );
    let bill = 5.9373e-05;
    let ratio = bill / expected;
    assert!(
        (0.8..=1.25).contains(&ratio),
        "bill {bill} vs snapshot {expected}: {ratio}"
    );
    assert_eq!(stats.prompt_tokens, 125);
    assert_eq!(stats.cached_tokens, 5312);
    assert_eq!(stats.completion_tokens, 44);
}

/// R79 F2: the opencode harness's billing signals, read from the lines
/// the local stub made opencode print: the 402 trips at once, the
/// exhausted 429 is a fault (not a count), and the broken stream is not a
/// billing fault at all.
#[test]
fn r79_f2_opencode_billing_signals() {
    fn log_json(fixture: &str) -> String {
        fixture
            .lines()
            .find_map(|line| line.split_once("[LOG] ").map(|(_, json)| json.to_string()))
            .expect("the fixture has a [LOG] line")
    }
    for (name, fixture) in [
        (
            "402",
            include_str!("../../campaign/fixtures/health/opencode-402.txt"),
        ),
        (
            "exhausted 429",
            include_str!("../../campaign/fixtures/health/opencode-exhausted.txt"),
        ),
    ] {
        assert!(
            OpenCodeRunner::error_event_billing_reason(&log_json(fixture)).is_some(),
            "{name} must be a billing fault"
        );
    }
    assert_eq!(
        OpenCodeRunner::error_event_billing_reason(&log_json(include_str!(
            "../../campaign/fixtures/health/opencode-stream_error.txt"
        ))),
        None,
        "a broken stream is a tool error, not a billing fault"
    );
    let event = ev(r#"{"error":{"type":"provider.billing","message":"insufficient credits"}}"#);
    assert!(OpenCodeRunner::provider_auth_billing_reason(&event).is_some());
    let rate = ev(r#"{"error":{"type":"provider.rate-limit","status":429}}"#);
    assert!(OpenCodeRunner::is_rate_limit_event(&rate, ""));
}

/// R79-4: the per-trial config carries the snapshot provider pin (the
/// same endpoint the omp check sends), and none for an unpinned model.
#[test]
fn r79_f4_opencode_config_pins_the_provider() {
    let cfg: serde_json::Value = serde_json::from_str(&OpenCodeRunner::variant_config_json(
        "inclusionai/ling-3.1-flash",
    ))
    .unwrap();
    assert_eq!(
        cfg["providers"]["openrouter"]["models"]["inclusionai/ling-3.1-flash"]["body"]["provider"]
            ["only"][0],
        "novita"
    );
    assert_eq!(
        cfg["providers"]["openrouter"]["models"]["inclusionai/ling-3.1-flash"]["body"]["provider"]
            ["order"][0],
        "novita"
    );
    assert!(
        cfg["providers"]["openrouter"]["body"].is_null(),
        "the pin must not be provider-wide (it would pin the title model): {cfg}"
    );
    let none: serde_json::Value = serde_json::from_str(&OpenCodeRunner::variant_config_json(
        "google/gemini-2.5-flash",
    ))
    .unwrap();
    assert!(
        none["providers"]["openrouter"]["models"]["google/gemini-2.5-flash"]["body"].is_null(),
        "an unpinned model must carry no provider body: {none}"
    );
}

fn all_live_tasks() -> Vec<crate::config::TaskType> {
    use crate::config::TaskType;
    vec![TaskType::Sql]
}

#[test]
fn test_agents_md_renders_worker_ports() {
    // End-to-end through the real writer (tempdir, never the candidate
    // tree): every task, every worker. Catches the live incident where
    // AGENTS.md said 8091 while the brief said the worker port.
    for task in all_live_tasks() {
        let base = task.default_port();
        let (port, test_cmd, snippet) = OmpRunner::task_guidance(Some(task), "");
        assert_eq!(port, base);
        for worker in [1u8, 2, 3] {
            let dir = std::env::temp_dir().join(format!(
                "agents_md_{}_{}_{}",
                std::process::id(),
                task,
                worker
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            OpenCodeRunner::write_agents_md(&dir, snippet, port, test_cmd, "high", worker);
            let content = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
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
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[test]
fn test_cap_line_never_splits_utf8() {
    // CJK + emoji straddling the cut point must not panic the thread.
    let s = "a".repeat(1498) + "你好🌍";
    let out = OpenCodeRunner::cap_line(&s);
    assert!(out.ends_with("…[truncated]"));
    assert!(out.len() <= 1500 + "…[truncated]".len());
    // Short strings pass through untouched.
    assert_eq!(OpenCodeRunner::cap_line("hi"), "hi");
}

#[test]
fn test_openrouter_only_guard() {
    assert_eq!(
        OpenCodeRunner::resolve_model_id("openrouter/inclusionai/ling-3.1-flash").unwrap(),
        "openrouter/inclusionai/ling-3.1-flash"
    );
    for bad in [
        "opencode/ling-3.1-flash-free",
        "anthropic/claude-x",
        "",
        "  ",
    ] {
        let err = OpenCodeRunner::resolve_model_id(bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("OpenRouter-only"), "unexpected: {err}");
    }
}

#[test]
fn test_snapshot_stats_carries_turns_to_steps_taken() {
    // Regression: 171/171 OpenCode manifests recorded turns: 0 because
    // the step_start counter never reached the returned stats (OMP sets
    // the field, so the defect was harness-correlated, not data).
    let stats = std::sync::Mutex::new(OmpSessionStats::default());
    let turns = std::sync::atomic::AtomicU32::new(0);
    assert_eq!(
        OpenCodeRunner::snapshot_stats(&stats, &turns, None).steps_taken,
        0
    );
    turns.fetch_add(1, Ordering::SeqCst);
    turns.fetch_add(1, Ordering::SeqCst);
    let st = OpenCodeRunner::snapshot_stats(&stats, &turns, Some("#max".to_string()));
    assert_eq!(
        st.steps_taken, 2,
        "reader-thread turns must reach the manifest"
    );
    assert_eq!(
        st.effort_effective.as_deref(),
        Some("#max"),
        "effective effort must ride the same snapshot"
    );
}

#[test]
fn test_resolve_effort_effective_labels_what_ran() {
    use OpenCodeRunner as R;
    assert_eq!(
        R::resolve_effort_effective(&Some("max".to_string()), false),
        "#max"
    );
    assert_eq!(
        R::resolve_effort_effective(&Some("high".to_string()), false),
        "#high"
    );
    // Fallback or unmapped levels: prose-only, honestly labeled.
    assert_eq!(
        R::resolve_effort_effective(&Some("max".to_string()), true),
        "prose-only"
    );
    assert_eq!(R::resolve_effort_effective(&None, true), "prose-only");
    assert_eq!(R::resolve_effort_effective(&None, false), "prose-only");
}

#[test]
fn test_variant_for_effort_maps_wave_vocabulary() {
    // Only low/high/max (+ the high aliases) get machine variants;
    // anything else stays prose-only rather than risking unknown-variant
    // failures on untested levels.
    for (eff, want) in [
        ("low", Some("low")),
        ("high", Some("high")),
        ("max", Some("max")),
        ("HIGH", Some("high")),
        ("  max  ", Some("max")),
        ("", Some("high")),
        ("default", Some("high")),
        ("auto", Some("high")),
    ] {
        assert_eq!(
            OpenCodeRunner::variant_for_effort(eff).as_deref(),
            want,
            "effort {eff:?}"
        );
    }
    for eff in ["minimal", "medium", "xhigh", "off", "ultra"] {
        assert_eq!(
            OpenCodeRunner::variant_for_effort(eff),
            None,
            "effort {eff:?} must stay prose-only"
        );
    }
}

#[test]
fn test_variant_config_defines_machine_effort() {
    let cfg: serde_json::Value = serde_json::from_str(&OpenCodeRunner::variant_config_json(
        "inclusionai/ling-3.1-flash",
    ))
    .unwrap();
    // Slashes in model ids defeat naive JSON pointers; index directly.
    let arr = cfg["providers"]["openrouter"]["models"]["inclusionai/ling-3.1-flash"]["variants"]
        .as_array()
        .expect("three effort variants");
    assert_eq!(arr.len(), 3);
    for (id, effort) in [("low", "low"), ("high", "high"), ("max", "max")] {
        let v = arr.iter().find(|v| v["id"] == id).expect("variant {id}");
        assert_eq!(
            v["settings"]["reasoning"]["effort"], effort,
            "variant {id} must set native reasoning effort"
        );
    }
}

#[test]
fn test_variant_unavailable_event_matches_only_that_shape() {
    // Exact shape captured from a live probe: instant, zero-spend,
    // retryable prose-only.
    let e = ev(
        r#"{"type":"error","timestamp":1791556519670,"sessionID":"ses_x","error":{"type":"provider.no-route","message":"Variant unavailable for openrouter/inclusionai/ling-3.1-flash: max"}}"#,
    );
    assert!(OpenCodeRunner::is_variant_unavailable_event(&e));
    // Rate-limit, billing, and ordinary events must keep their paths.
    let r = ev(r#"{"type":"error","error":{"type":"provider.rate-limit","status":429}}"#);
    assert!(!OpenCodeRunner::is_variant_unavailable_event(&r));
    let b = ev(r#"{"error":{"code":402,"message":"Payment Required"}}"#);
    assert!(!OpenCodeRunner::is_variant_unavailable_event(&b));
    let t = ev(r#"{"type":"text","part":{"type":"text","text":"hi"}}"#);
    assert!(!OpenCodeRunner::is_variant_unavailable_event(&t));
    assert!(!OpenCodeRunner::is_variant_unavailable_event(
        &serde_json::Value::Null
    ));
}

/// Round-13 ops: a provider auth/billing event names itself — a stale
/// key surfaced as "exited with no further detail" and burned a cell.
#[test]
fn test_provider_auth_billing_event_names_itself() {
    let auth = ev(
        r#"{"type":"error","timestamp":1792326227450,"sessionID":"ses_x","error":{"type":"provider.auth","message":"User not found.","status":401,"response":{"body":"{\"error\":{\"message\":\"User not found.\",\"code\":401}}"}}}"#,
    );
    let reason = OpenCodeRunner::provider_auth_billing_reason(&auth).expect("auth reason");
    assert!(reason.contains("User not found."), "{reason}");
    assert!(reason.contains("API key"), "{reason}");
    // Other event classes keep their own paths.
    let r = ev(r#"{"type":"error","error":{"type":"provider.rate-limit","status":429}}"#);
    assert!(OpenCodeRunner::provider_auth_billing_reason(&r).is_none());
    let t = ev(r#"{"type":"text","part":{"type":"text","text":"hi"}}"#);
    assert!(OpenCodeRunner::provider_auth_billing_reason(&t).is_none());
}

#[test]
fn test_should_recover_cutoff_needs_all_three_signals() {
    use OpenCodeRunner as R;
    // The measured shape: clean exit, 0 tools, empty workspace.
    assert!(R::should_recover_cutoff(true, false, 0, 0, true));
    // Second recovery allowed (cap is 2)...
    assert!(R::should_recover_cutoff(true, false, 0, 1, true));
    // ...third strike accepted as genuine failure.
    assert!(!R::should_recover_cutoff(true, false, 0, 2, true));
    // Acted or produced files: never recover, however it exited.
    assert!(!R::should_recover_cutoff(true, false, 1, 0, true));
    assert!(!R::should_recover_cutoff(true, false, 0, 0, false));
    // Non-clean exits keep their own paths (timeout/rate-limit arms).
    assert!(!R::should_recover_cutoff(false, false, 0, 0, true));
    assert!(!R::should_recover_cutoff(true, true, 0, 0, true));
}

#[test]
fn test_workspace_has_content_ignores_our_agents_md() {
    let dir = std::env::temp_dir().join(format!("ws_content_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!OpenCodeRunner::workspace_has_content(&dir));
    std::fs::write(dir.join("AGENTS.md"), "ours").unwrap();
    assert!(
        !OpenCodeRunner::workspace_has_content(&dir),
        "our pre-launch file must not count"
    );
    std::fs::write(dir.join("notes.txt"), "agent wrote this").unwrap();
    assert!(OpenCodeRunner::workspace_has_content(&dir));
    let _ = std::fs::remove_dir_all(&dir);

    let dir2 = std::env::temp_dir().join(format!("ws_entry_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir2);
    std::fs::create_dir_all(&dir2).unwrap();
    std::fs::write(dir2.join("start.sh"), "#!/bin/sh").unwrap();
    assert!(OpenCodeRunner::workspace_has_content(&dir2));
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn test_cutoff_nudge_is_tool_first_and_names_port() {
    let n = OpenCodeRunner::cutoff_nudge(8090);
    assert!(n.contains("8090"), "must restate the action contract");
    assert!(n.contains("`write`"), "must name tools, not plans");
    assert!(
        n.len() < 600,
        "nudge must be short — it rides every recovery attempt"
    );
}

#[test]
fn test_rate_limit_detector() {
    // Real 429 shape from the Novita shared pool.
    let e = ev(r#"{"type":"error","error":{"type":"provider.rate-limit","status":429}}"#);
    assert!(OpenCodeRunner::is_rate_limit_event(&e, ""));
    // Bare digits never match: token counts, toolCallIds, line numbers.
    let t = ev(r#"{"tokens":{"input":5390,"output":429}}"#);
    assert!(!OpenCodeRunner::is_rate_limit_event(&t, ""));
    // Billing/auth never match here (separate wave-pause path).
    let b = ev(r#"{"error":{"code":402,"message":"Payment Required"}}"#);
    assert!(!OpenCodeRunner::is_rate_limit_event(&b, ""));
    let a = ev(r#"{"error":{"code":401,"message":"unauthorized key"}}"#);
    assert!(!OpenCodeRunner::is_rate_limit_event(&a, ""));
    // Raw-text phrases match; bare digits do not.
    assert!(OpenCodeRunner::is_rate_limit_event(
        &serde_json::Value::Null,
        "provider rate-limited, retry shortly"
    ));
    assert!(!OpenCodeRunner::is_rate_limit_event(
        &serde_json::Value::Null,
        "output 429 tokens used"
    ));
}

/// A throttled no-op is a provider fault, never a model verdict. The
/// condition must stay narrow: any real model output (tokens or tool
/// calls) disqualifies it, and a clean session without a rate-limit
/// stays a normal (model-failure) shape — the cutoff-recovery path
/// owns that case.
#[test]
fn test_throttled_noop_classification() {
    // The live nemotron shape: 429 wall, 0 tokens, 0 tools.
    assert!(OpenCodeRunner::is_throttled_noop(true, 0, 0));
    // Any model interaction is real work — never reclassified.
    assert!(!OpenCodeRunner::is_throttled_noop(true, 1, 0));
    assert!(!OpenCodeRunner::is_throttled_noop(true, 0, 1));
    assert!(!OpenCodeRunner::is_throttled_noop(true, 5000, 3));
    // No rate-limit signal: a genuinely empty session is the
    // cutoff-recovery arm's case, not billing.
    assert!(!OpenCodeRunner::is_throttled_noop(false, 0, 0));
}

/// R24-3: an event with only cache-write tokens is priced, not free.
#[test]
fn r24_r3_a_cache_write_only_event_costs_more_than_zero() {
    let mut st = OmpSessionStats::default();
    let tokens = ev(r#"{"cache":{"write":1000}}"#);
    let cost = OpenCodeRunner::add_step(&mut st, &tokens, "anthropic/claude-haiku-5.5");
    assert!(cost > 0.0, "cache-write-only event cost {cost}");
    assert_eq!(st.cache_write_tokens, 1000);
}

#[test]
fn test_add_step_maps_usage_losslessly() {
    let mut st = OmpSessionStats::default();
    let tokens = ev(r#"{"input":100,"output":20,"reasoning":5,"cache":{"read":7}}"#);
    OpenCodeRunner::add_step(&mut st, &tokens, "test-model");
    assert_eq!(st.prompt_tokens, 100);
    assert_eq!(
        st.completion_tokens, 25,
        "output+reasoning bills as completion"
    );
    assert_eq!(st.cached_tokens, 7);
    assert_eq!(st.total_tokens, 125);
    // Missing/odd fields degrade to zero, never panic.
    OpenCodeRunner::add_step(&mut st, &serde_json::Value::Null, "test-model");
    OpenCodeRunner::add_step(
        &mut st,
        &ev(r#"{"input":"nope","output":-3}"#),
        "test-model",
    );
    assert_eq!(st.prompt_tokens, 100);
    assert_eq!(st.completion_tokens, 25);
    assert_eq!(st.cached_tokens, 7);
    assert_eq!(st.total_tokens, 125);
    // cache as a scalar / missing object is tolerated.
    let mut st2 = OmpSessionStats::default();
    OpenCodeRunner::add_step(&mut st2, &ev(r#"{"input":4,"cache":3}"#), "test-model");
    assert_eq!(st2.prompt_tokens, 4);
    assert_eq!(st2.cached_tokens, 0);
}

#[test]
fn test_backoff_sleep_exits_immediately_on_deadline_or_cancel() {
    // Past deadline: no sleep, false.
    assert!(!OpenCodeRunner::backoff_sleep(
        0,
        SystemTime::now() - Duration::from_secs(1),
        &None
    ));
    // Pre-set cancel flag: no sleep, false.
    let cancel = Some(Arc::new(AtomicBool::new(true)));
    assert!(!OpenCodeRunner::backoff_sleep(
        3,
        SystemTime::now() + Duration::from_secs(3600),
        &cancel
    ));
    // Inactive cancel flag + far deadline would sleep; verified by the
    // dedicated fake-docker scenario instead of blocking this test.
}

#[test]
fn test_backoff_sleep_sleeps_then_deadline_returns_false() {
    // A 400ms horizon walks one 500ms slice (the sleep line) before the
    // deadline check flips to false.
    let started = std::time::Instant::now();
    let ok =
        OpenCodeRunner::backoff_sleep(0, SystemTime::now() + Duration::from_millis(400), &None);
    assert!(!ok, "deadline must stop the backoff");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "at least one sleep slice must have run"
    );
}

#[test]
fn test_is_rate_limit_event_structural_shapes() {
    // type slug containing rate-limit.
    assert!(OpenCodeRunner::is_rate_limit_event(
        &ev(r#"{"error":{"type":"provider.rate-limit"}}"#),
        ""
    ));
    // statusCode spelled without the underscore.
    assert!(OpenCodeRunner::is_rate_limit_event(
        &ev(r#"{"error":{"statusCode":429}}"#),
        ""
    ));
    // Nested arrays are walked.
    assert!(OpenCodeRunner::is_rate_limit_event(
        &ev(r#"{"errors":["not this",{"code":429}]}"#),
        ""
    ));
    // Object keys that merely contain 429 digits never match.
    assert!(!OpenCodeRunner::is_rate_limit_event(
        &ev(r#"{"toolCallId":"call_429abc","tokens":{"output":429}}"#),
        ""
    ));
    // Raw phrases from either case.
    assert!(OpenCodeRunner::is_rate_limit_event(
        &serde_json::Value::Null,
        "Too Many Requests"
    ));
    assert!(OpenCodeRunner::is_rate_limit_event(
        &serde_json::Value::Null,
        "rate_limit_exceeded"
    ));
}

#[test]
fn test_workspace_has_content_walks_subdirectories() {
    let dir = std::env::temp_dir().join(format!("ws_nested_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/deep")).unwrap();
    assert!(!OpenCodeRunner::workspace_has_content(&dir));
    std::fs::write(dir.join("src/deep/lib.rs"), "fn main() {}").unwrap();
    assert!(
        OpenCodeRunner::workspace_has_content(&dir),
        "nested candidate files must count"
    );
    let _ = std::fs::remove_dir_all(&dir);
    // A missing directory is empty, not a panic.
    assert!(!OpenCodeRunner::workspace_has_content(&dir));
}

#[test]
fn test_resolve_model_id_trims_whitespace() {
    assert_eq!(
        OpenCodeRunner::resolve_model_id("  openrouter/inclusionai/ling-3.1-flash  ").unwrap(),
        "openrouter/inclusionai/ling-3.1-flash"
    );
}

#[test]
fn test_run_agent_rejects_non_openrouter_model_before_docker() {
    // Fail-fast guard: no container, no session, no spend.
    let dir = std::env::temp_dir().join(format!("oc_bad_model_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cancel: Option<Arc<AtomicBool>> = None;
    let res = <OpenCodeRunner as crate::sandbox::AgentRunner>::run_agent_with_logger(
        AgentRunSpec {
            model: "opencode/ling-3.1-flash-free",
            prompt: "covtest",
            workdir: &dir,
            api_key: None,
            limits: covtest_limits(10, 1.0, 10),
            effort: None,
            network: None,
            task: None,
            run_id: Some("covtest-oc-bad-model"),
            cancel_flag: cancel,
            worker: 0,
        },
        |_line| {},
    );
    let err = res.unwrap_err().to_string();
    assert!(err.contains("OpenRouter-only"), "unexpected: {err}");
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
// env mutation (PATH/HOME/scenario) and the parallel suite never sees
// them. The child also isolates ~/.config/opencode lookups behind a temp
// HOME.
const DRIVER_ENV: &str = "SDB_COVTEST_OC_DRIVER";
const BIN_ENV: &str = "SDB_COVTEST_OC_BIN";
const WORK_ENV: &str = "SDB_COVTEST_OC_WORK";

fn covtest_limits(
    max_turns: u32,
    max_budget: f64,
    timeout: u64,
) -> crate::sandbox::omp::AgentExecutionLimits {
    crate::sandbox::omp::AgentExecutionLimits {
        max_turns: Some(max_turns),
        max_budget_usd: Some(max_budget),
        max_effective_usd: None,
        timeout_seconds: Some(timeout),
    }
}

/// Deterministic fake `docker`. `rm`/`kill` touch a kill file while a
/// scenario script is up (so polling scenarios die the instant the
/// runner reaps them); `run` emits the scenario's NDJSON.
const FAKE_DOCKER: &str = r#"#!/bin/sh
# covtest fake `docker`: no engine, no containers, deterministic output.
scenario="${SDB_COVTEST_SCENARIO:-}"
state="${SDB_COVTEST_STATE:-}"
started="${state}.started"
kill="${state}.kill"

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
      success|network|rel)
        printf '%s\n' \
          '{"type":"step_start","sessionID":"ses_covtest"}' \
          '{"type":"tool_use","part":{"tool":"write"}}' \
          '{"type":"text","part":{"text":"working on it"}}' \
          '{"type":"step_finish","part":{"tokens":{"input":100,"output":20,"reasoning":5,"cache":{"read":7}},"cost":0.01}}' \
          ''
        printf '%s\n' '' 'note: incidental stderr' '{"type":"weird","note":"rate_limit"}' >&2
        i=0
        while [ "$i" -lt 25 ]; do
          printf 'stderr noise line %s\n' "$i" >&2
          i=$((i + 1))
        done
        exit 0
        ;;
      rel_missing)
        printf '%s\n' '{"type":"tool_use","part":{"tool":"write"}}'
        exit 0
        ;;
      text_rate)
        printf '%s\n' \
          '{"type":"step_start"}' \
          '{"type":"tool_use","part":{"tool":"write"}}' \
          '{"type":"step_finish","tokens":{"input":5,"output":5}}' \
          '{"type":"text","part":{"text":"all good","status":429}}'
        exit 0
        ;;
      malformed)
        printf '%s\n' \
          '{"type":"step_start"}' \
          'not json at all' \
          '{"type":"weird","x":1}' \
          '{"type":"tool_use","part":{"tool":"write"}}' \
          '{"type":"step_finish","tokens":{"input":9,"output":1}}'
        exit 0
        ;;
      cutoff)
        exit 0
        ;;
      nonzero)
        printf '%s\n' 'fatal: kaboom' >&2
        exit 3
        ;;
      nonzero_empty)
        exit 7
        ;;
      billing)
        printf '%s\n' '{"type":"text","part":{"text":"insufficient credit balance"}}'
        exit 0
        ;;
      billing_stderr)
        printf '%s\n' 'HTTP/1.1 402 Payment Required' >&2
        exit 0
        ;;
      rate)
        printf '%s\n' '{"type":"error","error":{"type":"provider.rate-limit","status":429}}'
        exit 0
        ;;
      variant)
        n=0
        if [ -n "$state" ] && [ -f "${state}.count" ]; then n=$(cat "${state}.count"); fi
        n=$((n + 1))
        if [ -n "$state" ]; then printf '%s' "$n" > "${state}.count"; fi
        if [ "$n" -eq 1 ]; then
          printf '%s\n' '{"type":"error","error":{"type":"provider.no-route","message":"Variant unavailable for openrouter/x: max"}}'
          exit 1
        fi
        printf '%s\n' '{"type":"step_start"}' '{"type":"tool_use","part":{"tool":"write"}}'
        exit 0
        ;;
      turn_limit)
        printf '%s\n' '{"type":"step_start"}' '{"type":"step_start"}'
        exit 0
        ;;
      budget_limit)
        printf '%s\n' '{"type":"step_start"}' '{"type":"step_finish","part":{"tokens":{"input":10000,"output":10}}}'
        exit 0
        ;;
      cap)
        printf '%s\n' \
          '{"type":"step_start"}' \
          '{"type":"step_finish","part":{"tokens":{"input":50000,"output":0}}}' \
          '{"type":"step_start"}' \
          '{"type":"step_finish","part":{"tokens":{"input":150000,"output":0}}}'
        wait_for_kill
        ;;
      ready)
        if [ -n "$SDB_COVTEST_WORKDIR" ]; then : > "$SDB_COVTEST_WORKDIR/READY"; fi
        wait_for_kill
        ;;
      timeout|cancel)
        wait_for_kill
        ;;
    esac
    exit 1
    ;;
esac
exit 1
"#;

fn run_oc_scenario(
    name: &str,
    root: &std::path::Path,
    limits: crate::sandbox::omp::AgentExecutionLimits,
    cancel: Option<Arc<AtomicBool>>,
) -> (Result<OmpSessionStats>, Vec<String>) {
    run_oc_scenario_opts(name, root, limits, cancel, None, false, true)
}

fn run_oc_scenario_opts(
    name: &str,
    root: &std::path::Path,
    limits: crate::sandbox::omp::AgentExecutionLimits,
    cancel: Option<Arc<AtomicBool>>,
    network: Option<&str>,
    relative_workdir: bool,
    create_workdir: bool,
) -> (Result<OmpSessionStats>, Vec<String>) {
    let workdir = if relative_workdir {
        PathBuf::from(format!("ws-{name}"))
    } else {
        root.join(format!("ws-{name}"))
    };
    if create_workdir {
        std::fs::create_dir_all(&workdir).unwrap();
    }
    for suffix in ["started", "kill", "count"] {
        let _ = std::fs::remove_file(root.join(format!("state-{name}.{suffix}")));
    }
    std::env::set_var("SDB_COVTEST_SCENARIO", name);
    std::env::set_var("SDB_COVTEST_STATE", root.join(format!("state-{name}")));
    std::env::set_var("SDB_COVTEST_WORKDIR", &workdir);
    let run_id = format!("covtest-oc-{}-{name}", std::process::id());
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let logs_c = logs.clone();
    let res = <OpenCodeRunner as crate::sandbox::AgentRunner>::run_agent_with_logger(
        AgentRunSpec {
            model: "openrouter/covtest/model",
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
    let logs = logs.lock().unwrap().clone();
    (res, logs)
}

// Everything below relies on the fake `docker` being first on PATH.
fn oc_driver() {
    let bin = std::env::var(BIN_ENV).expect("SDB_COVTEST_OC_BIN");
    let work = PathBuf::from(std::env::var(WORK_ENV).expect("SDB_COVTEST_OC_WORK"));
    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", bin, old_path));
    let home = work.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let root = work.join("scenarios");
    std::fs::create_dir_all(&root).unwrap();
    let mut passed = 0usize;

    // 1. Clean exit 0 with a tool call and token stats.
    {
        let (res, logs) = run_oc_scenario("success", &root, covtest_limits(10, 1.0, 10), None);
        let st = res.expect("clean exit 0 must be Ok");
        assert_eq!(st.steps_taken, 1, "step_start events count turns");
        assert_eq!(st.prompt_tokens, 100);
        assert_eq!(st.completion_tokens, 25, "output+reasoning join completion");
        assert_eq!(st.cached_tokens, 7);
        assert_eq!(st.total_tokens, 125);
        assert_eq!(st.effort_effective.as_deref(), Some("#high"));
        assert!(logs.iter().any(|l| l.contains("[ACTION] write")));
        assert!(logs.iter().any(|l| l.contains("[RESPONSE] working on it")));
        assert!(logs.iter().any(|l| l.contains("[TELEMETRY] turn=1")));
        passed += 1;
    }

    // 2. Malformed and unknown NDJSON lines are logged, not fatal.
    {
        let (res, logs) = run_oc_scenario("malformed", &root, covtest_limits(10, 1.0, 10), None);
        let st = res.expect("unknown lines must not fail the run");
        assert_eq!(st.steps_taken, 1);
        assert_eq!(st.prompt_tokens, 9);
        assert_eq!(st.total_tokens, 10);
        assert!(logs.iter().any(|l| l == "[LOG] not json at all"));
        assert!(logs.iter().any(|l| l.contains("[LOG] {\"type\":\"weird\"")));
        passed += 1;
    }

    // 3. Clean exit 0 that never acted: two capped cutoff recoveries,
    //    then the clean exit is accepted.
    {
        let (res, logs) = run_oc_scenario("cutoff", &root, covtest_limits(10, 1.0, 10), None);
        let st = res.expect("post-recovery clean exit must conclude Ok");
        assert_eq!(st.steps_taken, 0);
        let recovered = logs
            .iter()
            .filter(|l| l.contains("[CUTOFF-RECOVER]"))
            .count();
        assert_eq!(recovered, 2, "cap is MAX_CUTOFF_RECOVERIES");
        passed += 1;
    }

    // 4. Non-zero exit: the stderr tail joins the message.
    {
        let (res, _) = run_oc_scenario("nonzero", &root, covtest_limits(10, 1.0, 10), None);
        let err = res.expect_err("exit 3 must be an error");
        let exec = err
            .downcast_ref::<OmpExecutionError>()
            .expect("OmpExecutionError");
        assert!(exec.message.contains("code 3"), "{}", exec.message);
        assert!(exec.message.contains("fatal: kaboom"), "{}", exec.message);
        passed += 1;
    }

    // 4b. Empty stderr tail: the message degrades to the code only.
    {
        let (res, _) = run_oc_scenario("nonzero_empty", &root, covtest_limits(10, 1.0, 10), None);
        let err = res.expect_err("exit 7 must be an error");
        let exec = err
            .downcast_ref::<OmpExecutionError>()
            .expect("OmpExecutionError");
        assert!(
            exec.message.contains("no further detail"),
            "{}",
            exec.message
        );
        passed += 1;
    }

    // 5. Billing phrase in a text event pauses in the billing envelope.
    {
        let (res, logs) = run_oc_scenario("billing", &root, covtest_limits(10, 1.0, 10), None);
        let err = res.expect_err("billing phrase must trip");
        assert!(err.downcast_ref::<OmpBillingError>().is_some());
        assert!(logs.iter().any(|l| l.contains("[BILLING]")));
        passed += 1;
    }

    // 6. Billing phrase on stderr takes the same envelope.
    {
        let (res, _) = run_oc_scenario("billing_stderr", &root, covtest_limits(10, 1.0, 10), None);
        let err = res.expect_err("stderr 402 must trip");
        assert!(err.downcast_ref::<OmpBillingError>().is_some());
        passed += 1;
    }

    // 6b. Rate-limit phrase inside a text event with real token usage:
    //     still a normal Ok (the resume arm only takes non-zero exits).
    {
        let (res, _) = run_oc_scenario("text_rate", &root, covtest_limits(10, 1.0, 10), None);
        let st = res.expect("clean exit with a phrase in text is still a verdict");
        assert_eq!(st.total_tokens, 10);
        passed += 1;
    }

    // 7. Provider 429 with zero model output: backoff starts and the
    //    short deadline cuts it (never a ranked 0%).
    {
        let (res, logs) = run_oc_scenario("rate", &root, covtest_limits(10, 1.0, 1), None);
        let err = res.expect_err("backoff interrupted by the deadline");
        let exec = err
            .downcast_ref::<OmpExecutionError>()
            .expect("OmpExecutionError");
        assert!(
            exec.message.contains("backoff interrupted"),
            "{}",
            exec.message
        );
        assert!(logs
            .iter()
            .any(|l| l.contains("[RETRY] provider rate-limited with zero model output")));
        passed += 1;
    }

    // 8. Variant resolution failure: one prose-only retry, then Ok.
    {
        let (res, logs) = run_oc_scenario("variant", &root, covtest_limits(10, 1.0, 10), None);
        let st = res.expect("prose-only retry must succeed");
        assert_eq!(st.steps_taken, 1);
        assert_eq!(st.effort_effective.as_deref(), Some("prose-only"));
        assert!(logs
            .iter()
            .any(|l| l.contains("[VARIANT] model variant unavailable")));
        assert!(logs.iter().any(|l| l.contains("resuming prose-only")));
        passed += 1;
    }

    // 9. Wall-clock timeout: reaped and reported.
    {
        let (res, _) = run_oc_scenario("timeout", &root, covtest_limits(10, 1.0, 1), None);
        let err = res.expect_err("timeout must fail the attempt");
        let exec = err
            .downcast_ref::<OmpExecutionError>()
            .expect("OmpExecutionError");
        assert!(exec.message.contains("timeout reached"), "{}", exec.message);
        assert_eq!(exec.partial_stats.stop_reason.as_deref(), Some("wall"));
        passed += 1;
    }

    // 10. Cancel flag: reported as user cancellation.
    {
        let cancel = Arc::new(AtomicBool::new(true));
        let (res, _) = run_oc_scenario("cancel", &root, covtest_limits(10, 1.0, 10), Some(cancel));
        let err = res.expect_err("cancel must fail the attempt");
        let exec = err
            .downcast_ref::<OmpExecutionError>()
            .expect("OmpExecutionError");
        assert!(
            exec.message.contains("cancelled by user"),
            "{}",
            exec.message
        );
        passed += 1;
    }

    // 11. Max-turns limit: partial stats, no error.
    {
        let (res, _) = run_oc_scenario("turn_limit", &root, covtest_limits(1, 1.0, 10), None);
        let st = res.expect("limit stop is a normal conclusion");
        // Round-14 N4: the turn killed at the cap is not counted, so an
        // agent stopped at its cap records exactly the cap.
        assert_eq!(st.steps_taken, 1);
        assert_eq!(st.stop_reason.as_deref(), Some("turns"));
        passed += 1;
    }

    // 12. Budget cap: partial stats, no error.
    {
        let (res, _) = run_oc_scenario("budget_limit", &root, covtest_limits(10, 0.001, 10), None);
        let st = res.expect("budget stop is a normal conclusion");
        assert_eq!(st.total_tokens, 10010);
        assert_eq!(st.stop_reason.as_deref(), Some("dollars"));
        passed += 1;
    }

    // 13. Campaign cap headroom: two calls ($0.01, $0.03) with a $0.02
    // effective ceiling trip the cap line, never the $1.00 API budget,
    // and the attempt reports the metered sum and largest call.
    {
        let mut limits = covtest_limits(10, 1.0, 10);
        limits.max_effective_usd = Some(0.02);
        let (res, logs) = run_oc_scenario("cap", &root, limits, None);
        let st = res.expect("cap trip is a normal conclusion");
        assert!(logs
            .iter()
            .any(|l| l.contains("Reached the campaign cap's headroom")));
        assert!(!logs.iter().any(|l| l.contains("budget $1.00 exceeded")));
        assert!((st.cost_usd - 0.04).abs() < 1e-9, "cost {}", st.cost_usd);
        assert!(
            (st.max_call_usd - 0.03).abs() < 1e-9,
            "max {}",
            st.max_call_usd
        );
        passed += 1;
    }

    // 14. READY signal scores exactly like a normal exit.
    {
        let (res, logs) = run_oc_scenario("ready", &root, covtest_limits(10, 1.0, 10), None);
        res.expect("READY conclusion must be Ok");
        assert!(logs
            .iter()
            .any(|l| l.contains("signaled completion via /workspace/READY")));
        passed += 1;
    }

    // 14. Egress filter on with no run network: fail closed pre-spawn.
    {
        std::env::set_var("SDB_EGRESS_FILTER", "1");
        let (res, _) = run_oc_scenario("egress", &root, covtest_limits(10, 1.0, 10), None);
        std::env::remove_var("SDB_EGRESS_FILTER");
        let err = res.expect_err("filtered run without a proxy sidecar must bail");
        assert!(
            err.to_string()
                .contains("refusing to run the agent unfiltered"),
            "{}",
            err
        );
        passed += 1;
    }

    // 15. Explicit run network (egress filter off): forwarded to docker.
    {
        let (res, _) = run_oc_scenario_opts(
            "network",
            &root,
            covtest_limits(10, 1.0, 10),
            None,
            Some("covtest-net"),
            false,
            true,
        );
        res.expect("explicit network must not change the run outcome");
        passed += 1;
    }

    // 16. Relative workdir resolution: canonicalized, run still Ok.
    {
        let rel_root = root.clone();
        std::env::set_current_dir(&rel_root).unwrap();
        let (res, _) = run_oc_scenario_opts(
            "rel",
            &rel_root,
            covtest_limits(10, 1.0, 10),
            None,
            None,
            true,
            true,
        );
        res.expect("relative workdir must resolve and run");
        passed += 1;
    }

    // 16b. Missing relative workdir: fallback resolution, run still Ok.
    {
        let (res, _) = run_oc_scenario_opts(
            "rel_missing",
            &root,
            covtest_limits(10, 1.0, 10),
            None,
            None,
            true,
            false,
        );
        res.expect("missing relative workdir must fall back and run");
        passed += 1;
    }

    // 17. Egress filter on with a run network: the sidecar start is
    //     attempted, fails against the fake engine, and fails closed.
    {
        std::env::set_var("SDB_EGRESS_FILTER", "1");
        let (res, _) = run_oc_scenario_opts(
            "egress_net",
            &root,
            covtest_limits(10, 1.0, 10),
            None,
            Some("subdollar-net-covtest"),
            false,
            true,
        );
        std::env::remove_var("SDB_EGRESS_FILTER");
        let err = res.expect_err("sidecar failure must fail closed");
        assert!(
            err.to_string()
                .contains("refusing to run the agent unfiltered"),
            "{}",
            err
        );
        passed += 1;
    }

    println!("fake-docker opencode scenarios passed ({passed})");
}

/// Re-exec'd child entry point: the parent sets DRIVER_ENV, so a normal
/// suite run is a no-op.
#[test]
fn fake_docker_driver() {
    if std::env::var(DRIVER_ENV).ok().as_deref() != Some("1") {
        return;
    }
    oc_driver();
}

/// Parent-side scenario launcher: writes the fake `docker` and runs the
/// driver in a child process so PATH/HOME mutations never leak into the
/// parallel test suite.
#[test]
fn test_opencode_runner_fake_docker_scenarios() {
    let root = std::env::temp_dir().join(format!("covtest-oc-{}", std::process::id()));
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
            "sandbox::opencode::tests::fake_docker_driver",
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
        "fake-docker opencode driver failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("fake-docker opencode scenarios passed"),
        "driver must report its scenarios; got:\n{stdout}\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// R82 F2/F3: the stub-backed session helpers. The stub is the local
/// OpenRouter chat-completions server (`scripts/opencode-stub.py`); the
/// container uses `--network=host`, so `127.0.0.1:<port>` is the host's
/// stub. Nothing is billed and no network is touched.
fn r82_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

struct R82Stub(std::process::Child);
impl Drop for R82Stub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn r82_start_stub(case: &str, log: &std::path::Path) -> (R82Stub, u16) {
    let port = r82_free_port();
    let stub = crate::config::get_repo_root().join("scripts/opencode-stub.py");
    let child = std::process::Command::new("python3")
        .arg(&stub)
        .arg(port.to_string())
        .arg(log)
        .arg(case)
        .spawn()
        .expect("start the stub");
    let stub = R82Stub(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (stub, port);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the stub did not start on port {port}");
}

fn r82_config(port: u16, bare: &str) -> String {
    // R82-1: the test runs the config the runner actually sends. The
    // stub's baseURL goes in the production plural `providers` schema's
    // `settings` (round 79's path), so the per-model pin and the effort
    // variants are the ones under test.
    let mut cfg: serde_json::Value =
        serde_json::from_str(&OpenCodeRunner::variant_config_json(bare))
            .expect("the runner config");
    cfg["providers"]["openrouter"]["settings"] = serde_json::json!({
        "baseURL": format!("http://127.0.0.1:{port}")
    });
    cfg.to_string()
}

fn r82_run_opencode(
    base: &std::path::Path,
    cfg: &str,
    model_arg: &str,
    session: &str,
    title: bool,
) {
    let ws = base.join("ws");
    let cfg_dir = base.join("cfg");
    let data = base.join("data");
    for d in [&ws, &cfg_dir, &data] {
        std::fs::create_dir_all(d).unwrap();
    }
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--network=host".into(),
        "-v".into(),
        format!("{}:/workspace", ws.display()),
        "-v".into(),
        format!("{}:/home/ubuntu/.config/opencode", cfg_dir.display()),
        "-v".into(),
        format!("{}:/home/ubuntu/.local/share/opencode", data.display()),
        "-w".into(),
        "/workspace".into(),
        "-e".into(),
        "HOME=/home/ubuntu".into(),
        "-e".into(),
        "OPENROUTER_API_KEY=dummy".into(),
        "-e".into(),
        format!("OPENCODE_CONFIG_CONTENT={cfg}"),
        "subdollar-sandbox".into(),
        "opencode".into(),
        "run".into(),
        "--standalone".into(),
        "--format".into(),
        "json".into(),
        "--session".into(),
        session.into(),
        "-m".into(),
        model_arg.into(),
        "--auto".into(),
    ];
    if title {
        args.push("--title".into());
        args.push("sdb-r82-test".into());
    }
    args.push("--".into());
    args.push(
        "Use the shell tool once: run `echo probe-forty-two`, then reply with its output.".into(),
    );
    let out = std::process::Command::new("docker")
        .args(&args)
        .output()
        .expect("docker run opencode");
    assert!(
        out.status.success(),
        "opencode failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn r82_bodies(log: &std::path::Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(body) = v.get("body").and_then(|b| b.as_str()) else {
            continue;
        };
        if let Ok(b) = serde_json::from_str::<serde_json::Value>(body) {
            out.push(b);
        }
    }
    out
}

/// R82 F2: every request of a two-turn session (with and without
/// `--title`) carries the career model and its provider pin. The stub's
/// request log is the evidence; no network is used.
#[test]
fn r82_f2_one_model_per_session() {
    let base = r79_temp_dir("r82-f2");
    let model = "openrouter/inclusionai/ling-3.1-flash";
    let bare = "inclusionai/ling-3.1-flash";
    let pin = crate::cost::calculator::provider_for_model(bare).expect("the model's pin");
    for title in [false, true] {
        let tag = if title { "title" } else { "no-title" };
        let log = base.join(format!("requests-{tag}.jsonl"));
        let (_stub, port) = r82_start_stub("tool", &log);
        let cfg = r82_config(port, bare);
        r82_run_opencode(
            &base,
            &cfg,
            &format!("{model}#low"),
            &format!("ses_r82_f2_{tag}"),
            title,
        );
        let bodies = r82_bodies(&log);
        assert!(!bodies.is_empty(), "{tag}: the stub saw no request");
        for body in &bodies {
            assert_eq!(
                body.get("model").and_then(|m| m.as_str()),
                Some(bare),
                "{tag}: a request used another model: {body}"
            );
            assert_eq!(
                body.pointer("/provider/only")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
                Some(vec![pin]),
                "{tag}: a request lacks the {pin} pin: {body}"
            );
            assert_eq!(
                body.pointer("/provider/order")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
                Some(vec![pin]),
                "{tag}: a request lacks the {pin} order: {body}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// R82 F3: `#low` and `#high` reach the request body as
/// `reasoning.effort`; the stub's log is the evidence.
#[test]
fn r82_f3_effort_reaches_the_request() {
    let base = r79_temp_dir("r82-f3");
    let model = "openrouter/inclusionai/ling-3.1-flash";
    let bare = "inclusionai/ling-3.1-flash";
    for effort in ["low", "high"] {
        let log = base.join(format!("requests-{effort}.jsonl"));
        let (_stub, port) = r82_start_stub("tool", &log);
        let cfg = r82_config(port, bare);
        r82_run_opencode(
            &base,
            &cfg,
            &format!("{model}#{effort}"),
            &format!("ses_r82_f3_{effort}"),
            false,
        );
        let bodies = r82_bodies(&log);
        assert!(!bodies.is_empty(), "{effort}: the stub saw no request");
        assert!(
            bodies
                .iter()
                .any(|b| b.pointer("/reasoning/effort").and_then(|v| v.as_str()) == Some(effort)),
            "{effort}: no request body carries reasoning.effort: {bodies:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}
