use super::super::compare::{compare, render_expected};
use super::super::driver::{run_stage, stage_seed};
use super::super::generator::{build_script, Profile};
use super::super::oracle::Oracle;
use super::super::scenarios::pipeline;
use super::*;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the reference on drop, panics included.
struct RefProc {
    child: Child,
    script: std::path::PathBuf,
    port: u16,
    data: std::path::PathBuf,
}

impl RefProc {
    fn spawn(&mut self) {
        self.child = Command::new("python3")
            .arg(&self.script)
            .args(["--port", &self.port.to_string(), "--data"])
            .arg(&self.data)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("python3 must be installed for the reference adapter");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("reference did not open port {}", self.port);
    }

    /// SIGKILL the reference (ticket 5's crash probe): no respawn.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Stop and start on the same store (tickets 4 and 5).
    fn restart(&mut self) -> Result<(), String> {
        self.kill();
        self.spawn();
        Ok(())
    }
}

impl Drop for RefProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The fixture's script and data dirs go with it (the suite used
        // to leave /tmp/sdb-sql-* dirs behind, including on panics).
        if let Some(dir) = self.script.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_reference(port: u16) -> RefProc {
    let dir = std::env::temp_dir().join(format!("sdb-sql-ref-{}-{port}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_reference(&dir, port).unwrap();
    let data = std::env::temp_dir().join(format!("sdb-sql-data-{}-{port}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let mut guard = RefProc {
        // Placeholder replaced by spawn(): the struct owns the child.
        child: Command::new("true").spawn().unwrap(),
        script: dir.join("sql_server.py"),
        port,
        data,
    };
    // From here the guard owns the child, so the wait-for-port panic
    // path still kills and reaps it.
    guard.spawn();
    guard
}

/// The suite's 20/20 bar does not cover the ladder and the transaction
/// checks (the pipeline runs those itself). Run them 20 times against
/// one host process too, with a smaller bulk table: the point is
/// flakiness, not size — the Docker live proofs use the real 50k.
#[test]
fn reference_passes_the_crash_and_txn_checks_twenty_times() {
    use super::super::crash;
    use super::super::driver::wait_until_answering;
    use super::super::txn;
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let t = Duration::from_secs(10);
    for rep in 0..20u32 {
        let out = {
            let mut kill = || {
                proc.kill();
                Ok(())
            };
            crash::ack_kill(&addr, t, &format!("r{rep}_a"), 20, &mut kill).expect("ack_kill")
        };
        let acked = out.acked;
        proc.restart().unwrap();
        wait_until_answering(&addr, t).expect("answers after a SIGKILL");
        if out.completed {
            crash::check_acked_rows(&addr, t, &format!("r{rep}_a"), acked + 1)
                .expect("answered in-flight row survived");
        } else {
            crash::check_acked_prefix(&addr, t, &format!("r{rep}_a"), acked, 1)
                .expect("acked prefix");
        }

        let completed = {
            let mut kill = || {
                proc.kill();
                Ok(())
            };
            crash::bulk_update_and_kill(&addr, t, &format!("r{rep}_b"), 2_000, 500, &mut kill)
                .expect("bulk update")
        };
        proc.restart().unwrap();
        wait_until_answering(&addr, t).expect("answers after the killed update");
        crash::check_uniform(&addr, t, &format!("r{rep}_b"), 2_000, completed).expect("uniform");

        let before = crash::snapshot_files(&proc.data).expect("stamps");
        crash::append_row(&addr, t, &format!("r{rep}_a"), acked + 1).expect("victim");
        proc.kill();
        let victim = crash::changed_file(&proc.data, &before)
            .expect("changed")
            .expect("the victim commit wrote something");
        let cut = crash::torn_tail(&victim, 256).expect("torn tail");
        assert!(cut > 0, "rep {rep}: nothing to tear");
        proc.restart().unwrap();
        wait_until_answering(&addr, t).expect("answers after a torn tail");
        crash::check_acked_prefix(&addr, t, &format!("r{rep}_a"), acked, 2)
            .expect("after a torn tail");

        txn::isolation(&addr, t, &format!("r{rep}_iso")).expect("isolation");
        {
            // Two callbacks share the one process.
            let proc = std::cell::RefCell::new(&mut proc);
            let mut kill = || {
                proc.borrow_mut().kill();
                Ok(())
            };
            let mut restart = || {
                proc.borrow_mut().restart()?;
                wait_until_answering(&addr, t)
            };
            txn::kill_with_open_transaction(
                &addr,
                t,
                &format!("r{rep}_txn"),
                &mut kill,
                &mut restart,
            )
            .expect("kill with a transaction open");
        }
    }
}

/// Stage 8's lite variant (`join-inner-only`): the cumulative suite
/// keeps comma/inner joins and never emits `LEFT JOIN`, and the
/// reference passes it.
#[test]
fn reference_passes_the_stage8_lite_suite() {
    use super::super::driver::{run_stage_with_variants, wait_until_answering};
    use super::super::generator::{build_script, Profile};
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let t = Duration::from_secs(10);
    wait_until_answering(&addr, t).expect("answers");
    let passed: Vec<u32> = (0..=8).collect();
    let variants = vec![(8u32, vec!["join-inner-only".to_string()])];
    for run in 0..3u64 {
        let seed = stage_seed(&format!("stage8-lite-{run}"), 8);
        let mut restart = || proc.restart();
        let out = run_stage_with_variants(
            &addr,
            t,
            seed,
            &passed,
            &variants,
            120,
            None,
            Some(&mut restart),
        )
        .unwrap_or_else(|e| panic!("run {run}: verifier fault: {e}"));
        assert!(out.passed, "stage-8 lite run {run}: {}", out.detail);
    }
    // The narrowed profile really drops LEFT JOIN (and keeps joins).
    let mut p = Profile::for_passed(&passed);
    p.narrow(&variants[0].1);
    let (mut joins, mut left) = (0, 0);
    for seed in 0..40u64 {
        let s = build_script(seed, &p, 120);
        for c in &s.steps {
            let q = c.stmt.sql.to_lowercase();
            if c.stmt.kind == super::super::generator::Kind::Select
                && (q.contains(" join ") || q.contains(", "))
            {
                joins += 1;
                left += usize::from(q.contains("left join"));
            }
        }
    }
    assert!(joins > 0, "the lite profile emitted no joins at all");
    assert_eq!(left, 0, "the lite profile emitted LEFT JOIN");
}

/// The dialect's statement atomicity: a failed multi-row INSERT
/// changes nothing, in autocommit and inside a transaction alike. The
/// host's SQLite keeps earlier rows, so the reference wraps DML in a
/// statement savepoint — this pins both cases.
#[test]
fn reference_statement_atomicity_always_holds() {
    use super::super::client::TcpCandidate;
    use super::super::run::Candidate;
    use super::super::wire::Response;
    let port = free_port();
    let proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let mut c = TcpCandidate::connect(&addr, Duration::from_secs(10)).unwrap();
    c.exec("CREATE TABLE quirk (a INTEGER, b INTEGER)").unwrap();
    match c
        .exec("INSERT INTO quirk VALUES (1, 1), ('zz', 2)")
        .unwrap()
    {
        Response::Err { .. } => {}
        other => panic!("expected a type error, got {other:?}"),
    }
    match c.exec("SELECT * FROM quirk").unwrap() {
        Response::Rows { rows, .. } => {
            assert!(rows.is_empty(), "autocommit kept partial rows: {rows:?}")
        }
        other => panic!("{other:?}"),
    }
    // Inside a transaction the same statement changes nothing either.
    c.exec("BEGIN").unwrap();
    assert!(matches!(
        c.exec("INSERT INTO quirk VALUES (2, 2), ('zz', 3)")
            .unwrap(),
        Response::Err { .. }
    ));
    match c.exec("SELECT * FROM quirk").unwrap() {
        Response::Rows { rows, .. } => {
            assert!(rows.is_empty(), "inside a transaction: {rows:?}")
        }
        other => panic!("{other:?}"),
    }
    // `INSERT ... SELECT` with a bad cell is atomic too.
    c.exec("CREATE TABLE quirk_src (a INTEGER, b TEXT)")
        .unwrap();
    c.exec("INSERT INTO quirk_src VALUES (1, '1'), (2, 'zz')")
        .unwrap();
    assert!(matches!(
        c.exec("INSERT INTO quirk SELECT a, b FROM quirk_src")
            .unwrap(),
        Response::Err { .. }
    ));
    match c.exec("SELECT * FROM quirk").unwrap() {
        Response::Rows { rows, .. } => {
            assert!(rows.is_empty(), "insert-select left rows: {rows:?}")
        }
        other => panic!("{other:?}"),
    }
    c.exec("ROLLBACK").unwrap();
    drop(proc);
}

/// The flake bar (docs/PROPOSAL-sdb-sql.md): each stage passes 20/20
/// against the reference, through stage 11 (aggregate, join,
/// constraint, subquery and index scripts). All
/// runs share one server process, so this also proves stage scripts
/// never collide on schema names. Stage 11's *perf* half is timing,
/// not flakes, and lives in `reference_passes_the_stage11_perf_gate`
/// (a loaded host must not fail the flake bar).
#[test]
fn reference_passes_every_stage_twenty_times() {
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    for stage in 0..=13u32 {
        let passed: Vec<u32> = (0..=stage).collect();
        for run in 0..20u32 {
            let seed = stage_seed(&format!("reference-bar-{run}"), stage);
            // Stage 4's suite restarts the candidate: the reference
            // comes back on the same store.
            let mut restart = || proc.restart();
            let out = run_stage(
                &addr,
                Duration::from_secs(10),
                seed,
                &passed,
                120,
                None,
                Some(&mut restart),
            )
            .unwrap_or_else(|e| panic!("stage {stage} run {run}: verifier fault: {e}"));
            assert!(
                out.passed,
                "stage {stage} run {run} (seed {seed}): {}",
                out.detail
            );
        }
    }
}

/// Stage 11's perf gate against a real process: two 200k-row tables,
/// 500 alternating point lookups, a range round, and a restart between
/// the two point rounds. The host is shared, so the gate's own bounds
/// (5 ms p50, 10x) are the assertions; a healthy SQLite clears them by
/// an order of magnitude.
#[test]
fn reference_passes_the_stage11_perf_gate() {
    use super::super::perf;
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let full = {
        let mut restart = || proc.restart();
        perf::perf_gate(
            &addr,
            Duration::from_secs(30),
            0x5E11,
            false,
            Some(&mut restart),
        )
        .expect("the full perf gate")
    };
    assert!(full.contains("point p50"), "full report: {full}");
    assert!(full.contains("range p50"), "full report: {full}");
    assert!(full.contains("after restart"), "full report: {full}");
    // Lite: equality only, no restart.
    let lite =
        perf::perf_gate(&addr, Duration::from_secs(30), 0x5E12, true, None).expect("lite gate");
    assert!(!lite.contains("range"), "lite report: {lite}");
    drop(proc);
}

/// Stage 5 against a real process: SIGKILL after acks, a mid-flight
/// bulk UPDATE, a torn newest data file, and a double restart.
#[test]
fn reference_survives_the_crash_safety_ladder() {
    use super::super::crash;
    use super::super::driver::wait_until_answering;
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let t = Duration::from_secs(10);

    // (a) Acked rows survive a SIGKILL; only the one statement that was
    // still in flight when the kill landed may be missing.
    let out = {
        let mut kill = || {
            proc.kill();
            Ok(())
        };
        crash::ack_kill(&addr, t, "crash_acked", 40, &mut kill).expect("ack_kill")
    };
    let acked = out.acked;
    assert_eq!(acked, 40);
    proc.restart().unwrap();
    wait_until_answering(&addr, t).expect("answers after the SIGKILL");
    if out.completed {
        crash::check_acked_rows(&addr, t, "crash_acked", acked + 1)
            .expect("the answered in-flight row survived");
    } else {
        crash::check_acked_prefix(&addr, t, "crash_acked", acked, 1).expect("acked rows survived");
    }

    // (b) A bulk UPDATE killed mid-flight is all or nothing.
    let completed = {
        let mut kill = || {
            proc.kill();
            Ok(())
        };
        crash::bulk_update_and_kill(&addr, t, "crash_bulk", 2_000, 500, &mut kill)
            .expect("bulk update")
    };
    proc.restart().unwrap();
    wait_until_answering(&addr, t).expect("answers after the killed update");
    crash::check_uniform(&addr, t, "crash_bulk", 2_000, completed).expect("all-or-nothing");

    // (c) A torn tail (a partial trailing record): kill first, cut the
    // file the victim commit touched, and require every older row.
    let before = crash::snapshot_files(&proc.data).expect("store stamps");
    crash::append_row(&addr, t, "crash_acked", acked + 1).expect("victim row");
    proc.kill();
    let victim = crash::changed_file(&proc.data, &before)
        .expect("changed file")
        .expect("the victim commit wrote something");
    let cut = crash::torn_tail(&victim, 256).expect("torn tail");
    assert!(cut > 0, "the store must have a newest file to tear");
    proc.restart().unwrap();
    wait_until_answering(&addr, t).expect("answers after a torn tail");
    crash::check_acked_prefix(&addr, t, "crash_acked", acked, 2).expect("data after a torn tail");
    crash::check_uniform(&addr, t, "crash_bulk", 2_000, false).expect("bulk after a torn tail");

    // (d) A second restart changes nothing.
    proc.restart().unwrap();
    wait_until_answering(&addr, t).expect("answers after two restarts");
    crash::check_acked_prefix(&addr, t, "crash_acked", acked, 2).expect("double restart");
    crash::check_uniform(&addr, t, "crash_bulk", 2_000, false).expect("double restart");
}

/// Stage 6 against a real process: connections see only committed
/// rows, and a kill with a transaction open discards it.
#[test]
fn reference_transactions_isolate_and_roll_back_on_a_kill() {
    use super::super::driver::wait_until_answering;
    use super::super::txn;
    let port = free_port();
    let proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let t = Duration::from_secs(10);
    txn::isolation(&addr, t, "txn_iso").expect("isolation");
    {
        // Two callbacks share one process: a cell keeps the borrows
        // disjoint.
        let proc = std::cell::RefCell::new(proc);
        let mut kill = || {
            proc.borrow_mut().kill();
            Ok(())
        };
        let mut restart = || {
            proc.borrow_mut().restart().map_err(|e| e.to_string())?;
            wait_until_answering(&addr, t).map_err(|e| e.to_string())
        };
        txn::kill_with_open_transaction(&addr, t, "txn_kill", &mut kill, &mut restart)
            .expect("open transaction discarded by recovery");
    }
}

/// A restart that loses the store must be caught (the reference's
/// `--data` is the harness-owned directory).
#[test]
fn reference_data_lost_across_a_restart_is_a_failure() {
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let mut keep = || proc.restart();
    assert_eq!(
        super::super::scenarios::persistence(
            &addr,
            Duration::from_secs(10),
            stage_seed("reference-persistence", 4),
            &Profile::through(4),
            &mut keep,
        ),
        super::super::scenarios::ScenarioResult::Pass
    );
    // Point the reference at a fresh store and restart: the reads must
    // drift.
    proc.data =
        std::env::temp_dir().join(format!("sdb-sql-data-lost-{}-{port}", std::process::id()));
    let _ = std::fs::remove_dir_all(&proc.data);
    std::fs::create_dir_all(&proc.data).unwrap();
    let mut fresh = || proc.restart();
    match super::super::scenarios::persistence(
        &addr,
        Duration::from_secs(10),
        stage_seed("reference-persistence", 4) ^ 1,
        &Profile::through(4),
        &mut fresh,
    ) {
        super::super::scenarios::ScenarioResult::Fail(d) => {
            assert!(d.contains("after a clean restart"), "{d}")
        }
        other => panic!("{other:?}"),
    }
}

/// R37 F5/R36-1: the persistence path records the sentinel seed it
/// actually used, and that seed stays out of the candidate-visible
/// failure detail. The run seed maps onto a known bad persistence seed.
#[test]
fn r37_f5_persistence_seed_is_evidence_not_feedback() {
    let bad = 9231256963851754504u64;
    let run_seed = bad ^ 0x5E27_A510_0000_0004;
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let mut used = None;
    let mut keep = || proc.restart();
    assert_eq!(
        super::super::scenarios::persistence_with_seed(
            &addr,
            Duration::from_secs(10),
            run_seed,
            &Profile::through(4),
            &mut keep,
            &mut used,
        ),
        super::super::scenarios::ScenarioResult::Pass
    );
    let used = used.expect("the used seed is recorded");
    assert!(used > bad, "the bad seed moved on: {used} > {bad}");

    // The same run seed against an empty store fails; its detail is
    // candidate-visible and must not contain the harness seed.
    proc.data =
        std::env::temp_dir().join(format!("sdb-sql-seed-lost-{}-{port}", std::process::id()));
    let _ = std::fs::remove_dir_all(&proc.data);
    std::fs::create_dir_all(&proc.data).unwrap();
    let mut used_again = None;
    let mut fresh = || proc.restart();
    match super::super::scenarios::persistence_with_seed(
        &addr,
        Duration::from_secs(10),
        run_seed,
        &Profile::through(4),
        &mut fresh,
        &mut used_again,
    ) {
        super::super::scenarios::ScenarioResult::Fail(d) => {
            assert!(!d.contains(&used.to_string()), "seed leaked: {d}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(used_again, Some(used));
}

/// R37 F5: the driver carries the persistence seed into its outcome.
#[test]
fn r37_f5_driver_records_the_persistence_seed() {
    let bad = 9231256963851754504u64;
    let run_seed = bad ^ 0x5E27_A510_0000_0004;
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let mut restart = || proc.restart();
    let out = run_stage(
        &addr,
        Duration::from_secs(10),
        run_seed,
        &[0, 1, 2, 3, 4],
        120,
        None,
        Some(&mut restart),
    )
    .expect("stage run");
    assert!(out.passed, "{}", out.detail);
    let used = out
        .sentinel_seed
        .expect("the stage outcome records the seed");
    assert!(used > bad, "the bad seed moved on: {used} > {bad}");
    assert!(!out.detail.contains(&used.to_string()), "{}", out.detail);
}

/// Review round 14, G1, as it happened live. A ladder that failed
/// tickets 2 and 4 is graded on `[0, 1, 3, 9]`: no restart, no
/// persistence dataset (gating on the highest stage used to run it). One
/// that failed tickets 1-3 is graded on `[0, 4]`: the restart runs, on
/// a dataset of ticket-0 grammar compared as a multiset.
#[test]
fn reference_suites_run_persistence_only_with_ticket_4() {
    let port = free_port();
    let mut proc = start_reference(port);
    let addr = format!("127.0.0.1:{port}");
    let restarts = std::cell::Cell::new(0u32);
    for (passed, restarted) in [
        (vec![0u32, 1, 3, 9], false),
        (vec![0, 4], true),
        (vec![0, 1, 2, 3, 4], true),
    ] {
        for run in 0..3u32 {
            restarts.set(0);
            let seed = stage_seed(&format!("g1-{passed:?}-{run}"), passed[passed.len() - 1]);
            let mut restart = || {
                restarts.set(restarts.get() + 1);
                proc.restart()
            };
            let out = run_stage(
                &addr,
                Duration::from_secs(10),
                seed,
                &passed,
                120,
                None,
                Some(&mut restart),
            )
            .unwrap_or_else(|e| panic!("{passed:?} run {run}: verifier fault: {e}"));
            assert!(out.passed, "{passed:?} run {run}: {}", out.detail);
            assert_eq!(restarts.get() > 0, restarted, "{passed:?} run {run}");
        }
    }
}

/// Pipelining a generated script against the reference: every line is
/// sent before the first response is read, and every answer grades
/// clean against the oracle.
#[test]
fn reference_answers_a_pipelined_script() {
    let port = free_port();
    let _ref = start_reference(port);
    let script = build_script(
        stage_seed("reference-pipeline", 3),
        &Profile::through(3),
        60,
    );
    let checked: Vec<_> = script.setup.iter().chain(&script.steps).collect();
    let lines: Vec<String> = checked.iter().map(|c| c.stmt.sql.clone()).collect();
    let got = pipeline(("127.0.0.1", port), &lines, Duration::from_secs(10))
        .expect("the reference must answer every pipelined line");
    assert_eq!(got.len(), lines.len());
    let oracle = Oracle::new().unwrap();
    for ((reply, line), c) in got.iter().zip(&lines).zip(&checked) {
        let expected = oracle.exec(line);
        compare(&expected, reply, c.stmt.ordered).unwrap_or_else(|m| {
            panic!("{line:?}: {m:?} (expected {})", render_expected(&expected))
        });
    }
}
