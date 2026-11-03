use super::*;

/// Round-14 N3: how an agent error ended. Billing never reads as "never
/// started" by accident, the runner's own sign-of-life flag wins over a
/// zero step count, and an unknown error never started.
#[test]
fn test_agent_error_end_classifies_billing_starts_and_unknowns() {
    use crate::sandbox::omp::{OmpBillingError, OmpExecutionError, OmpSessionStats};
    let billing = anyhow::Error::new(OmpBillingError {
        reason: "provider.auth: User not found.".into(),
        partial_stats: OmpSessionStats::default(),
    });
    let end = agent_error_end(&billing);
    assert_eq!(
        end.billing.as_deref(),
        Some("provider.auth: User not found.")
    );
    assert!(!end.started);
    let billing_mid = anyhow::Error::new(OmpBillingError {
        reason: "402".into(),
        partial_stats: OmpSessionStats {
            steps_taken: 7,
            total_tokens: 9000,
            ..Default::default()
        },
    });
    let end = agent_error_end(&billing_mid);
    assert!(end.started && end.billing.is_some());
    assert_eq!(end.partial.steps_taken, 7, "partial stats are kept");
    // A lost session file: 0 steps, but the runner saw stdout.
    let lost = anyhow::Error::new(OmpExecutionError {
        message: "exit 1".into(),
        partial_stats: OmpSessionStats::default(),
        started: true,
    });
    assert!(agent_error_end(&lost).started, "graded, not retried");
    let argv = anyhow::Error::new(OmpExecutionError {
        message: "exit 2".into(),
        partial_stats: OmpSessionStats::default(),
        started: false,
    });
    let end = agent_error_end(&argv);
    assert!(!end.started && end.billing.is_none());
    assert!(!agent_error_end(&anyhow::anyhow!("Failed to spawn omp")).started);
}

struct DummyLogger;
impl PipelineLogger for DummyLogger {
    fn log(&self, _msg: &str) {}
}

/// Logger capturing every line, for asserting on pipeline decisions.
#[derive(Default)]
struct RecordingLogger(std::sync::Mutex<Vec<String>>);

impl RecordingLogger {
    fn contains(&self, needle: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l.contains(needle))
    }

    fn text(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
}

impl PipelineLogger for RecordingLogger {
    fn log(&self, msg: &str) {
        self.0.lock().unwrap().push(msg.to_string());
    }
}

/// R31 F1: a live-proof assertion failure must carry the pipeline log
/// (the original infra_error panic did not, and the scratch dir was
/// already gone).
fn assert_proof_result(infra_error: bool, status: &str, logger: &RecordingLogger) {
    assert!(
        !infra_error,
        "infra_error; status {status}; log:\n{}",
        logger.text()
    );
}

fn assert_proof_ok(out: &PipelineOutput, logger: &RecordingLogger) {
    assert_proof_result(out.infra_error, &out.manifest.status, logger);
}

/// R30 F3: every needle failure carries the pipeline log.
fn assert_logged(logger: &RecordingLogger, needle: &str) {
    assert!(
        logger.contains(needle),
        "missing {needle:?}; log:\n{}",
        logger.text()
    );
}

/// R31-2/R37 F5: a failed proof execute prints the recorded log before
/// the panic, not only the error.
async fn expect_proof(
    fut: impl std::future::Future<Output = anyhow::Result<PipelineOutput>>,
    logger: &RecordingLogger,
    context: &str,
) -> PipelineOutput {
    match fut.await {
        Ok(out) => out,
        Err(e) => panic!("{context}: {e}; log:\n{}", logger.text()),
    }
}

#[tokio::test]
#[should_panic(expected = "[INFRA]")]
async fn r37_f5_proof_execute_err_prints_log() {
    let logger = RecordingLogger::default();
    logger.log("[INFRA] seeded execute failure");
    let _: PipelineOutput =
        expect_proof(async { Err(anyhow::anyhow!("boom")) }, &logger, "proof").await;
}

fn assert_proof_score(
    out: &PipelineOutput,
    logger: &RecordingLogger,
    passed_stages: u32,
    total_stages: u32,
) {
    assert_proof_ok(out, logger);
    assert_eq!(
        (out.manifest.passed_stages, out.manifest.total_stages),
        (passed_stages, total_stages),
        "score mismatch; log:\n{}",
        logger.text()
    );
}

/// R48 F1: two SQL workers never share a host port. This extends
/// round 37's table to the live worker slots 0-7: candidate 5480,
/// reference 5481, the admin/dashboard ports and the proxy pool all
/// move by 100 per worker.
#[test]
fn r48_f1_two_workers_share_no_port() {
    use crate::web::server::worker_port;
    use std::collections::HashSet;
    let ports = |w: u8| -> HashSet<u16> {
        [5480, 5481, 6395, 8090, 8101, 8102, 8103]
            .into_iter()
            .map(|base| worker_port(base, w))
            .collect()
    };
    for (a, b) in [(0u8, 1u8), (1, 2), (2, 3), (3, 4), (6, 7)] {
        assert!(
            ports(a).is_disjoint(&ports(b)),
            "workers {a} and {b} share a host port"
        );
    }
    assert_eq!(worker_port(5480, 2), 5680);
    assert_eq!(worker_port(5481, 2), 5681);
    assert_eq!(worker_port(8101, 2), 8301);
}

/// R43 F2 (R37-8): the task comes from the test's own `worker_port`
/// call, not its name. Only the candidate/reference ports identify a
/// task; the shared admin/dashboard/proxy-pool ports do not.
fn task_from_worker_port(src: &str) -> Option<TaskType> {
    let mut rest = src;
    while let Some(i) = rest.find("worker_port(") {
        let after = &rest[i + "worker_port(".len()..];
        let arg = after.split(',').next().unwrap_or("").trim();
        if matches!(arg, "TaskType::Sql.default_port()" | "5480" | "5481") {
            return Some(TaskType::Sql);
        }
        rest = after;
    }
    None
}

#[test]
fn r43_f2_port_table_reads_task_type() {
    // Misnamed `proxy`, but its own worker_port line says Sql.
    let src = r#"
            fn r99_proxy_misnamed() {
                let w = 8u8;
                let _ = worker_port(TaskType::Sql.default_port(), w);
            }
        "#;
    assert_eq!(task_from_worker_port(src), Some(TaskType::Sql));
    // The name alone proves nothing.
    assert_eq!(
        task_from_worker_port("fn r99_proxy_misnamed() { let _ = 1; }"),
        None
    );
    // A shared port is not a task port either.
    assert_eq!(
        task_from_worker_port("fn t() { let _ = worker_port(8090, 8); }"),
        None
    );
}

/// R37 F1: for every pipeline test, compute every host port it can
/// bind and reject two concurrent tests sharing one. Tests serialized
/// by SQL_PERF_LIVE may share with each other.
#[test]
fn r37_f1_worker_ports_do_not_collide() {
    use std::collections::{HashMap, HashSet};
    let source = concat!(
        include_str!("../pipeline.rs"),
        "\n",
        include_str!("tests.rs")
    );
    let mut current = String::new();
    let mut entries: Vec<(String, u8)> = Vec::new();
    let mut serialized: HashSet<String> = HashSet::new();
    // R43 F2: the whole body of every module-level fn, so the task can
    // be read from its worker_port call below (module-level fns are
    // indented four spaces; a test's inner helpers are deeper).
    let mut bodies: HashMap<String, String> = HashMap::new();
    let mut body_name: Option<String> = None;
    let mut body = String::new();
    for raw in source.lines() {
        let module_fn = raw.starts_with("    fn ")
            || raw.starts_with("    async fn ")
            || raw.starts_with("    pub fn ")
            || raw.starts_with("    pub async fn ");
        if module_fn {
            if let Some(n) = body_name.take() {
                bodies.insert(n, std::mem::take(&mut body));
            }
            let t = raw.trim_start();
            let t = t.strip_prefix("pub ").unwrap_or(t);
            let t = t.strip_prefix("async ").unwrap_or(t);
            let t = t.strip_prefix("fn ").unwrap_or(t);
            body_name = Some(t.split('(').next().unwrap_or("").trim().to_string());
        }
        if body_name.is_some() {
            body.push_str(raw);
            body.push('\n');
        }
        let line = raw.trim();
        // Build the markers so scripts/live-proofs.sh's awk does not
        // mistake this source for a test holding the lock.
        let async_prefix = "async".to_string() + " fn ";
        if let Some(rest) = line
            .strip_prefix(&async_prefix)
            .or_else(|| line.strip_prefix("fn "))
        {
            current = rest.split('(').next().unwrap_or("").trim().to_string();
        }
        let lock_name = "SQL_PERF_LIVE".to_string() + ".lock";
        if line.contains(&lock_name) {
            serialized.insert(current.clone());
        }
        if let Some(rest) = line.strip_prefix("let worker = ") {
            if let Some(num) = rest.strip_suffix("u8;") {
                entries.push((current.clone(), num.parse::<u8>().expect("worker id")));
            }
        } else if let Some(rest) = line.strip_prefix("worker: ") {
            if let Some(num) = rest.strip_suffix(',') {
                if let Ok(id) = num.parse::<u8>() {
                    entries.push((current.clone(), id));
                }
            }
        }
    }
    if let Some(n) = body_name {
        bodies.insert(n, body);
    }
    assert!(
        entries.len() >= 31,
        "worker table lost entries: {entries:?}"
    );
    let mut owners: HashMap<u16, String> = HashMap::new();
    let mut max_port = 0u16;
    for (name, worker) in &entries {
        assert!(
            *worker >= 8,
            "{name} uses worker {worker}; workers 0..7 are live cell slots"
        );
        // R43 F2 (R37-8): read the task from the test's own worker_port
        // call. With sdb-sql the only task, a body that binds only
        // shared ports still grades as Sql.
        let task_kind = bodies
            .get(name)
            .and_then(|src| task_from_worker_port(src))
            .unwrap_or(TaskType::Sql);
        let base = task_kind.default_port();
        // A test binds its own task's reference, plus the shared admin,
        // dashboard, HA and proxy-pool ports. A campaign only starts the
        // reference of its own task, so the other refs are not here.
        let mut ports = vec![
            crate::web::server::worker_port(base, *worker),
            crate::web::server::worker_port(6395, *worker),
            crate::web::server::worker_port(8090, *worker),
            crate::web::server::worker_port(7001, *worker),
            crate::web::server::worker_port(7002, *worker),
            crate::web::server::worker_port(7003, *worker),
            crate::web::server::worker_port(8101, *worker),
            crate::web::server::worker_port(8102, *worker),
            crate::web::server::worker_port(8103, *worker),
        ];
        ports.push(crate::web::server::worker_port(5481, *worker));
        ports.push(crate::web::server::worker_port(base + 50, *worker));
        ports.push(crate::web::server::worker_port(base + 60, *worker));
        ports.push(crate::web::server::worker_port(base + 61, *worker));
        ports.push(crate::web::server::worker_port(base + 62, *worker));
        for port in ports {
            max_port = max_port.max(port);
            if let Some(other) = owners.get(&port) {
                if other != name && !(serialized.contains(other) && serialized.contains(name)) {
                    panic!("{name} and {other} both bind host port {port}");
                }
            } else {
                owners.insert(port, name.clone());
            }
        }
    }
    assert!(
        max_port < 20_000,
        "worker ports leave the free_port range: {max_port}"
    );
}

#[test]
#[should_panic(expected = "[INFRA]")]
fn r31_f1_proof_assert_prints_log() {
    let logger = RecordingLogger::default();
    logger.log("[INFRA] seeded failure");
    assert_proof_result(true, "infra_error", &logger);
}

/// A test's temp dir, removed when the guard drops — on success and on
/// panic alike (the suite used to leave hundreds of `/tmp/covtest-*`
/// dirs behind, round-3 review finding 9). Derefs to the path, so
/// `dir.join(..)` and `&dir` keep working.
struct TestDir(PathBuf);
impl std::ops::Deref for TestDir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}
impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Temp fixture root unique to this process and test tag.
fn covtest_dir(tag: &str) -> TestDir {
    // The host's /tmp is a tmpfs. The live memory/disk proofs keep
    // multi-hundred-MB stores under this directory and cap the
    // candidate at 64/192 MiB; on tmpfs those files are unreclaimable
    // shmem, so the cap would test the host's scratch store instead of
    // the candidate. Keep this test scratch on a real disk, matching
    // the production campaign directories. SDB_COVTEST_ROOT overrides
    // it on CI hosts where /var/tmp is small.
    let root = std::env::var_os("SDB_COVTEST_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/var/tmp"));
    let dir = root.join(format!("covtest-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    TestDir(dir)
}

/// A deliberately broken sdb-sql candidate (review finding 2's fixture):
/// it speaks the protocol and answers every statement `OK 0`, so the
/// first setup INSERT mismatches on its affected count. Every start
/// drops a marker file into its `--data` directory, so a test can tell
/// which store each instance wrote to. Stateless, so a replay from a
/// fresh store reproduces the failure exactly.
const ALWAYS_OK_SQL_PY: &str = r#"import os, socket, sys, threading, time


def serve(conn):
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    f = conn.makefile("rwb")
    try:
        for _ in f:
            f.write(b"OK 0\n")
            f.flush()
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def main():
    port = 5480
    data = "/tmp"
    args = sys.argv[1:]
    for i, a in enumerate(args):
        if a == "--port" and i + 1 < len(args):
            port = int(args[i + 1])
        elif a == "--data" and i + 1 < len(args):
            data = args[i + 1]
    try:
        os.makedirs(data, exist_ok=True)
        with open(os.path.join(data, "marker-%d.txt" % time.time_ns()), "w") as f:
            f.write(str(os.getpid()))
    except OSError:
        pass
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(64)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


main()
"#;

/// Write the always-OK sdb-sql mock into `dir`, listening on `port`.
fn write_always_ok_sql_mock(dir: &Path, port: u16) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("server.py"), ALWAYS_OK_SQL_PY).unwrap();
    let start = format!(
        "#!/bin/bash\nif [ $# -eq 0 ]; then set -- --port {port} --data /tmp; fi\n\
             exec python3 -u /workspace/server.py \"$@\"\n"
    );
    fs::write(dir.join("start.sh"), start).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir.join("start.sh"), fs::Permissions::from_mode(0o755));
    }
}

/// Eval-only Redis config on a high worker offset: candidate/ref ports
/// live at base + worker*100, far away from the worker-0 ports
/// (6379/6380/8090) used by live campaign cells.
fn covtest_config(workdir: PathBuf, worker: u8, tag: &str) -> BenchmarkConfig {
    BenchmarkConfig {
        model: "test_model".to_string(),
        task: TaskType::Sql,
        effort: "low".to_string(),
        harness: Harness::Omp,
        max_turns: 1,
        budget_usd: 0.10,
        timeout_min: 1,
        api_key: None,
        workdir,
        eval_only: true,
        run_id: Some(format!("test_covtest_{tag}_{}", std::process::id())),
        save_results: false,
        review_model: None,
        worker,
        feedback_rounds: 0,
        campaign_stage: None,
        baseline_rps: None,
    }
}

/// The pipeline's own cleanup deliberately keeps networks younger than
/// its staleness window (a parallel worker may still be attaching). Tests
/// know their run is over, so reap their own named network explicitly.
fn cleanup_covtest_network(tag: &str) {
    let net = format!("subdollar-net-test_covtest_{tag}_{}", std::process::id());
    let _ = std::process::Command::new("docker")
        .args(["network", "rm", &net])
        .output();
}

/// Ticket 15's gate end-to-end against a real fixture (Docker + the
/// sandbox image): a repo with stage-6/7/8 commits, a green offline
/// `test.sh` and a coverage line passes (both reverts go red); a
/// toothless suite fails; the lite gate accepts 65% where the full gate
/// does not.
#[test]
fn test_tests_gate_sensitivity_end_to_end() {
    // The perf-gate live proofs measure ratios; keep the extra Docker
    // load out of their windows (same serialization as the SQL proofs).
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    use std::process::Command;
    let root = std::env::temp_dir().join(format!("s15-ci-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let good = root.join("good");
    std::fs::create_dir_all(&good).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&good)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(good.join("engine.py"), "BASE = True\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "stage 6: Transactions [pass]"]);
    std::fs::write(good.join("engine.py"), "BASE = True\nAGG = True\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "stage 7: Aggregates [pass]"]);
    std::fs::write(
        good.join("engine.py"),
        "BASE = True\nAGG = True\nJOIN = True\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "stage 8: Joins [pass]"]);
    std::fs::write(good.join("coverage.txt"), "coverage: 85.0%\n").unwrap();
    let good_suite =
        "#!/bin/sh\nset -e\ngrep -q '^AGG = True' engine.py\ngrep -q '^JOIN = True' engine.py\n";
    std::fs::write(good.join("test.sh"), good_suite).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(good.join("test.sh"), std::fs::Permissions::from_mode(0o755));
    }

    let st = run_tests_gate_blocking(
        SandboxManager::with_id(&format!("s15-good-{}", std::process::id())),
        good.clone(),
        Arc::new(DummyLogger),
        false,
    );
    assert!(st.passed, "good fixture must pass: {:?}", st.error);

    // Toothless: always green, so neither revert can fail it.
    std::fs::write(good.join("test.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    let st = run_tests_gate_blocking(
        SandboxManager::with_id(&format!("s15-bad-{}", std::process::id())),
        good.clone(),
        Arc::new(DummyLogger),
        false,
    );
    assert_eq!(st.stage, 15);
    assert!(!st.passed, "toothless suite must fail");
    assert!(
        st.error.as_deref().unwrap_or("").contains("stayed green"),
        "expected 'stayed green': {:?}",
        st.error
    );

    // Coverage: 65% fails the full 80% gate but passes the lite 60% one.
    std::fs::write(good.join("test.sh"), good_suite).unwrap();
    std::fs::write(good.join("coverage.txt"), "coverage: 65.0%\n").unwrap();
    let st = run_tests_gate_blocking(
        SandboxManager::with_id(&format!("s15-cov-{}", std::process::id())),
        good.clone(),
        Arc::new(DummyLogger),
        false,
    );
    assert!(!st.passed, "65% must fail the 80% gate");
    assert!(
        st.error.as_deref().unwrap_or("").contains("below the 80%"),
        "expected the 80% verdict: {:?}",
        st.error
    );
    let st = run_tests_gate_blocking(
        SandboxManager::with_id(&format!("s15-lite-{}", std::process::id())),
        good.clone(),
        Arc::new(DummyLogger),
        true,
    );
    assert!(st.passed, "65% must pass the lite 60% gate: {:?}", st.error);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn test_perf_gate_verdict() {
    // 1.3x boundary: exactly 130 passes, 129 fails with the ratio shown.
    let (ok, detail) = perf_gate_verdict(Some(100.0), Some(130.0));
    assert!(ok, "{detail}");
    let (ok, detail) = perf_gate_verdict(Some(100.0), Some(129.0));
    assert!(!ok);
    assert!(detail.contains("1.29x"), "{detail}");
    // Baseline present, measurement missing: fail (no number, no gate).
    let (ok, detail) = perf_gate_verdict(Some(100.0), None);
    assert!(!ok);
    assert!(detail.contains("130"), "{detail}");
    // No baseline (single-shot / genesis measurement missing): skipped.
    assert!(perf_gate_verdict(None, Some(500.0)).0);
    assert!(perf_gate_verdict(None, None).0);
    // Degenerate baseline: skipped, never a division by zero.
    assert!(perf_gate_verdict(Some(0.0), Some(1.0)).0);
}

/// R23 D5: the ticket prompt carries the dollars and says turns are
/// not the limit.
#[test]
fn r23_d5_session_budget_line_has_the_dollars_and_no_turn_limit() {
    let line = session_budget_line(0.25);
    assert!(line.contains("$0.25"), "{line}");
    assert!(line.contains("Turns are not the limit"), "{line}");
    let uncapped = session_budget_line(0.0);
    assert!(uncapped.contains("Turns are not the limit"), "{uncapped}");
}

/// Zero model output + empty workspace is a provider/harness fault, not
/// a 0% verdict. The guard must stay narrow: eval-only runs and any run
/// with tokens or files are untouched (live regression: opencode quota
/// retries ranked as failed_tests with 0 tokens).
#[test]
fn test_zero_output_guard_stays_narrow() {
    assert!(is_zero_output_non_result(false, 0, 0));
    // Real output keeps its own status.
    assert!(!is_zero_output_non_result(false, 1, 0));
    assert!(!is_zero_output_non_result(false, 0, 1));
    assert!(!is_zero_output_non_result(false, 1500, 2));
    // Eval-only has no agent by design.
    assert!(!is_zero_output_non_result(true, 0, 0));
}

#[test]
fn test_stage_commit_parsing() {
    let log = "aaaa111 stage 0: MVP: RESP server [pass]\nbbbb222 stage 1: Expiry [fail]\ncccc333 not a stage line\n";
    assert_eq!(parse_stage_commit(log, 0).as_deref(), Some("aaaa111"));
    assert_eq!(parse_stage_commit(log, 1).as_deref(), Some("bbbb222"));
    assert_eq!(parse_stage_commit(log, 2), None);
}

#[test]
fn test_coverage_parsers() {
    // go tool cover -func tail
    assert_eq!(
        parse_go_coverage_total("total:\t(statements)\t87.5%"),
        Some(87.5)
    );
    assert_eq!(
        parse_go_coverage_total("sdb-kv/main.go:12:\tfoo\t90.0%\ntotal:\t(statements)\t79.9%"),
        Some(79.9)
    );
    assert_eq!(parse_go_coverage_total("no total here"), None);
    // self-reported
    assert_eq!(parse_self_coverage("coverage: 81.2%\n"), Some(81.2));
    assert_eq!(parse_self_coverage("# Coverage\nlines: 80.0%"), None);
    assert_eq!(parse_self_coverage("Coverage summary: 100%"), Some(100.0));
    assert_eq!(parse_self_coverage("nothing"), None);
}

#[tokio::test]
async fn test_pipeline_cancelled_early() {
    let cancel = Arc::new(AtomicBool::new(true));
    let config = BenchmarkConfig {
        model: "test_model".to_string(),
        task: TaskType::Sql,
        effort: "low".to_string(),
        max_turns: 1,
        budget_usd: 0.10,
        timeout_min: 1,
        api_key: None,
        workdir: std::env::temp_dir().join(format!("test_pipe_cancel_{}", std::process::id())),
        eval_only: true,
        run_id: Some("test_cancel_run".to_string()),
        save_results: false,
        review_model: None,
        worker: 14,
        harness: Harness::Omp,
        feedback_rounds: 0,
        campaign_stage: None,
        baseline_rps: None,
    };

    let res = BenchmarkPipeline::execute(config, Arc::new(DummyLogger), Some(cancel)).await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("cancelled"));
}

#[tokio::test]
async fn test_pipeline_buffers_and_writes_console_log() {
    let temp_workdir =
        std::env::temp_dir().join(format!("test_pipe_buf_ws_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_workdir);

    let run_id = format!("test_pipe_buf_run_{}", std::process::id());
    let config = BenchmarkConfig {
        model: "test_model".to_string(),
        task: TaskType::Sql,
        effort: "low".to_string(),
        max_turns: 1,
        budget_usd: 0.10,
        timeout_min: 1,
        api_key: None,
        workdir: temp_workdir.clone(),
        eval_only: true,
        run_id: Some(run_id.clone()),
        save_results: true,
        review_model: None,
        // Dedicated worker 114: this test only asserts the archive path;
        // workers 0..7 belong to live campaign cells on this host, so
        // keep test traffic on its own offset.
        worker: 114,
        harness: Harness::Omp,
        feedback_rounds: 0,
        campaign_stage: None,
        baseline_rps: None,
    };

    struct CaptureLogger(std::sync::Mutex<Vec<String>>);
    impl PipelineLogger for CaptureLogger {
        fn log(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
    }

    let logger = Arc::new(CaptureLogger(std::sync::Mutex::new(Vec::new())));
    let res = BenchmarkPipeline::execute(config, logger.clone(), None).await;
    assert!(res.is_ok());
    assert!(!logger.0.lock().unwrap().is_empty());

    let runs_dir = RunArchiver::resolve_runs_dir();
    let console_log_path = runs_dir.join(&run_id).join("console.log");
    assert!(console_log_path.exists());
    let content = fs::read_to_string(&console_log_path).unwrap();
    assert!(!content.is_empty());
    assert!(content.contains("[PIPELINE] Starting benchmark run"));
    let first_line = content.lines().next().unwrap_or("");
    assert!(
        first_line.starts_with('[') && first_line.contains("] [PIPELINE] Starting benchmark run")
    );

    // Clean up, including the probe's DB rows (parents + runs tables).
    let _ = fs::remove_dir_all(&temp_workdir);
    let _ = fs::remove_dir_all(runs_dir.join(&run_id));
    let results_dir = get_repo_root().join("results");
    let _ = fs::remove_file(results_dir.join(format!("{}.json", run_id)));
    if let Ok(store) = crate::report::Store::open(&crate::report::Store::resolve_db_path()) {
        let _ = store.delete_parent(&run_id);
        let _ = store.delete_run_manifest(&run_id);
    }
}

#[test]
fn test_format_feedback_prompt() {
    let stages = vec![
        crate::verifier::StageResult {
            stage: 1,
            name: "PING / PONG".to_string(),
            passed: true,
            error: None,
        },
        crate::verifier::StageResult {
            stage: 2,
            name: "SET / GET".to_string(),
            passed: false,
            error: Some("expected +OK, got -ERR".to_string()),
        },
    ];
    let prompt = BenchmarkPipeline::format_feedback_prompt(
        TaskType::Sql,
        1,
        2,
        &stages,
        "server started on port 5480\ncommand error: SELECT foo",
    );
    assert!(prompt.contains("# Automated Verification Failure Report"));
    assert!(prompt.contains("sql"));
    assert!(prompt.contains("passed 1 of 2 stages (50.0%)"));
    assert!(prompt.contains("[PASS] Stage 1: PING / PONG"));
    assert!(prompt.contains("[FAIL] Stage 2: SET / GET"));
    assert!(prompt.contains("Diagnostic details: expected +OK, got -ERR"));
    assert!(prompt.contains("server started on port 5480"));
    assert!(prompt.contains("Directives for Surgical Bug Fixing:"));
    assert!(prompt.contains("DO NOT wipe your workspace"));
}

#[test]
fn test_output_tail_trims_and_respects_char_boundaries() {
    assert_eq!(output_tail("  hello  ", 10), "hello");
    assert_eq!(output_tail("hello world", 5), "…world");
    // A cut landing inside a multi-byte char walks forward to the next
    // boundary ("héllo" = 6 bytes: cutting at 3 keeps "llo").
    assert_eq!(output_tail("héllo", 3), "…llo");
    // Entire tail is one partial char: the marker stands alone, never a
    // broken UTF-8 slice.
    assert_eq!(output_tail("h€", 2), "…");
    assert_eq!(output_tail("", 4), "");
}

#[test]
fn test_hash_seed_is_stable_and_hex_shaped() {
    let a = hash_seed("redis_test_model_20261014");
    assert_eq!(a, hash_seed("redis_test_model_20261014"));
    assert_ne!(a, hash_seed("redis_test_model_20261015"));
    assert_eq!(a.len(), 8);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_git_in_success_and_failure() {
    let dir = covtest_dir("git");
    fs::create_dir_all(&dir).unwrap();
    // Not a repo: the failure must name the git args for diagnosis.
    let err = git_in(&dir, &["rev-parse", "HEAD"]).unwrap_err();
    assert!(err.contains("rev-parse"), "{err}");
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(init.status.success());
    let inside = git_in(&dir, &["rev-parse", "--is-inside-work-tree"]).unwrap();
    assert_eq!(inside.trim(), "true");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_buffering_logger_honors_cancel_filter() {
    let inner = Arc::new(RecordingLogger::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let buf = BufferingPipelineLogger::new(inner.clone(), Some(cancel.clone()));
    buf.log("normal before cancel");
    cancel.store(true, Ordering::SeqCst);
    buf.log("suppressed while cancelled");
    buf.log("[USER ACTION] still shown");
    buf.log("[DONE] marker");
    buf.log("[STOP] marker");
    buf.log("[PIPELINE] Run cancelled before archiving.");
    buf.log("[PIPELINE] Cancellation reason");
    assert!(inner.contains("normal before cancel"));
    assert!(!inner.contains("suppressed while cancelled"));
    assert!(inner.contains("[USER ACTION] still shown"));
    assert!(inner.contains("[DONE] marker"));
    assert!(inner.contains("[STOP] marker"));
    assert!(inner.contains("Run cancelled before archiving"));
    assert!(inner.contains("Cancellation reason"));
    // Buffer mirrors the forwarded lines, timestamp-prefixed.
    let logs = buf.get_logs();
    assert!(logs.contains("normal before cancel"));
    assert!(!logs.contains("suppressed while cancelled"));
    assert!(logs
        .lines()
        .any(|l| l.starts_with('[') && l.contains("] normal before cancel")));
}

#[test]
fn test_format_feedback_prompt_zero_stages_and_tail_limit() {
    // Zero totals: the percentage guard must not divide by zero.
    let p = BenchmarkPipeline::format_feedback_prompt(TaskType::Sql, 0, 0, &[], "");
    assert!(p.contains("passed 0 of 0 stages (0.0%)"));
    assert!(!p.contains("Service Container Runtime Output"));
    // Container logs are tail-capped at 60 lines: older lines drop off.
    let logs: Vec<String> = (0..80).map(|i| format!("line {i:02}")).collect();
    let p = BenchmarkPipeline::format_feedback_prompt(TaskType::Sql, 1, 2, &[], &logs.join("\n"));
    assert!(p.contains("line 79"));
    assert!(p.contains("line 20"));
    assert!(!p.contains("line 19"));
}

/// Empty workspace: no start.sh/Dockerfile -> verification is skipped,
/// no container is built, and the verdict stays a (0%, 6-stage) trial.
#[tokio::test]
async fn test_pipeline_eval_only_no_runnable_workspace() {
    let workdir = covtest_dir("norun");
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("notes.txt"), "no server here").unwrap();
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(
            covtest_config(workdir.clone(), 91, "norun"),
            logger.clone(),
            None,
        ),
        &logger,
        "pipeline completes even without a candidate",
    )
    .await;
    assert_proof_ok(&out, &logger);
    assert!(!out.disqualified && !out.agent_failed);
    assert_eq!(out.manifest.status, "failed_tests");
    assert_eq!(out.manifest.pass_rate, 0.0);
    assert_proof_score(&out, &logger, 0, 21);
    assert!(out.manifest.stages.is_empty());
    assert!(logger.contains("No runnable start.sh or Dockerfile found"));
    cleanup_covtest_network("norun");
    let _ = fs::remove_dir_all(&workdir);
}

/// Forbidden base image -> disqualified before any container work, with
/// the compliance stage carrying the reason and the cheat status.
#[tokio::test]
async fn test_pipeline_eval_only_disqualified_workspace() {
    let workdir = covtest_dir("dq");
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("Dockerfile"), "FROM redis:alpine\n").unwrap();
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(
            covtest_config(workdir.clone(), 92, "dq"),
            logger.clone(),
            None,
        ),
        &logger,
        "disqualified run still returns Ok",
    )
    .await;
    assert!(out.disqualified);
    assert_proof_ok(&out, &logger);
    assert_eq!(out.manifest.status, "disqualified_cheat");
    assert_eq!(out.manifest.pass_rate, 0.0);
    assert_proof_score(&out, &logger, 0, 21);
    assert_eq!(out.manifest.stages.len(), 1);
    assert_eq!(out.manifest.stages[0].stage, 0);
    assert!(out.manifest.stages[0]
        .error
        .as_deref()
        .unwrap_or("")
        .contains("forbidden base image"));
    assert!(logger.contains("[ANTI-CHEAT VIOLATION]"));
    cleanup_covtest_network("dq");
    let _ = fs::remove_dir_all(&workdir);
}

/// sdb-sql stages 0-3 live proof (eval-only, no model): the Python
/// sqlite3 reference adapter (`verifier::sql::ref_server`) driven
/// through the real pipeline with acceptance [0, 1, 2, 3]. All four
/// stage scripts run against one long-lived server, which also proves
/// the seed-namespaced schemas cannot collide.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stages_0_3_live() {
    let worker = 112u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqllive");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stages 0-3 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqllive");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 3,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: Some("# SPEC.md fixture\n".to_string()),
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "sdb-sql stages 0-3 must complete",
    )
    .await;
    assert!(
        !out.infra_error,
        "unexpected infra error: {:?}",
        out.manifest.status
    );
    for needle in [
        "Stage 0: Genesis: tables, insert, select -> PASS",
        "Stage 1: Expressions and WHERE -> PASS",
        "Stage 2: Sorting and paging -> PASS",
        "Stage 3: UPDATE and DELETE -> PASS",
    ] {
        assert_logged(&logger, needle);
    }
    assert_proof_score(&out, &logger, 4, 4);
    // The harness-owned contract lands in the prepared workspace.
    assert_eq!(
        fs::read_to_string(workdir.join("SPEC.md")).unwrap(),
        "# SPEC.md fixture\n"
    );
    cleanup_covtest_network("sqllive");
    let _ = fs::remove_dir_all(&dir);
}

use crate::verifier::sql::perfscore::{PerfScore, WorkloadResult};

fn perf_fixture(ratio: f64, score: Option<f64>) -> PerfScore {
    PerfScore {
        workloads: vec![WorkloadResult {
            id: "W1".to_string(),
            metric: "latency".to_string(),
            reference: 1.0,
            candidate: ratio,
            ratio,
            correct: true,
            numbers: Default::default(),
            note: None,
            harness_fault: false,
        }],
        score,
        error: None,
    }
}

fn perf_tolerance(score: &PerfScore) -> Result<(), String> {
    let mut bad = Vec::new();
    for w in &score.workloads {
        if (w.ratio - 1.0).abs() > 0.1 {
            bad.push(format!(
                "{} ratio {:.3} (correct {}) note {:?}: {:?}",
                w.id, w.ratio, w.correct, w.note, w.numbers
            ));
        }
    }
    match score.score {
        Some(value) if (value - 1.0).abs() <= 0.1 => {}
        Some(value) => bad.push(format!("score {value:.3}: {:?}", score.workloads)),
        None => bad.push(format!("score missing: {:?}", score.workloads)),
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

fn loadavg_text() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|e| format!("unreadable ({e})"))
}

/// R60 F1/R61 F9: one measurement inside the tolerance, or one re-measure
/// after a miss. The closure returns a `PerfScore` or its error. A first
/// miss is printed to stderr before the re-measure.
fn perf_self_check<F>(mut run: F) -> Result<(), String>
where
    F: FnMut() -> Result<PerfScore, String>,
{
    let mut failures = Vec::new();
    for attempt in 1..=2 {
        let before = loadavg_text();
        let result = run();
        let after = loadavg_text();
        let message = match result {
            Ok(score) => match perf_tolerance(&score) {
                Ok(()) => return Ok(()),
                Err(msg) => msg,
            },
            Err(e) => e,
        };
        failures.push(format!(
            "run {attempt}: {message}\nloadavg before: {before}\nloadavg after: {after}"
        ));
        if attempt == 1 {
            eprintln!("{}", failures.last().unwrap());
        }
    }
    Err(failures.join("\n\n"))
}

#[test]
fn r60_f1_first_run_in_tolerance_measures_once() {
    let mut calls = 0;
    let result = perf_self_check(|| {
        calls += 1;
        Ok(perf_fixture(1.0, Some(1.0)))
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(calls, 1);
}

#[test]
fn r60_f1_one_remeasure_passes() {
    let mut calls = 0;
    let result = perf_self_check(|| {
        calls += 1;
        if calls == 1 {
            Ok(perf_fixture(0.8, Some(1.0)))
        } else {
            Ok(perf_fixture(1.0, Some(1.0)))
        }
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(calls, 2);
}

#[test]
fn r60_f1_two_misses_fail_with_both_runs() {
    let mut calls = 0;
    let result = perf_self_check(|| {
        calls += 1;
        Ok(perf_fixture(0.8, Some(0.8)))
    });
    let err = result.unwrap_err();
    assert_eq!(calls, 2);
    assert!(err.contains("run 1:"), "{err}");
    assert!(err.contains("run 2:"), "{err}");
    assert!(err.contains("W1 ratio 0.800"), "{err}");
    assert!(err.contains("loadavg"), "{err}");
}

#[test]
fn r60_f1_score_counts() {
    let missing = perf_self_check(|| Ok(perf_fixture(1.0, None))).unwrap_err();
    assert!(missing.contains("score missing"), "{missing}");
    let low = perf_self_check(|| Ok(perf_fixture(1.0, Some(0.8)))).unwrap_err();
    assert!(low.contains("score 0.800"), "{low}");
}

/// R45 F1 acceptance (live): the reference against itself scores
/// 1.0 ± 0.1 on every workload over 5 alternating rounds. Both sides
/// run in the sandbox (2 CPUs, 2 GiB) through the production runner.
/// R59-1: a loaded host can move one run; the test re-measures once
/// on a fresh directory and fails only if the second run misses too.
#[test]
fn r45_f1_reference_scores_one() {
    use crate::verifier::sql::perfscore::{run_workspace_at, PerfConfig};
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let mut attempt = 0;
    let result = perf_self_check(|| {
        attempt += 1;
        let dir = covtest_dir(&format!("perfref_{attempt}"));
        let workdir = dir.join("workspace");
        crate::verifier::sql::ref_server::write_reference(&workdir, 5480)
            .map_err(|e| format!("reference workspace: {e}"))?;
        run_workspace_at(&workdir, 4, &PerfConfig::default(), &dir.join("runs"))
            .map_err(|e| format!("reference run: {e}"))
    });
    result.expect("the reference workspace must score within tolerance");
}

/// R49 F2 (live): a reference with `synchronous=OFF` as the candidate
/// floors W4 on the real device flush counter, while the FULL
/// reference passes its own check and the phase keeps a score.
#[test]
fn r49_f2_unsynced_candidate_floors_live() {
    // The lock line is first so scripts/live-proofs.sh names this test,
    // not the helper fns below it.
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    use crate::verifier::sql::perfscore::{run_pair_with_flush, DeviceFlushCounter, PerfConfig};
    use std::process::{Command, Stdio};
    struct RefProc(std::process::Child);
    impl Drop for RefProc {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
    fn start(script: &std::path::Path, port: u16, data: &std::path::Path, sync: &str) -> RefProc {
        let child = Command::new("python3")
            .arg("-u")
            .arg(script)
            .args(["--port", &port.to_string(), "--data"])
            .arg(data)
            .env("SDB_SQL_SYNCHRONOUS", sync)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the reference");
        let proc = RefProc(child);
        crate::verifier::sql::driver::wait_until_answering(
            &format!("127.0.0.1:{port}"),
            std::time::Duration::from_secs(10),
        )
        .expect("the reference answers");
        proc
    }
    let dir = covtest_dir("perfunsync");
    let ws = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&ws, 5480).unwrap();
    let ref_data = dir.join("ref-data");
    let cand_data = dir.join("cand-data");
    fs::create_dir_all(&ref_data).unwrap();
    fs::create_dir_all(&cand_data).unwrap();
    let ref_port = free_port();
    let cand_port = free_port();
    let script = ws.join("sql_server.py");
    let _ref = start(&script, ref_port, &ref_data, "FULL");
    let _cand = start(&script, cand_port, &cand_data, "OFF");
    let ref_flush = DeviceFlushCounter::for_dir(&ref_data).expect("reference flush counter");
    let cand_flush = DeviceFlushCounter::for_dir(&cand_data).expect("candidate flush counter");
    let cfg = PerfConfig {
        rounds: 2,
        rows_w1: 1_000,
        lookups: 20,
        w4_seconds: 0.5,
        timeout: std::time::Duration::from_secs(15),
        workload_deadline: std::time::Duration::from_secs(60),
        ..PerfConfig::small_for_tests()
    };
    let score = run_pair_with_flush(
        &format!("127.0.0.1:{ref_port}"),
        &format!("127.0.0.1:{cand_port}"),
        1,
        &cfg,
        Some(&ref_flush),
        Some(&cand_flush),
    );
    let w4 = score
        .workloads
        .iter()
        .find(|w| w.id == "W4")
        .expect("phase 1 runs W4");
    assert!(
        !w4.correct,
        "synchronous=OFF must floor W4: {:?}",
        w4.numbers
    );
    assert!(
        w4.note.as_deref().unwrap_or("").contains("not durable"),
        "{:?}",
        w4.note
    );
    assert!(
        score.score.is_some(),
        "the candidate floor keeps the phase scored: {:?}",
        score.error
    );
}

/// R45 F5 acceptance (live): the CLI path measures a finished
/// workspace and prints a score of 1.0 ± 0.1. The small config keeps
/// the phase-4 shape (all five workloads) without the 30 s W5.
#[test]
fn r45_f5_perfscore_cli() {
    use crate::verifier::sql::perfscore::{cli_with_at, render, PerfConfig};
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = covtest_dir("perfcli");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, 5480).unwrap();
    let score = cli_with_at(
        &workdir,
        &PerfConfig::fixture_for_tests(),
        &dir.join("runs"),
    )
    .expect("the reference workspace must start and score");
    let text = render(&score);
    assert!(text.contains("perf score"), "{text}");
    assert_eq!(score.workloads.len(), 5, "{text}");
    let value = score.score.unwrap_or_else(|| {
        panic!(
            "the reference must score: error {:?}; {:?}",
            score.error, score.workloads
        )
    });
    assert!((value - 1.0).abs() <= 0.1, "score {value:.3}\n{text}");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage 4 live proof (eval-only, no model): the reference
/// passes the cumulative suite through stage 4 — the differential
/// script plus the persistence scenario, which restarts the candidate
/// container on the same harness-owned store — and leaves a sentinel
/// for later tickets.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage4_persistence_live() {
    let worker = 75u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlpersist");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-4 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlpersist");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 4,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 4],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-4 fixture run must complete",
    )
    .await;
    assert!(
        !out.infra_error,
        "unexpected infra error: {:?}",
        out.manifest.status
    );
    for needle in [
        "Stage 0: Genesis: tables, insert, select -> PASS",
        "Stage 4: Persistence -> PASS",
        "  [UPGRADE] sentinel for stage 4 written",
    ] {
        assert_logged(&logger, needle);
    }
    assert_proof_score(&out, &logger, 5, 5);
    assert!(
        dir.join("sentinel-4.json").is_file(),
        "the sentinel must be saved next to the store"
    );
    cleanup_covtest_network("sqlpersist");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage 5 live proof (eval-only, no model): the reference
/// survives the crash ladder under Docker — acked rows across a kill
/// -9, a mid-flight 50k-row UPDATE, a torn newest file, and a double
/// restart — and the stage result stays green.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage5_crash_live() {
    let worker = 80u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlcrash");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-5 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlcrash");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 5,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 5],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-5 fixture run must complete",
    )
    .await;
    assert!(
        !out.infra_error,
        "unexpected infra error: {:?}",
        out.manifest.status
    );
    for needle in [
        "Stage 5: Crash safety -> PASS",
        "[CRASH] 40 acked rows",
        "[CRASH] the killed bulk UPDATE was all-or-nothing",
        "[CRASH] a double restart changed nothing",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlcrash");
    let _ = fs::remove_dir_all(&dir);
}

/// Stage 5's lite variant (proposal: acked rows survive kill -9; no
/// torn tail or mid-statement atomicity) runs the narrowed ladder: the
/// reference passes and the bulk/torn checks do not run.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage5_lite_live() {
    let worker = 85u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlcrashlite");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-5 lite fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlcrashlite");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 5,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 5],
        lite_profile: vec!["crash-acked-only".to_string()],
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-5 lite fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 5: Crash safety -> PASS",
        "[CRASH] lite variant: acked rows and restarts only",
        "[CRASH] 40 acked rows",
        "[CRASH] a double restart changed nothing",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    for absent in [
        "[CRASH] the killed bulk UPDATE was all-or-nothing",
        "torn bytes",
    ] {
        assert!(
            !logger.contains(absent),
            "the lite variant must not run {absent:?}"
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlcrashlite");
    let _ = fs::remove_dir_all(&dir);
}

/// A stage-6 ticket after the model won stage 5 on the lite variant:
/// the suite keeps grading stage 5 lite (the full crash ladder would
/// demand work ticket 5 never asked for) while stage 6 runs in full.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage6_after_lite_stage5() {
    let worker = 87u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqltxnlite5");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-6 fixture after lite stage 5").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqltxnlite5");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 6,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 4, 5, 6],
        lite_profile: Vec::new(),
        passed_lite: vec![(5, vec!["crash-acked-only".to_string()])],
        passed_stages: (0..=5).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-6 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 5: Crash safety -> PASS",
        "Stage 6: Transactions -> PASS",
        "[CRASH] lite variant: acked rows and restarts only",
        "[TXN] a kill with an open transaction discarded it",
    ] {
        assert_logged(&logger, needle);
    }
    for absent in [
        "[CRASH] the killed bulk UPDATE was all-or-nothing",
        "torn bytes",
    ] {
        assert!(
            !logger.contains(absent),
            "stage 5 must stay lite in a later ticket: {absent:?}"
        );
    }
    assert_proof_score(&out, &logger, 7, 7);
    cleanup_covtest_network("sqltxnlite5");
    let _ = fs::remove_dir_all(&dir);
}

/// Stage 7 after a failed storage tier: the acceptance list carries
/// only the *passed* stages plus the current ticket, so stage 7's suite
/// runs the aggregate grammar against stages 0-4 and never touches the
/// crash ladder or the transaction checks (finding: the raw declared
/// list made one storage failure sink every later ticket).
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage7_without_storage_stages() {
    let worker = 100u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlno5");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-7 fixture without stage 5").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlno5");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 7,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 4, 7],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: vec![0, 1, 2, 3, 4],
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-7 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    assert!(
        logger.contains("Stage 7: Aggregates -> PASS"),
        "missing the stage-7 pass; log:\n{}",
        logger.text()
    );
    for absent in ["[CRASH]", "[TXN]"] {
        assert!(
            !logger.contains(absent),
            "{absent} ran although stage 5/6 are not in the suite"
        );
    }
    assert_proof_score(&out, &logger, 6, 6);
    cleanup_covtest_network("sqlno5");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage-11 live proof (eval-only, no model): the reference
/// server driven through the real pipeline with acceptance [0..11],
/// including the perf gate (two 200k-row tables, an indexed copy, 500
/// alternating lookups, a restart, and the measured ratios).
/// Ticket 13 live proof: the reference (SQLite) runs the whole accepted
/// suite through stage 13, including the ALTER-heavy differential
/// script, the schema-change stage and the cumulative extras.
/// Ticket 14 live proof: the reference answers the plan and the counter
/// statements, and the cumulative suite through stage 14 passes.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage14_observability_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 42u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqlobs");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqlobs");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-14 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlobs");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 14,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 14],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..14).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-14 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 14: Observability -> PASS",
        "[OBSERVABILITY] EXPLAIN names the index/scan",
        "[UPGRADE] sentinel for stage 14 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlobs");
    let _ = fs::remove_dir_all(&dir);
}

/// R27 F4: stage-12 full live proof. The reference adapter runs the
/// cumulative suite through stage 12; the concurrency scenario checks
/// lost update, transfers and non-blocking readers.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage12_concurrency_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 30u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqlconc");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqlconc");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-12 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlconc");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 12,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 12],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..12).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-12 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 12: Concurrency and isolation -> PASS",
        "[CONCURRENCY] serializable: lost update, transfers, and readers never blocked",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlconc");
    let _ = fs::remove_dir_all(&dir);
}

/// R27 F4: stage-12 lite live proof. The global-lock profile must pass
/// the lost-update and transfer checks; it does not claim non-blocking
/// readers.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage12_concurrency_lite_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 28u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqlconclite");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqlconclite");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-12 lite fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlconclite");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 12,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 12],
        lite_profile: vec!["concurrency-global-lock".to_string()],
        passed_lite: Vec::new(),
        passed_stages: (0..12).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-12 lite fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 12: Concurrency and isolation -> PASS",
        "[CONCURRENCY] lite: lost update and transfers are serializable (global lock)",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlconclite");
    let _ = fs::remove_dir_all(&dir);
}

/// Ticket 15's gate needs `./test.sh`, a coverage line and stage-7/8
/// commits; stage 16's acceptance includes ticket 15, so both live
/// proofs install the same fixture. `sql_server.py` stays byte-identical
/// to the harness reference (the anti-cheat check recognizes it).
fn install_tests_ci_fixture(workdir: &std::path::Path, port: u16) {
    install_tests_ci_fixture_with_coverage(workdir, port, 85.0);
}

/// R27 F4: the same stage-15 fixture with a chosen self-reported
/// coverage, so the lite proof can show the 60% gate accepting a value
/// the full 80% gate would reject.
fn install_tests_ci_fixture_with_coverage(workdir: &std::path::Path, port: u16, coverage: f64) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(workdir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "ref@example.com"]);
    git(&["config", "user.name", "reference"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "stage 6: Transactions [pass]"]);
    // The feature commits carry files the suite greps for; the reference
    // adapter itself stays untouched.
    std::fs::write(workdir.join("feature_aggregates.py"), "AGG = True\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "stage 7: Aggregates [pass]"]);
    std::fs::write(workdir.join("feature_joins.py"), "JOIN = True\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "stage 8: Joins [pass]"]);
    std::fs::write(
        workdir.join("coverage.txt"),
        format!("coverage: {coverage:.1}%\n"),
    )
    .unwrap();
    let suite = format!(
        "#!/bin/sh\n\
             set -eu\n\
             grep -q '^AGG = True' feature_aggregates.py\n\
             grep -q '^JOIN = True' feature_joins.py\n\
             ./start.sh &\n\
             pid=$!\n\
             trap 'kill $pid 2>/dev/null || true' EXIT\n\
             python3 - {port} <<'PY'\n\
             import socket, sys, time\n\
             port = int(sys.argv[1])\n\
             for _ in range(100):\n\
             \x20   try:\n\
             \x20       s = socket.create_connection((\"127.0.0.1\", port), 1)\n\
             \x20       break\n\
             \x20   except OSError:\n\
             \x20       time.sleep(0.05)\n\
             else:\n\
             \x20   raise SystemExit(\"server did not start\")\n\
             s.sendall(b\"SELECT 1;\\n\")\n\
             line = s.makefile(\"rb\").readline()\n\
             s.close()\n\
             if not line.startswith(b\"ROWS\"):\n\
             \x20   raise SystemExit(\"unexpected answer: %r\" % line)\n\
             PY\n"
    );
    std::fs::write(workdir.join("test.sh"), suite).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            workdir.join("test.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

/// Ticket 15 live proof (eval-only, no model): the reference workspace
/// ships an offline `test.sh`, a coverage line and stage-7/8 commits; the
/// real pipeline runs the cumulative suite through stage 15 and the gate
/// (two green runs, coverage, both sensitivity reverts) passes.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage15_tests_ci_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 45u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqltests");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqltests");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-15 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqltests");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 15,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 15],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..15).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-15 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 15: Tests and CI gate -> PASS",
        "[TESTS] ./test.sh green (run 1/2, offline, <= 120s)",
        "[TESTS] ./test.sh green (run 2/2, offline, <= 120s)",
        "[TESTS] coverage 85.0% (self-report, not verified; gate 80%)",
        "[TESTS] sensitivity ok (stage-7 diff reversed)",
        "[TESTS] sensitivity ok (stage-8 diff reversed)",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqltests");
    let _ = fs::remove_dir_all(&dir);
}

/// R27 F4: stage-15 lite live proof. The fixture self-reports 65%
/// coverage: the lite 60% gate accepts it, where the full 80% gate
/// would fail it. The suite and both sensitivity reverts still pass.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage15_tests_ci_lite_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 43u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqltestslite");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqltestslite");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture_with_coverage(&workdir, port, 65.0);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-15 lite fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqltestslite");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 15,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 15],
        lite_profile: vec!["coverage-60".to_string()],
        passed_lite: Vec::new(),
        passed_stages: (0..15).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-15 lite fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 15: Tests and CI gate -> PASS",
        "[TESTS] coverage 65.0% (self-report, not verified; gate 60%)",
        "[TESTS] sensitivity ok (stage-7 diff reversed)",
        "[TESTS] sensitivity ok (stage-8 diff reversed)",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqltestslite");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn suite_variants_carry_the_lite_grammar_into_later_suites() {
    let passed_lite = vec![(8u32, vec!["join-inner-only".to_string()])];
    let wanted: Vec<u32> = (0..=19).collect();
    let variants = suite_variants(&passed_lite, &["two-nodes-promote".to_string()], &wanted);
    assert_eq!(
        variants,
        vec![
            (8u32, vec!["join-inner-only".to_string()]),
            (19u32, vec!["two-nodes-promote".to_string()]),
        ]
    );
    assert_eq!(
        suite_variants(&passed_lite, &[], &wanted),
        vec![(8u32, vec!["join-inner-only".to_string()])]
    );
}

#[test]
fn repl_control_failures_split_harness_faults_from_crashes() {
    use crate::verifier::sql::replication::HaAction;
    // Docker failed on a node that is still up, or could not say.
    assert!(repl_control_failure_is_infra(
        HaAction::Pause(1),
        Some("running")
    ));
    assert!(repl_control_failure_is_infra(
        HaAction::Resume(1),
        Some("paused")
    ));
    assert!(repl_control_failure_is_infra(HaAction::Kill(0), None));
    // The candidate's node already exited: a model failure.
    assert!(!repl_control_failure_is_infra(
        HaAction::Kill(0),
        Some("exited")
    ));
    assert!(!repl_control_failure_is_infra(
        HaAction::Pause(2),
        Some("dead")
    ));
    // A rejoin recreates the container: always the harness.
    assert!(repl_control_failure_is_infra(
        HaAction::Rejoin {
            node: 0,
            primary: 1
        },
        Some("exited")
    ));
}

/// Ticket 16 live proof (eval-only, no model): the reference runs the
/// cumulative suite through stage 16; the memory extra restarts it on a
/// fresh scratch store under a 64 MiB cap, loads 400k x 900 B rows and
/// checks the full aggregate, the external sort and the indexed join
/// without being OOM-killed.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage16_memory_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 51u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqlmemory");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqlmemory");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-16 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlmemory");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 16,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 16],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..16).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-16 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 16: Larger than memory -> PASS",
        "[MEMORY] gate runs on a fresh scratch store under a 64 MiB cap",
        "[MEMORY] 400000 rows (~343 MiB of text)",
        "aggregate, ORDER BY and indexed join exact",
        "[UPGRADE] sentinel for stage 16 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlmemory");
    let _ = fs::remove_dir_all(&dir);
}

/// Ticket 17 live proof (eval-only, no model): the reference runs the
/// cumulative suite through stage 17; the incident extra restarts it on
/// a 16 MiB tmpfs, fills the volume through the harness, checks that a
/// too-large write is refused without being acked or applied, re-reads
/// the seeds, frees the filler and retries the write successfully.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage17_disk_full_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 55u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqldisk");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqldisk");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-17 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqldisk");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 17,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 17],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..17).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-17 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 17: Incident: disk full -> PASS",
        "[DISK] incident runs on a fresh 16 MiB tmpfs at /sdb-data",
        "[DISK] seed 40 rows; ENOSPC refused the write and never applied it",
        "the retry succeeded after the filler was removed",
        "[UPGRADE] sentinel for stage 17 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqldisk");
    let _ = fs::remove_dir_all(&dir);
}

/// Ticket 18 live proof (eval-only, no model): the reference runs the
/// cumulative suite through stage 18; the hardening extra restarts it
/// with a password on a fresh scratch store and drives AUTH, fuzzing,
/// the nesting/size/time limits, a slow-loris client and the
/// 256-connection cap.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage18_security_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 60u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    cleanup_covtest_network("sqlsec");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqlsec");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-18 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlsec");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 18,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 18],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..18).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-18 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
            "Stage 18: Hostile input and limits -> PASS",
            "[SECURITY] scenario runs on a fresh scratch store with SDB_SQL_PASSWORD set",
            "[SECURITY] auth enforced; fuzz/mutations survived; nesting, 16777216-byte, 5s and slow-loris limits held; 256-connection cap enforced",
            "[UPGRADE] sentinel for stage 18 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlsec");
    let _ = fs::remove_dir_all(&dir);
}

/// Ticket 19 live proof (eval-only, no model): three reference nodes,
/// a sync-acked transaction, a primary kill with election, a new write,
/// the old primary rejoined, and the cumulative suite on the winner.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage19_replication_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 65u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let ha_ports = [port + 60, port + 61, port + 62];
    cleanup_covtest_network("sqlrepl");
    for p in [port, ha_ports[0], ha_ports[1], ha_ports[2]] {
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={p}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
    }
    let dir = covtest_dir("sqlrepl");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-19 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlrepl");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 19,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 19],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..19).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-19 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 19: Replication and failover -> PASS",
        "[REPL] cluster of 3 nodes on ports",
        "[REPL] 3 nodes; the replicas were frozen across the COMMIT",
        "[REPL] the cumulative differential suite passes against the current primary",
        "[UPGRADE] sentinel for stage 19 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlrepl");
    let _ = fs::remove_dir_all(&dir);
}

/// Ticket 19 lite live proof: two nodes, kill the primary, manual
/// `PROMOTE`, then the cumulative suite on the promoted node.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage19_replication_lite_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 63u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let ha_ports = [port + 60, port + 61];
    cleanup_covtest_network("sqlrepllite");
    for p in [port, ha_ports[0], ha_ports[1]] {
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={p}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
    }
    let dir = covtest_dir("sqlrepllite");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-19-lite fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlrepllite");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 19,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 19],
        lite_profile: vec!["two-nodes-promote".to_string()],
        passed_lite: Vec::new(),
        passed_stages: (0..19).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-19 lite fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 19: Replication and failover -> PASS",
        "[REPL] cluster of 2 nodes on ports",
        "[REPL] 2 nodes; the replica was frozen across the COMMIT",
        "[REPL] the cumulative differential suite passes against the current primary",
        "[UPGRADE] sentinel for stage 19 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlrepllite");
    let _ = fs::remove_dir_all(&dir);
}

/// Ticket 20 live proof (eval-only, no model): the reference serves
/// PostgreSQL protocol 3.0 on a second port; the harness frontend and a
/// stock psql check SSL, startup/auth, simple + extended query,
/// transactions and SQLSTATE.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage20_pgwire_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 73u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let pg_port = port + 50;
    cleanup_covtest_network("sqlpg");
    for p in [port, pg_port] {
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={p}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
    }
    let dir = covtest_dir("sqlpg");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-20 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlpg");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 20,
        prompt_path,
        continuity: true,
        accepted_stages: (0..=20).collect(),
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..20).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-20 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 20: PostgreSQL wire protocol -> PASS",
        "Stage 19: Replication and failover -> PASS",
        "[PG] PostgreSQL wire scenario on port",
        "[PG] stock psql SELECT 1 -> 1",
        "[PG] SSL refused; startup/auth/ParameterStatus ok; simple + extended query",
        "[UPGRADE] sentinel for stage 20 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 21, 21);
    cleanup_covtest_network("sqlpg");
    let _ = fs::remove_dir_all(&dir);
}

/// Ticket 20 lite live proof: the simple-query protocol only (no
/// extended query), plus the psql cross-check.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage20_pgwire_lite_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 88u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let pg_port = port + 50;
    cleanup_covtest_network("sqlpglite");
    for p in [port, pg_port] {
        if let Ok(out) = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("publish={p}")])
            .output()
        {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                let mut args = vec!["rm".to_string(), "-f".to_string()];
                args.extend(ids);
                let _ = std::process::Command::new("docker").args(&args).output();
            }
        }
    }
    let dir = covtest_dir("sqlpglite");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    install_tests_ci_fixture(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-20-lite fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlpglite");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 20,
        prompt_path,
        continuity: true,
        accepted_stages: (0..=20).collect(),
        lite_profile: vec!["pg-simple".to_string()],
        passed_lite: Vec::new(),
        passed_stages: (0..20).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-20 lite fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
            "Stage 20: PostgreSQL wire protocol -> PASS",
            "[PG] PostgreSQL wire scenario on port",
            "[PG] stock psql SELECT 1 -> 1",
            "[PG] SSL refused; startup/auth/ParameterStatus ok; simple query, transactions and SQLSTATE",
            "[UPGRADE] sentinel for stage 20 written",
        ] {
            assert!(
                logger.contains(needle),
                "missing {needle:?}; log:\n{}",
                logger.text()
            );
        }
    assert_proof_score(&out, &logger, 21, 21);
    cleanup_covtest_network("sqlpglite");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage13_schema_change_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 33u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    // A previous failed run's reference holds this worker's host port;
    // a new run would silently talk to that stale binary. Clear it.
    cleanup_covtest_network("sqlschema");
    if let Ok(out) = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("publish={port}")])
        .output()
    {
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut args = vec!["rm".to_string(), "-f".to_string()];
            args.extend(ids);
            let _ = std::process::Command::new("docker").args(&args).output();
        }
    }
    let dir = covtest_dir("sqlschema");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-13 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlschema");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 13,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 13],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..13).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-13 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 13: Schema change -> PASS",
        "[UPGRADE] sentinel for stage 13 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlschema");
    let _ = fs::remove_dir_all(&dir);
}

/// The SQL live proofs through stage 11 each run the cumulative suite,
/// including the perf gate (which needs an idle host) and their own
/// reference container. Run them one at a time; three parallel gates
/// turn timing into noise.
static SQL_PERF_LIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
#[allow(clippy::await_holding_lock)] // a test-only serialization guard
async fn test_pipeline_eval_only_sql_stage11_indexes_live() {
    let _serial = SQL_PERF_LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let worker = 21u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlidx");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-11 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlidx");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 11,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 11],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..11).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-11 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    // gui-spec.md §5.1: every verification round is recorded in the
    // manifest (the initial try here; eval-only runs have no repairs).
    assert!(
        out.manifest
            .rounds
            .iter()
            .any(|r| r.round == 0 && !r.stage_results.is_empty()),
        "round 0 recorded with results: {:?}",
        out.manifest.rounds
    );
    for needle in [
        "Stage 11: Indexes + perf gate -> PASS",
        "[PERF] gate runs on a fresh scratch store",
        "[PERF] point p50",
        "after restart",
        "[UPGRADE] sentinel for stage 11 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlidx");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage 10 live proof (eval-only, no model): the reference
/// passes the subquery suite under Docker — scalar subqueries,
/// `[NOT] IN (subquery)`/`IN (list)`, correlated `[NOT] EXISTS` and
/// `INSERT ... SELECT` — plus everything 0-9 again.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage10_subqueries_live() {
    let worker = 8u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlsub");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-10 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlsub");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 10,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 10],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: (0..10).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-10 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 10: Subqueries -> PASS",
        "[UPGRADE] sentinel for stage 10 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlsub");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage 9 live proof (eval-only, no model): the reference
/// passes the constraint suite under Docker — constrained DDL, key
/// auto-assignment, defaults, UNIQUE/NOT NULL and foreign keys
/// (including the violation probes) — plus everything 0-8 again.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage9_constraints_live() {
    let worker = 105u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlcons");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-9 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlcons");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 9,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 9],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-9 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 9: Constraints -> PASS",
        "[UPGRADE] sentinel for stage 9 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlcons");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage 8 live proof (eval-only, no model): the reference
/// passes the join suite under Docker — comma/`INNER`/`LEFT` joins with
/// `ON`, qualified references, `SELECT *` over a join, the ambiguity
/// probe — plus everything 0-7 again.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage8_joins_live() {
    let worker = 102u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqljoin");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-8 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqljoin");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 8,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 8],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-8 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 8: Joins -> PASS",
        "[UPGRADE] sentinel for stage 8 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqljoin");
    let _ = fs::remove_dir_all(&dir);
}

/// Stage 11's lite variant through the pipeline: the attempt carries
/// `index-equality-only`, so the perf gate drops its range round and
/// only the equality point lookups are gated.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage11_lite_live() {
    let worker = 24u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlidxlite");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-11 lite fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlidxlite");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 11,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 11],
        lite_profile: vec!["index-equality-only".to_string()],
        passed_lite: Vec::new(),
        passed_stages: (0..11).collect(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-11 lite fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in ["Stage 11: Indexes + perf gate -> PASS", "[PERF] point p50"] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert!(
        !logger.contains("range p50"),
        "the lite gate must not measure ranges; log:\n{}",
        logger.text()
    );
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlidxlite");
    let _ = fs::remove_dir_all(&dir);
}

/// Stage 8's lite variant through the pipeline: the attempt carries
/// `join-inner-only`, so the graded suite keeps comma/inner joins and
/// never runs a `LEFT JOIN`.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage8_lite_live() {
    let worker = 104u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqljoinlite");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-8 lite fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqljoinlite");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 8,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 8],
        lite_profile: vec!["join-inner-only".to_string()],
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-8 lite fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in ["Stage 8: Joins -> PASS", "Stage 7: Aggregates -> PASS"] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqljoinlite");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage 7 live proof (eval-only, no model): the reference
/// passes the aggregate suite under Docker — `COUNT`/`SUM`/`MIN`/`MAX`,
/// grouped queries with `HAVING`, the empty-group contract and the
/// aggregate error probes — plus everything 0-6 again.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage7_aggregates_live() {
    let worker = 97u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlagg");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-7 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlagg");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 7,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 7],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-7 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 7: Aggregates -> PASS",
        "[UPGRADE] sentinel for stage 7 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqlagg");
    let _ = fs::remove_dir_all(&dir);
}

/// sdb-sql stage 6 live proof (eval-only, no model): the reference
/// passes the transactions suite under Docker — cross-connection
/// isolation (two connections, uncommitted rows invisible) and a kill
/// -9 with a transaction open.
#[tokio::test]
async fn test_pipeline_eval_only_sql_stage6_transactions_live() {
    let worker = 70u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqltxn");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql stage-6 fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqltxn");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 6,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 6],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the stage-6 fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    for needle in [
        "Stage 6: Transactions -> PASS",
        "[TXN] uncommitted rows stay invisible across connections",
        "[TXN] a kill with an open transaction discarded it",
        "[UPGRADE] sentinel for stage 6 written",
    ] {
        assert!(
            logger.contains(needle),
            "missing {needle:?}; log:\n{}",
            logger.text()
        );
    }
    assert_proof_score(&out, &logger, 5, 5);
    cleanup_covtest_network("sqltxn");
    let _ = fs::remove_dir_all(&dir);
}

/// A sentinel whose data is gone fails the stage that wrote it
/// (regression class `upgrade`): the check runs before the suite and
/// the stage result is overridden even though the suite itself passes.
#[tokio::test]
async fn test_pipeline_sql_sentinel_drift_fails_the_writing_stage() {
    let worker = 117u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqldrift");
    let workdir = dir.join("workspace");
    crate::verifier::sql::ref_server::write_reference(&workdir, port).unwrap();
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql sentinel-drift fixture").unwrap();
    // A sentinel the store never saw: its reads cannot answer.
    let sent = crate::verifier::sql::sentinel::build(
        0x5EED_0001,
        4,
        &crate::verifier::sql::generator::Profile::through(4),
    )
    .unwrap();
    crate::verifier::sql::sentinel::save(&dir.join("sentinel-4.json"), &sent).unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqldrift");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 4,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0, 1, 2, 3, 4],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the drift fixture run must complete",
    )
    .await;
    assert_proof_ok(&out, &logger);
    assert!(
        logger.contains("[UPGRADE] sentinel from stage 4 FAILED"),
        "the drift must be reported"
    );
    assert!(
        logger.contains("Stage 4: Persistence -> FAIL"),
        "the writing stage must fail"
    );
    assert!(
        logger.contains("[upgrade]"),
        "the stage error must carry the upgrade class"
    );
    // The suite itself passed: stages 0-3 are green, only stage 4 is red.
    assert_proof_score(&out, &logger, 4, 5);
    cleanup_covtest_network("sqldrift");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review finding 2: a failing sql stage must still get a *minimized*
/// repro. The shrink trials run on throwaway stores, so the replay is
/// trustworthy (with the old reset, the candidate's persistent store
/// answered `exists` and every report fell back to the unminimized
/// prefix). The real store must survive with only the initial and the
/// restore starts.
#[tokio::test]
async fn test_pipeline_eval_only_sql_shrink_uses_a_throwaway_store() {
    let worker = 118u8;
    let port = crate::web::server::worker_port(TaskType::Sql.default_port(), worker);
    let dir = covtest_dir("sqlshrink");
    let workdir = dir.join("workspace");
    write_always_ok_sql_mock(&workdir, port);
    let prompt_path = dir.join("ticket_prompt.md");
    fs::write(&prompt_path, "sdb-sql shrink fixture").unwrap();

    let mut config = covtest_config(workdir.clone(), worker, "sqlshrink");
    config.task = TaskType::Sql;
    config.campaign_stage = Some(crate::campaign::CampaignStageConfig {
        campaign: "covtest-sdb-sql".to_string(),
        stage: 0,
        prompt_path,
        continuity: true,
        accepted_stages: vec![0],
        lite_profile: Vec::new(),
        passed_lite: Vec::new(),
        passed_stages: Vec::new(),
        spec_md: None,
        preamble: None,
        cap_headroom_usd: None,
    });
    let logger = Arc::new(RecordingLogger::default());
    let out = expect_proof(
        BenchmarkPipeline::execute(config, logger.clone(), None),
        &logger,
        "the shrink fixture run must complete",
    )
    .await;
    assert!(
        !out.infra_error,
        "unexpected infra error: {:?}",
        out.manifest.status
    );
    assert!(
        logger.contains("Stage 0: Genesis: tables, insert, select -> FAIL"),
        "the always-OK candidate must fail stage 0"
    );

    // The shrink logged its work: trials > 0 and a small repro.
    let line = logger
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|l| l.contains("shrink trials"))
        .cloned()
        .expect("the sql arm must log the repro size");
    let toks: Vec<&str> = line.split_whitespace().collect();
    let statements: usize = toks[1].parse().expect("statement count");
    let trials: usize = toks[3]
        .trim_start_matches('(')
        .parse()
        .expect("trial count");
    assert!(trials > 0, "the shrink never ran: {line}");
    assert!(statements <= 8, "repro not minimized: {line}");

    // Throwaway stores live next to the real one, keyed per instance and
    // cleaned once the stage is graded; the real store saw only the
    // initial start plus the post-shrink restore, never a trial's rows.
    let markers = |p: &Path| -> usize {
        fs::read_dir(p)
            .map(|it| {
                it.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with("marker-"))
                    .count()
            })
            .unwrap_or(0)
    };
    let real = dir.join("sql-data");
    let real_markers = markers(&real);
    assert!(
        (1..=2).contains(&real_markers),
        "the real store was written by {real_markers} starts (expected initial + restore)"
    );
    let leftovers: Vec<String> = fs::read_dir(&dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("sql-shrink"))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "shrink stores must not accumulate: {leftovers:?}"
    );

    cleanup_covtest_network("sqlshrink");
    let _ = fs::remove_dir_all(&dir);
}

/// R56 F3: another live lane makes the shared key delta unattributable;
/// alone, the delta is kept.
#[test]
fn r56_f3_spend_delta_only_alone() {
    let dir = std::env::temp_dir().join(format!("r56_spend_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let ticks = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    fs::write(
        dir.join(".campaign.lock.w9"),
        serde_json::json!({
            "pid": std::process::id(),
            "campaign_id": "campaign_other_lane",
            "worker": 9,
            "start_ticks": ticks.parse::<u64>().unwrap(),
        })
        .to_string(),
    )
    .unwrap();
    let note = shared_key_note(&dir, 0);
    assert_eq!(note.as_deref(), Some("shared key: w9 live"));
    let (delta, note) = gate_spend_delta(Some(0.2), note);
    assert!(delta.is_none());
    assert_eq!(note.as_deref(), Some("shared key: w9 live"));

    fs::remove_file(dir.join(".campaign.lock.w9")).unwrap();
    let note = shared_key_note(&dir, 0);
    assert!(note.is_none());
    let (delta, note) = gate_spend_delta(Some(0.2), note);
    assert_eq!(delta, Some(0.2));
    assert!(note.is_none());
    let _ = fs::remove_dir_all(&dir);
}

/// R63 F2: the two stage-20 proofs keep the whole ladder, so every stage's
/// grader still runs once per proof run in the cumulative order.
#[test]
fn r63_f2_stage20_proofs_keep_the_full_ladder() {
    let src = include_str!("tests.rs");
    for name in [
        "test_pipeline_eval_only_sql_stage20_pgwire_live",
        "test_pipeline_eval_only_sql_stage20_pgwire_lite_live",
    ] {
        let start = src
            .find(&format!("fn {name}()"))
            .expect("the stage-20 proof is in this file");
        let body = &src[start..];
        let end = body.find("\n}\n").unwrap_or(body.len());
        assert!(
            body[..end].contains("accepted_stages: (0..=20).collect()"),
            "{name} must keep the full ladder (0..=20)"
        );
    }
}
