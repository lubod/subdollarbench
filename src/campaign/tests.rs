use super::*;
use crate::verifier::StageResult;

/// F2: a provider billing/auth fault is a real harness fault: it keeps
/// the fault label and stops without a lite.
#[tokio::test]
async fn f2_a_provider_billing_fault_stays_fault_and_stops() {
    let lc = fixture_campaign_with_lite();
    let opts = CampaignRunOptions {
        campaign_id: "f2billing".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let c = calls.clone();
    let rollup = run_campaign(
        &lc,
        &opts,
        move |attempt| {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = if attempt.stage.id == 0 {
                outcome(true, &[(1, true), (2, true)])
            } else {
                let mut o = outcome(false, &[]);
                o.agent_failed = true;
                o.harness_fault = Some("provider billing/auth: exhausted".into());
                o
            };
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(rollup.stages[1].status, "fault");
    assert!(
        !rollup
            .stages
            .iter()
            .any(|r| r.id == 1 && r.variant == "lite"),
        "a billing fault must not get a lite"
    );
    assert!(
        rollup
            .harness_fault
            .as_deref()
            .unwrap_or("")
            .contains("provider billing"),
        "{:?}",
        rollup.harness_fault
    );
}

/// R65 F2 (R63-3): the real outage line classifies, and the phase it trips
/// is a harness fault: the cell stops and no ticket is a model failure.
#[tokio::test]
async fn r65_f2_billing_trip_is_harness_fault() {
    let outage = "402 This request requires more credits, or fewer max_tokens. You requested up to 65536 tokens, but can only afford 44804. To increase, visit https://openrouter.ai/workspaces/default/keys/REDACTED and adjust the key's total limit";
    let reason = crate::sandbox::omp::classify_billing_error(outage)
        .expect("R65 F1 must classify the outage line");
    let fault = format!("provider billing/auth: {reason}");
    let lc = fixture_campaign_with_lite();
    let opts = CampaignRunOptions {
        campaign_id: "r65f2".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        move |attempt| {
            let out = if attempt.stage.id == 0 {
                outcome(true, &[(1, true), (2, true)])
            } else {
                let mut o = outcome(false, &[]);
                o.agent_failed = true;
                o.harness_fault = Some(fault.clone());
                o
            };
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(stop_reason(&rollup), "harness fault");
    assert!(
        rollup
            .harness_fault
            .as_deref()
            .unwrap_or("")
            .contains("provider billing exhausted (HTTP 402)"),
        "{:?}",
        rollup.harness_fault
    );
    assert_eq!(rollup.stages[1].status, "fault");
    assert!(
        !rollup.stages.iter().any(|r| r.status == "failed"),
        "a provider fault is not a model failure: {:?}",
        rollup.stages
    );
    assert!(
        !rollup
            .stages
            .iter()
            .any(|r| r.id == 1 && r.variant == "lite"),
        "a billing fault must not get a lite"
    );
}

/// R66 F1: the paid launch's price preflight accepts the next grid's four
/// ids and still refuses an unknown one.
#[tokio::test]
async fn r66_f1_price_preflight_accepts_the_new_grid() {
    for model in [
        "tencent/hy4",
        "minimax/minimax-m3.1-flash",
        "openai/gpt-6-luna-pro",
        "xiaomi/mimo-v2.6-pro",
    ] {
        require_model_list_price(model, false)
            .await
            .unwrap_or_else(|e| panic!("{model}: {e}"));
    }
    assert!(
        require_model_list_price("not/a-priced-model", false)
            .await
            .is_err(),
        "an unknown model must still be refused"
    );
}

/// R67 F1: a priced row without a pinned provider is refused, and a pinned
/// grid model passes.
#[tokio::test]
async fn r67_f1_unpinned_refused() {
    let err = require_model_list_price("google/gemini-2.5-flash", false)
        .await
        .unwrap_err();
    assert!(err.contains("no pinned provider"), "{err}");
    require_model_list_price("openai/gpt-6-luna", false)
        .await
        .expect("a pinned grid model must pass");
}

/// R67 F2: the phase rollup sums its tickets' billed totals.
#[tokio::test]
async fn r67_f2_rollup_billed() {
    let lc = fixture_campaign_with_lite();
    let opts = CampaignRunOptions {
        campaign_id: "r67billed".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        move |attempt| {
            let mut o = outcome(true, &[(1, true), (2, true)]);
            o.billed_usd = if attempt.stage.id == 0 { 0.2 } else { 0.3 };
            async move { Ok(o) }
        },
        None,
    )
    .await
    .unwrap();
    let want: f64 = rollup.stages.iter().map(|r| r.billed_usd).sum();
    assert!(
        (rollup.billed_usd - want).abs() < 1e-9,
        "billed {} vs tickets {}",
        rollup.billed_usd,
        want
    );
    assert!(want > 0.0);
}

/// gui-spec.md §5.2: the classifier's six kinds, one fixture each.
#[test]
fn failure_classifier_kinds() {
    let result = |id: u32, passed: bool, error: Option<&str>| StageResult {
        stage: id,
        name: format!("stage {id}"),
        passed,
        error: error.map(str::to_string),
    };
    // fault: a harness/provider fault is never a model verdict.
    let fault = classify_failure(
        5,
        &[0],
        &[result(0, false, Some("ignored"))],
        false,
        true,
        None,
    )
    .expect("classified");
    assert_eq!(fault.kind, "fault");
    // no_start: stage 0 unreachable, with the stderr cause.
    let tail = "File \"/app/./server.py\", line 101\n    elif c == \"*\":\nIndentationError: expected an indented block";
    let no_start = classify_failure(
        8,
        &[0, 1, 3, 4, 5],
        &[result(
            0,
            false,
            Some("server unreachable (candidate at 127.0.0.1:5480 never answered a probe)"),
        )],
        false,
        false,
        Some(tail),
    )
    .expect("classified");
    assert_eq!(no_start.kind, "no_start");
    assert!(no_start.line.contains("server.py:101"), "{}", no_start.line);
    assert!(
        no_start.line.contains("IndentationError"),
        "{}",
        no_start.line
    );
    assert!(no_start.line.chars().count() <= 160);
    // regression: an earlier green ticket fails now.
    let reg = classify_failure(
        8,
        &[0, 1, 3, 4, 5],
        &[
            result(0, true, None),
            result(3, false, Some("Kind: expected OK 2, got OK 0")),
        ],
        false,
        false,
        None,
    )
    .expect("classified");
    assert_eq!(reg.kind, "regression");
    assert!(reg.line.starts_with("broke ticket 3"), "{}", reg.line);
    // Round-7 finding 3: stages the verifier skipped only follow the
    // real break; they are counted, not blamed.
    let skipped = classify_failure(
        6,
        &[0, 1, 3, 4, 5],
        &[
            result(0, false, Some("Kind: expected OK 0, got ERR syntax")),
            result(1, false, Some("skipped due to failure in earlier stages")),
            result(3, false, Some("skipped due to failure in earlier stages")),
            result(4, false, Some("skipped due to failure in earlier stages")),
            result(5, false, Some("skipped due to failure in earlier stages")),
        ],
        false,
        false,
        None,
    )
    .expect("classified");
    assert_eq!(skipped.kind, "regression");
    assert_eq!(
        skipped.line,
        "broke ticket 0 (1,3,4,5 not run): Kind: expected OK 0, got ERR syntax"
    );
    // late_check: the pipeline's own tag wins over own_stage.
    let late = classify_failure(
        13,
        &[0],
        &[
            result(0, true, None),
            result(
                13,
                false,
                Some("[perf] point lookups: indexed p50 7.2 ms is over the 5 ms budget"),
            ),
        ],
        false,
        false,
        None,
    )
    .expect("classified");
    assert_eq!(late.kind, "late_check");
    assert!(late.line.starts_with("[perf]"), "{}", late.line);
    // own_stage: only this ticket's stage failed.
    let own = classify_failure(
        8,
        &[0, 1, 3],
        &[
            result(0, true, None),
            result(
                8,
                false,
                Some("ErrorClass: expected ERR no_column, got ERR syntax"),
            ),
        ],
        false,
        false,
        None,
    )
    .expect("classified");
    assert_eq!(own.kind, "own_stage");
    assert!(own.line.starts_with("own stage:"), "{}", own.line);
    // Round-18 N6: late checks the pipeline did not run follow the real
    // break too. Ticket 8's own failure is not a regression of 5 and 6.
    let not_run = "not run: late checks skipped after stage 8 failed";
    let own_not_run = classify_failure(
        8,
        &[0, 1, 2, 3, 4, 5, 6, 7],
        &[
            result(0, true, None),
            result(5, false, Some(not_run)),
            result(6, false, Some(not_run)),
            result(7, true, None),
            result(
                8,
                false,
                Some("Kind: expected ROWS (10 rows), got ERR syntax"),
            ),
        ],
        false,
        false,
        None,
    )
    .expect("classified");
    assert_eq!(own_not_run.kind, "own_stage");
    assert_eq!(own_not_run.stage, Some(8));
    assert_eq!(
        own_not_run.line,
        "own stage: Kind: expected ROWS (10 rows), got ERR syntax"
    );
    assert!(
        own_not_run.detail.starts_with("Kind:"),
        "{}",
        own_not_run.detail
    );
    // A real regression is still named, with the late checks listed as
    // not run.
    let reg_not_run = classify_failure(
        11,
        &[0, 5, 6, 7, 10],
        &[
            result(
                5,
                false,
                Some("not run: late checks skipped after stage 6 failed"),
            ),
            result(6, false, Some("Kind: expected OK 2, got ERR syntax")),
            result(7, false, Some("skipped due to failure in earlier stages")),
            result(10, false, Some("skipped due to failure in earlier stages")),
            result(11, false, Some("skipped due to failure in earlier stages")),
        ],
        false,
        false,
        None,
    )
    .expect("classified");
    assert_eq!(reg_not_run.kind, "regression");
    assert_eq!(reg_not_run.stage, Some(6));
    assert_eq!(
        reg_not_run.line,
        "broke ticket 6 (5,7,10 not run): Kind: expected OK 2, got ERR syntax"
    );
    // agent: the agent died or never signalled READY.
    let agent = classify_failure(8, &[], &[], true, false, None).expect("classified");
    assert_eq!(agent.kind, "agent");
    // A passing round has no cause.
    assert!(classify_failure(8, &[0], &[result(0, true, None)], false, false, None).is_none());
}

/// gui-spec.md §6.1 backfill: a pre-rounds cell's per-round scores come
/// from its console log, and its final failure from the manifest.
#[test]
fn backfill_reads_rounds_from_the_console_and_classifies_the_failure() {
    let dir = std::env::temp_dir().join(format!("backfill_{}", std::process::id()));
    let stage = dir.join("s8_full");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
            stage.join("console.log"),
            "[FEEDBACK] Initial attempt passed 5 of 6 stages (83.3%). Starting interactive repair loop (max 1 rounds)...\n[FEEDBACK] Round 1 test result: 2 of 6 stages passed (33.3%)\n",
        )
        .unwrap();
    std::fs::write(
            stage.join("manifest.json"),
            serde_json::json!({
                "status": "failed_tests",
                "passed_stages": 2,
                "total_stages": 6,
                "duration_seconds": 900.0,
                "stages": [
                    {"stage": 3, "name": "s3", "passed": false, "error": "Kind: expected OK 2, got OK 0"},
                    {"stage": 8, "name": "s8", "passed": false, "error": "ErrorClass: expected ERR no_column, got ERR syntax"}
                ]
            })
            .to_string(),
        )
        .unwrap();
    let mut rollup = CampaignRollup {
        campaign_id: "c".into(),
        name: "sdb-sql".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        task: "sql".into(),
        started_at: String::new(),
        finished_at: String::new(),
        stages: vec![],
        reached: None,
        regression_free_tickets: 0,
        total_cost_usd: 0.0,
        billed_usd: 0.0,
        health: None,
        over_budget: false,
        cap_reached: false,
        over_wall: false,
        rules_version: String::new(),
        sqlite_version: None,
        cap_price_basis: None,
        sandbox_image: None,
        budget_usd: 0.0,
        wall_min: 0,
        worker: 0,
        concurrent_cells: 1,
        grading_wait_s: 0.0,
        points: 0.0,
        max_points: 0.0,
        harness_fault: None,
        rehearsal: false,
        smoke: false,
        phase: None,
        chain_from: None,
        career: None,
        perf: None,
        ladder_state: None,
        refork: None,
    };
    // Ticket 3 passed before this attempt: stage 3 failing again is a
    // regression, not an own-stage miss.
    rollup.stages.push(StageRecord {
        id: 3,
        title: "Update/Delete".into(),
        kind: "feature".into(),
        passed: true,
        pass_rate: 100.0,
        passed_stages: 3,
        total_stages: 3,
        regressed: vec![],
        regression_severity: 0,
        api_cost_usd: 0.02,
        billed_usd: 0.0,
        effective_cost_usd: 0.02,
        turns: Some(12),
        duration_seconds: Some(200.0),
        status: "passed".into(),
        stop_reason: None,
        variant: "full".into(),
        tier: 1,
        points: 1.0,
        run_id: "r3".into(),
        git_commit: None,
        git_stat: None,
        rounds: vec![],
        failure: None,
        grading_wait_s: 0.0,
    });
    rollup.stages.push(StageRecord {
        id: 8,
        title: "Joins".into(),
        kind: "feature".into(),
        passed: false,
        pass_rate: 33.3,
        passed_stages: 2,
        total_stages: 6,
        regressed: vec![3, 4, 5],
        regression_severity: 3,
        api_cost_usd: 0.1,
        billed_usd: 0.0,
        effective_cost_usd: 0.1,
        turns: Some(30),
        duration_seconds: Some(900.0),
        status: "failed".into(),
        stop_reason: None,
        variant: "full".into(),
        tier: 3,
        points: 0.0,
        run_id: "r8".into(),
        git_commit: None,
        git_stat: None,
        rounds: vec![],
        failure: None,
        grading_wait_s: 0.0,
    });
    backfill_rounds(&mut rollup, &dir);
    let rounds = &rollup.stages[1].rounds;
    assert_eq!(rounds.len(), 2, "both rounds recovered: {rounds:?}");
    assert_eq!(
        (
            rounds[0].round,
            rounds[0].passed_stages,
            rounds[0].total_stages
        ),
        (0, 5, 6)
    );
    assert_eq!((rounds[1].round, rounds[1].passed_stages), (1, 2));
    assert!(rounds.iter().all(|r| r.derived));
    // Only the final round carries the classified failure (stage 3 is
    // not green-before, stage 8 owns the ticket).
    assert!(rounds[0].failure.is_none());
    let f = rounds[1].failure.as_ref().expect("classified");
    assert_eq!(f.kind, "regression");
    assert!(f.line.starts_with("broke ticket 3"), "{}", f.line);
    let _ = std::fs::remove_dir_all(&dir);
}

/// R26 R23-7 acceptance: every session's stop reason reaches the stage
/// record, not only the attempt's final one.
#[test]
fn r26_backfill_keeps_each_sessions_stop_reason() {
    let dir = tmp_dir("r26stops");
    let stage = dir.join("s8_full");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join("manifest.json"),
        serde_json::json!({
            "status": "failed_tests",
            "rounds": [
                {"round": 0, "passed_stages": 5, "total_stages": 6,
                 "pass_rate": 83.3, "turns": 50, "stop_reason": "turns"},
                {"round": 1, "passed_stages": 6, "total_stages": 6,
                 "pass_rate": 100.0, "turns": 20, "stop_reason": "ready"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let mut rollup = chain_rollup(1, true);
    rollup.stages.push(failed_record(8, "Joins", "not yet", ""));
    backfill_rounds(&mut rollup, &dir);
    let rounds = &rollup.stages[0].rounds;
    assert_eq!(rounds.len(), 2, "{rounds:?}");
    assert_eq!(rounds[0].stop_reason.as_deref(), Some("turns"));
    assert_eq!(rounds[1].stop_reason.as_deref(), Some("ready"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// F2: old rows wrote `fault` for a graded agent crash. The backfill
/// corrects that to `failed` unless the cell has a real harness fault.
#[test]
fn f2_backfill_reads_an_old_graded_crash_as_failed() {
    let dir = tmp_dir("f2backfill");
    let stage = dir.join("s8_full");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join("manifest.json"),
        serde_json::json!({"status": "agent_failed"}).to_string(),
    )
    .unwrap();
    let mut rec = failed_record(8, "Joins", "agent stopped", "");
    rec.status = "fault".into();
    let mut rollup = chain_rollup(1, true);
    rollup.stages = vec![rec];
    rollup.harness_fault = None;
    backfill_rounds(&mut rollup, &dir);
    assert_eq!(rollup.stages[0].status, "failed");
    // A real harness fault (provider billing) keeps the fault label.
    let mut rec = failed_record(8, "Joins", "provider billing", "");
    rec.status = "fault".into();
    let mut rollup = chain_rollup(1, true);
    rollup.stages = vec![rec];
    rollup.harness_fault = Some("provider billing/auth: exhausted".into());
    backfill_rounds(&mut rollup, &dir);
    assert_eq!(rollup.stages[0].status, "fault");
    let _ = std::fs::remove_dir_all(&dir);
}

/// R22-5 acceptance: the relabel is independent of the rounds backfill,
/// so a stage that already has recorded rounds still reads `failed`.
#[test]
fn r22_5_a_stage_with_recorded_rounds_still_relabels() {
    let dir = tmp_dir("r22_5rounds");
    let stage = dir.join("s8_full");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join("manifest.json"),
        serde_json::json!({"status": "agent_failed"}).to_string(),
    )
    .unwrap();
    let mut rec = failed_record(8, "Joins", "agent stopped", "");
    rec.status = "fault".into();
    rec.rounds = vec![RoundRecord {
        round: 0,
        passed_stages: 1,
        total_stages: 1,
        turns: Some(12),
        cost_usd: 0.01,
        duration_seconds: Some(60.0),
        git_commit: None,
        failure: None,
        stop_reason: Some("crash".into()),
        derived: false,
    }];
    let mut rollup = chain_rollup(1, true);
    rollup.stages = vec![rec];
    rollup.harness_fault = None;
    backfill_rounds(&mut rollup, &dir);
    assert_eq!(rollup.stages[0].status, "failed");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Round-7 finding 1: the lock refuses a live holder, replaces a stale
/// one, and is released on drop.
#[test]
fn campaign_lock_refuses_live_holders_and_replaces_stale_ones() {
    let dir = std::env::temp_dir().join(format!("campaign_lock_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".campaign.lock");
    let lock = CampaignLock::try_acquire_at(&path, "c1").expect("first acquire");
    assert_eq!(
        live_lock_holder_at(&path).map(|h| h.campaign_id),
        Some("c1".to_string())
    );
    let err = CampaignLock::try_acquire_at(&path, "c2").unwrap_err();
    assert!(err.contains("already running"), "{err}");
    drop(lock);
    assert!(live_lock_holder_at(&path).is_none(), "released on drop");
    // A stale lock (dead pid) is replaced.
    std::fs::write(
        &path,
        serde_json::json!({"pid": 999_999_999u32, "campaign_id": "dead"}).to_string(),
    )
    .unwrap();
    assert!(live_lock_holder_at(&path).is_none(), "dead pid is stale");
    let lock2 = CampaignLock::try_acquire_at(&path, "c3").expect("stale replaced");
    assert_eq!(
        live_lock_holder_at(&path).map(|h| h.campaign_id),
        Some("c3".to_string())
    );
    drop(lock2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// R53 F1: one lock per worker; two workers are independent, the same
/// worker is refused by name, and a dead holder's lock is replaced.
#[test]
fn r53_f1_lock_per_worker() {
    let dir = tmp_dir("r53locks");
    let w0 = dir.join(".campaign.lock.w0");
    let w1 = dir.join(".campaign.lock.w1");
    let l0 = CampaignLock::try_acquire_at_worker(&w0, "c0", 0).expect("worker 0");
    let l1 = CampaignLock::try_acquire_at_worker(&w1, "c1", 1).expect("worker 1");
    let err = CampaignLock::try_acquire_at_worker(&w0, "c2", 0).unwrap_err();
    assert!(err.contains("c0"), "{err}");
    assert!(err.contains("already running"), "{err}");
    let mut holders = live_lock_holders_in(&dir);
    holders.sort_by_key(|h| h.worker);
    assert_eq!(holders.len(), 2, "{holders:?}");
    assert_eq!((holders[0].worker, holders[1].worker), (0, 1));
    drop(l1);
    // A stale lock is replaced.
    std::fs::write(
        &w1,
        serde_json::json!({
            "pid": 999_999_999u32, "campaign_id": "dead", "worker": 1,
            "start_ticks": 1u64
        })
        .to_string(),
    )
    .unwrap();
    let l2 = CampaignLock::try_acquire_at_worker(&w1, "c3", 1).expect("stale replaced");
    assert_eq!(
        live_lock_holder_at(&w1).map(|h| h.campaign_id),
        Some("c3".to_string())
    );
    drop(l0);
    drop(l2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// R53 F1: every worker's lock and a legacy single lock are visible.
#[test]
fn r53_f1_lock_readers_see_every_worker() {
    let dir = tmp_dir("r53readers");
    let l0 =
        CampaignLock::try_acquire_at_worker(&dir.join(".campaign.lock.w0"), "c0", 0).expect("w0");
    let l1 =
        CampaignLock::try_acquire_at_worker(&dir.join(".campaign.lock.w1"), "c1", 1).expect("w1");
    let legacy = CampaignLock::try_acquire_at(&dir.join(".campaign.lock"), "old").expect("legacy");
    let mut holders = live_lock_holders_in(&dir);
    holders.sort_by_key(|h| h.campaign_id.clone());
    assert_eq!(holders.len(), 3, "{holders:?}");
    assert!(holders
        .iter()
        .any(|h| h.campaign_id == "c0" && h.worker == 0));
    assert!(holders
        .iter()
        .any(|h| h.campaign_id == "c1" && h.worker == 1));
    assert!(holders.iter().any(|h| h.campaign_id == "old"));
    drop(l0);
    drop(l1);
    drop(legacy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// R53 F2: the id carries the worker, and the directory is exclusive.
#[test]
fn r53_f2_same_second_ids_differ() {
    let a = campaign_id_for("sdb-sql", "m", "20261031_070000", 0);
    let b = campaign_id_for("sdb-sql", "m", "20261031_070000", 1);
    assert_ne!(a, b);
    assert!(a.ends_with("_w0") && b.ends_with("_w1"), "{a} {b}");
    let root = tmp_dir("r53dir");
    let dir = create_campaign_dir(&root, &a).expect("first create");
    assert!(dir.is_dir());
    let err = create_campaign_dir(&root, &a).unwrap_err().to_string();
    assert!(err.contains("already exists"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Round-7 finding 6: a dead writer must not show "running" forever.
#[test]
fn effective_state_marks_stale_running_cells_stalled() {
    let fresh = chrono::Utc::now().to_rfc3339();
    let old = (chrono::Utc::now() - chrono::Duration::minutes(45)).to_rfc3339();
    assert_eq!(
        effective_state_with("running", Some(&fresh), false),
        "running"
    );
    assert_eq!(effective_state_with("running", Some(&old), true), "running");
    assert_eq!(
        effective_state_with("running", Some(&old), false),
        "stalled"
    );
    assert_eq!(effective_state_with("done", Some(&old), false), "done");
    assert_eq!(effective_state_with("running", None, false), "stalled");
}

/// gui-spec.md §4.1: the one ranking — current rules first, then
/// points, then points per dollar, then cheaper.
#[test]
fn board_order_ranks_points_then_dollars() {
    use crate::report::rules::RULES_VERSION;
    let key = |rules: &str, points: f64, cost: f64| BoardSortKey {
        live: false,
        current_rules: rules == RULES_VERSION,
        points,
        perf: None,
        per_dollar: (cost > 0.0).then(|| points / cost),
        cost,
    };
    let current = key(RULES_VERSION, 10.0, 0.5);
    let richer = key(RULES_VERSION, 12.0, 0.5);
    let cheaper = key(RULES_VERSION, 10.0, 0.2);
    let stale = key("2026-10-16.12", 99.0, 0.1);
    assert_eq!(board_order(&current, &richer), std::cmp::Ordering::Greater);
    assert_eq!(board_order(&current, &cheaper), std::cmp::Ordering::Greater);
    assert_eq!(board_order(&current, &stale), std::cmp::Ordering::Less);
    assert_eq!(board_order(&current, &current), std::cmp::Ordering::Equal);
}

/// R47 F1: perf breaks a points tie; more points always beats a better
/// perf; a score sorts above no score; points per dollar decides only
/// after equal points and equal perf.
#[test]
fn r47_f1_perf_breaks_points_ties() {
    let key = |points: f64, perf: Option<f64>, cost: f64| BoardSortKey {
        live: false,
        current_rules: true,
        points,
        perf,
        per_dollar: (cost > 0.0).then(|| points / cost),
        cost,
    };
    let better = key(20.0, Some(1.5), 0.5);
    let lower = key(20.0, Some(1.2), 0.5);
    assert_eq!(board_order(&better, &lower), std::cmp::Ordering::Less);
    assert_eq!(board_order(&lower, &better), std::cmp::Ordering::Greater);
    let scored = key(20.0, Some(0.9), 0.5);
    let unscored = key(20.0, None, 0.5);
    assert_eq!(
        board_order(&scored, &unscored),
        std::cmp::Ordering::Less,
        "a score sorts above no score"
    );
    let cheap = key(20.0, Some(1.5), 0.25);
    let pricey = key(20.0, Some(1.5), 0.5);
    assert_eq!(
        board_order(&cheap, &pricey),
        std::cmp::Ordering::Less,
        "equal points and perf: points per dollar decides"
    );
    let more = key(21.0, Some(0.1), 0.5);
    let fewer = key(20.0, Some(9.9), 0.5);
    assert_eq!(board_order(&more, &fewer), std::cmp::Ordering::Less);
    assert_eq!(board_order(&fewer, &more), std::cmp::Ordering::Greater);

    // The key reads the career's score, not the phase's own.
    use crate::verifier::sql::perfscore::PerfScore;
    let mut r = chain_rollup(1, true);
    r.points = 20.0;
    r.perf = Some(PerfScore {
        workloads: Vec::new(),
        score: Some(0.5),
        error: None,
    });
    let career = r.career.as_mut().expect("career");
    career.points = 20.0;
    career.perf = Some(PerfScore {
        workloads: Vec::new(),
        score: Some(1.5),
        error: None,
    });
    assert_eq!(BoardSortKey::of(&r).perf, Some(1.5));
}

/// Round-8 finding 9: a rollup-less (live) cell is pinned first in its
/// own group and the comparator stays a total order — sorting must
/// never see it as "equal to everything".
#[test]
fn board_order_pins_live_cells_first() {
    use crate::report::rules::RULES_VERSION;
    let key = |live: bool, points: f64| BoardSortKey {
        live,
        current_rules: true,
        points,
        perf: None,
        per_dollar: Some(points / 0.5),
        cost: 0.5,
    };
    let live = key(true, 0.0);
    let ranked = key(false, 5.8);
    assert_eq!(board_order(&live, &ranked), std::cmp::Ordering::Less);
    assert_eq!(board_order(&ranked, &live), std::cmp::Ordering::Greater);
    // A whole list sorts with the live cell first, without panicking.
    let mut cells = [key(false, 1.0), live, key(false, 5.8), key(false, 1.0)];
    cells.sort_by(board_order);
    assert!(cells[0].live, "live first");
    assert_eq!(cells[1].points, 5.8, "then by points");
    // The rules stamp is still a valid tiebreaker below points.
    let mut stale = key(false, 5.8);
    stale.current_rules = false;
    assert_eq!(
        board_order(&key(false, 5.8), &stale),
        std::cmp::Ordering::Less
    );
    let _ = RULES_VERSION;
}

/// gui-spec.md §5.3: attempted, lite, failed, skipped and not-reached
/// tickets all derive from the rollup and the spec.
#[test]
fn ladder_derives_every_ticket() {
    let stage = |id: u32, needs: Vec<u32>| StageSpec {
        id,
        kind: "feature".to_string(),
        title: format!("Ticket {id}"),
        prompt: format!("stages/{id}/prompt.md"),
        acceptance: Acceptance {
            sql_stages: vec![id],
            ..Default::default()
        },
        budget_usd: 0.1,
        max_turns: 10,
        feedback_rounds: 1,
        tier: 1,
        needs,
        lite: Some(LiteSpec {
            prompt: "lite.md".into(),
            profile: vec![],
            budget_usd: 0.05,
            max_turns: 5,
            weight: 0.4,
        }),
    };
    let spec = CampaignSpec {
        name: "sdb-sql".to_string(),
        version: 1,
        task: "sql".to_string(),
        ladder: LadderMode::Continue,
        title: "t".to_string(),
        description: String::new(),
        stages: vec![
            stage(0, vec![]),
            stage(1, vec![0]),
            stage(2, vec![1]),
            stage(3, vec![2]),
        ],
        phases: Vec::new(),
    };
    let record = |id: u32, passed: bool, variant: &str, status: &str| StageRecord {
        id,
        title: format!("Ticket {id}"),
        kind: "feature".to_string(),
        passed,
        pass_rate: if passed { 100.0 } else { 0.0 },
        passed_stages: 1,
        total_stages: 1,
        regressed: vec![],
        regression_severity: 0,
        api_cost_usd: 0.01,
        billed_usd: 0.0,
        effective_cost_usd: 0.01,
        turns: Some(3),
        duration_seconds: Some(10.0),
        status: status.to_string(),
        stop_reason: None,
        variant: variant.to_string(),
        tier: 1,
        points: if passed && variant == "lite" {
            0.4
        } else if passed {
            1.0
        } else {
            0.0
        },
        run_id: format!("r{id}"),
        git_commit: None,
        git_stat: None,
        rounds: vec![],
        failure: (status == "failed").then(|| FailureSummary {
            kind: "own_stage".to_string(),
            stage: Some(id),
            line: format!("own stage: boom {id}"),
            detail: String::new(),
            stderr_tail: None,
        }),
        grading_wait_s: 0.0,
    };
    let mut rollup = CampaignRollup {
        campaign_id: "c".into(),
        name: "sdb-sql".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        task: "sql".into(),
        started_at: String::new(),
        finished_at: String::new(),
        stages: vec![record(0, true, "full", "passed")],
        reached: Some(0),
        regression_free_tickets: 1,
        total_cost_usd: 0.01,
        billed_usd: 0.0,
        health: None,
        over_budget: true,
        cap_reached: false,
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        sqlite_version: None,
        cap_price_basis: None,
        sandbox_image: None,
        budget_usd: 0.25,
        wall_min: 30,
        worker: 0,
        concurrent_cells: 1,
        grading_wait_s: 0.0,
        points: 1.0,
        max_points: 4.0,
        harness_fault: None,
        rehearsal: false,
        smoke: false,
        phase: None,
        chain_from: None,
        career: None,
        perf: None,
        ladder_state: None,
        refork: None,
    };
    // 1's full attempt failed, its lite attempt passed.
    rollup.stages.push(record(1, false, "full", "failed"));
    rollup.stages.push(record(1, true, "lite", "passed"));
    // Ticket 2's full and lite both failed.
    rollup.stages.push(record(2, false, "full", "failed"));
    rollup.stages.push(record(2, false, "lite", "failed"));
    let entries = ladder(&spec, &rollup, None);
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].status, "pass");
    assert_eq!(entries[1].status, "lite");
    assert_eq!(entries[1].points, 0.4);
    // 2 was attempted, failed, with the recorded one-liner.
    assert_eq!(entries[2].status, "fail");
    assert_eq!(entries[2].cause.as_deref(), Some("own stage: boom 2"));
    // 3 was never attempted: needs 2 unmet beats the budget cap.
    assert_eq!(entries[3].status, "skipped");
    assert_eq!(entries[3].cause.as_deref(), Some("skipped: needs 2"));
    assert_eq!(stop_reason(&rollup), "budget cap");
}

fn stage_result(id: u32, passed: bool) -> StageResult {
    StageResult {
        stage: id,
        name: format!("stage {id}"),
        passed,
        error: None,
    }
}

fn outcome(passed: bool, result_ids: &[(u32, bool)]) -> StageOutcome {
    StageOutcome {
        run_id: "r".into(),
        passed,
        pass_rate: if passed { 100.0 } else { 0.0 },
        passed_stages: result_ids.iter().filter(|(_, p)| *p).count() as u32,
        total_stages: result_ids.len() as u32,
        stage_results: result_ids
            .iter()
            .map(|(id, p)| stage_result(*id, *p))
            .collect(),
        api_cost_usd: 0.01,
        billed_usd: 0.0,
        effective_cost_usd: 0.01,
        turns: Some(10),
        duration_seconds: Some(60.0),
        max_call_usd: 0.0,
        stop_reason: None,
        sandbox_image: None,
        agent_failed: false,
        infra_error: false,
        rounds: Vec::new(),
        failure: None,
        harness_fault: None,
        disqualified: false,
        git_commit: Some("abc1234".into()),
        git_stat: Some("1 file changed".into()),
        grading_wait_s: 0.0,
    }
}

fn fixture_campaign() -> LoadedCampaign {
    let dir = tmp_dir("spec");
    let stages = dir.join("stages");
    std::fs::create_dir_all(&stages).unwrap();
    for i in 0..3 {
        std::fs::write(stages.join(format!("{i}.md")), "# ticket").unwrap();
    }
    let spec = CampaignSpec {
        name: "t".into(),
        version: 1,
        task: "redis".into(),
        ladder: LadderMode::PassStop,
        title: "t".into(),
        description: String::new(),
        stages: vec![
            StageSpec {
                id: 0,
                kind: "genesis".into(),
                title: "mvp".into(),
                prompt: "stages/0.md".into(),
                acceptance: Acceptance {
                    redis_stages: vec![1, 2],
                    proxy_stages: Vec::new(),
                    queue_stages: Vec::new(),
                    http_stages: Vec::new(),
                    sql_stages: Vec::new(),
                },
                budget_usd: 0.1,
                max_turns: 10,
                feedback_rounds: 1,
                tier: 1,
                needs: Vec::new(),
                lite: None,
            },
            StageSpec {
                id: 1,
                kind: "feature".into(),
                title: "expiry".into(),
                prompt: "stages/1.md".into(),
                acceptance: Acceptance {
                    redis_stages: vec![1, 2, 3],
                    proxy_stages: Vec::new(),
                    queue_stages: Vec::new(),
                    http_stages: Vec::new(),
                    sql_stages: Vec::new(),
                },
                budget_usd: 0.1,
                max_turns: 10,
                feedback_rounds: 1,
                tier: 1,
                needs: Vec::new(),
                lite: None,
            },
            StageSpec {
                id: 2,
                kind: "feature".into(),
                title: "pubsub".into(),
                prompt: "stages/2.md".into(),
                acceptance: Acceptance {
                    redis_stages: vec![1, 2, 3, 5],
                    proxy_stages: Vec::new(),
                    queue_stages: Vec::new(),
                    http_stages: Vec::new(),
                    sql_stages: Vec::new(),
                },
                budget_usd: 0.1,
                max_turns: 10,
                feedback_rounds: 1,
                tier: 1,
                needs: Vec::new(),
                lite: None,
            },
        ],
        phases: Vec::new(),
    };
    std::fs::write(
        dir.join("campaign.json"),
        serde_json::to_string_pretty(&spec).unwrap(),
    )
    .unwrap();
    LoadedCampaign { spec, root: dir }
}

/// The three-ticket fixture with a lite on the *last* ticket: the R23-1
/// pool must still leave room for it.
fn fixture_campaign_last_lite() -> LoadedCampaign {
    let mut lc = fixture_campaign();
    lc.spec.stages[2].lite = Some(LiteSpec {
        prompt: "stages/2-lite.md".into(),
        profile: Vec::new(),
        budget_usd: 0.1,
        max_turns: 10,
        weight: 0.4,
    });
    std::fs::write(lc.root.join("stages/2-lite.md"), "# last lite").unwrap();
    lc
}

/// The same three-ticket fixture, with a lite variant on ticket 1 so the
/// F2 crash path can prove that the lite still runs.
fn fixture_campaign_with_lite() -> LoadedCampaign {
    let mut lc = fixture_campaign();
    lc.spec.stages[1].lite = Some(LiteSpec {
        prompt: "stages/1-lite.md".into(),
        profile: Vec::new(),
        budget_usd: 0.1,
        max_turns: 10,
        weight: 0.4,
    });
    std::fs::write(lc.root.join("stages/1-lite.md"), "# lite ticket").unwrap();
    lc
}

/// Tickets 12-15 landed (2026-10-19): concurrency (scenario-only, lite),
/// schema change (differential), observability (scenario-only) and the
/// tests/CI gate (scenario-only, lite). Pins each stage's spec and the
/// ladder's max points.
/// R23 D1: dollars are the limit; every sdb-sql stage and lite uses the
/// 200-turn runaway guard.
#[test]
fn r23_d1_sql_turn_limits_are_the_guard() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    assert_eq!(TURN_GUARD, 200);
    for s in &lc.spec.stages {
        assert_eq!(s.max_turns, TURN_GUARD, "stage {}", s.id);
        if let Some(lite) = &s.lite {
            assert_eq!(lite.max_turns, TURN_GUARD, "stage {} lite", s.id);
        }
    }
}

/// R23 D1: the stop reason reaches the campaign stage record.
#[tokio::test]
async fn r23_d1_stage_record_carries_the_dollar_stop() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "r23d1".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        |_| {
            let mut out = outcome(false, &[(0, false)]);
            out.stop_reason = Some("dollars".into());
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(rollup.stages[0].stop_reason.as_deref(), Some("dollars"));
}

/// R23 D2: the remaining pool is split across the runnable tickets by
/// tier weight; a zero pool (or zero weights) allocates nothing.
#[test]
fn r23_d2_pool_budget_splits_by_tier() {
    assert!((pool_budget(0.9, 2.0, 6.0) - 0.3).abs() < 1e-9);
    assert_eq!(pool_budget(0.0, 2.0, 6.0), 0.0);
    assert_eq!(pool_budget(0.9, 0.0, 6.0), 0.0);
    assert_eq!(pool_budget(0.9, 2.0, 0.0), 0.0);
}

async fn r23_d2_seen_budgets(
    lc: &LoadedCampaign,
    opts: &CampaignRunOptions,
    spend0: f64,
) -> Vec<(u32, StageVariant, f64)> {
    let seen = std::cell::RefCell::new(Vec::new());
    let rollup = run_campaign(
        lc,
        opts,
        |attempt| {
            seen.borrow_mut()
                .push((attempt.stage.id, attempt.variant, attempt.budget_usd));
            let spend = if attempt.stage.id == 0 { spend0 } else { 0.001 };
            let mut out = outcome(true, &[(0, true), (1, true), (2, true)]);
            out.api_cost_usd = spend;
            out.effective_cost_usd = spend;
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    assert!(!rollup.stages.is_empty());
    seen.into_inner()
}

/// R36 F1/D1: a phase chains only onto the same sandbox image ID.
#[test]
fn r36_f1_chain_requires_the_same_sandbox_image() {
    let state = phase1_state();
    let mut prev = chain_rollup(1, true);
    prev.sandbox_image = Some("sha256:image-a".into());
    validate_chain(
        &prev,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        Some("sha256:image-a"),
    )
    .expect("the same image chains");
    let err = validate_chain(
        &prev,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        Some("sha256:image-b"),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("sha256:image-a"), "{msg}");
    assert!(msg.contains("sha256:image-b"), "{msg}");
}

/// R37 F6: the rollup takes the first ticket's image; a mixed cell
/// cannot be chained onto.
#[tokio::test]
async fn r37_f6_rollup_uses_first_ticket_image_and_refuses_mixed() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "r37f6".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: CELL_BUDGET_USD,
        wall_min: None,
    };
    let first = run_campaign(
        &lc,
        &opts,
        |_attempt| {
            let mut o = outcome(true, &[(0, true), (1, true), (2, true)]);
            o.sandbox_image = Some("sha256:first".into());
            async move { Ok(o) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(first.sandbox_image.as_deref(), Some("sha256:first"));

    let mixed_opts = CampaignRunOptions {
        campaign_id: "r37f6mixed".into(),
        ..opts
    };
    let mixed = run_campaign(
        &lc,
        &mixed_opts,
        |attempt| {
            let mut o = outcome(true, &[(0, true), (1, true), (2, true)]);
            o.sandbox_image = Some(format!("sha256:image{}", attempt.stage.id));
            async move { Ok(o) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(mixed.sandbox_image.as_deref(), Some("mixed"));
    let state = phase1_state();
    let mut prev = chain_rollup(1, true);
    prev.sandbox_image = Some("mixed".into());
    let err = validate_chain(
        &prev,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        Some("sha256:first"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("mixed"), "{err}");
}

/// R37-5: a chained phase starts from the previous phase's image, so a
/// first ticket on a different image records `mixed`, and the next
/// phase is refused.
#[tokio::test]
async fn r43_f3_chained_mixed_image() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "r43f3mixed".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: CELL_BUDGET_USD,
        wall_min: None,
    };
    let state = LadderState::default();
    let rollup = run_campaign_phase_on_image(
        &lc,
        &opts,
        None,
        Some(&state),
        Some("prev"),
        None,
        Some("sha256:image-a"),
        |_attempt| {
            let mut o = outcome(true, &[(0, true)]);
            o.sandbox_image = Some("sha256:image-b".into());
            async move { Ok(o) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        rollup.sandbox_image.as_deref(),
        Some("mixed"),
        "a chained rollup seeds A; its first ticket on B makes it mixed"
    );
    let mut prev = rollup;
    prev.phase = Some(1);
    let err = validate_chain(
        &prev,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        Some("sha256:image-b"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("mixed"), "{err}");
}

/// R23 D2: an unspent ticket raises the next ticket's pool share.
#[tokio::test]
async fn r23_d2_a_cheap_ticket_raises_the_next_budget() {
    let mut lc = fixture_campaign();
    // A legacy fixed budget far below the pool share: the pool, not this
    // field, is the allocation.
    lc.spec.stages[0].budget_usd = 0.001;
    let opts = CampaignRunOptions {
        campaign_id: "r23d2cheap".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let cheap = r23_d2_seen_budgets(&lc, &opts, 0.001).await;
    let expensive = r23_d2_seen_budgets(&lc, &opts, 0.20).await;
    let cheap1 = cheap.iter().find(|(id, _, _)| *id == 1).unwrap().2;
    let exp1 = expensive.iter().find(|(id, _, _)| *id == 1).unwrap().2;
    assert!(
        cheap1 > exp1 + 0.05,
        "cheap next {cheap1} should exceed expensive next {exp1}"
    );
    // The old fixed stage budget is not the allocation.
    assert!(cheap[0].2 > lc.spec.stages[0].budget_usd);
}

/// R23 D2: the lite draws its budget from the same remaining pool.
#[tokio::test]
async fn r23_d2_a_lite_draws_from_the_same_pool() {
    let lc = fixture_campaign_with_lite();
    let opts = CampaignRunOptions {
        campaign_id: "r23d2lite".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::new());
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            seen.borrow_mut()
                .push((attempt.stage.id, attempt.variant, attempt.budget_usd));
            let out = if attempt.stage.id == 1 && attempt.variant == StageVariant::Full {
                let mut o = outcome(false, &[(1, false)]);
                o.api_cost_usd = 0.001;
                o.effective_cost_usd = 0.001;
                o
            } else {
                let mut o = outcome(true, &[(0, true), (1, true), (2, true)]);
                o.api_cost_usd = 0.001;
                o.effective_cost_usd = 0.001;
                o
            };
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    let lite = seen
        .borrow()
        .iter()
        .find(|(id, variant, _)| *id == 1 && *variant == StageVariant::Lite)
        .copied()
        .expect("the lite attempt");
    assert!(lite.2 > 0.0, "the lite gets a pool share: {lite:?}");
    assert!(rollup
        .stages
        .iter()
        .any(|r| r.id == 1 && r.variant == "lite"));
}

/// R23 D2: the pool plus reserve never lets the phase exceed $0.99.
#[tokio::test]
async fn r23_d2_the_pool_keeps_the_phase_under_the_cap() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "r23d2cap".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: CELL_BUDGET_USD,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let spend = attempt.budget_usd;
            let mut o = outcome(true, &[(0, true), (1, true), (2, true)]);
            o.api_cost_usd = spend;
            o.effective_cost_usd = spend;
            async move { Ok(o) }
        },
        None,
    )
    .await
    .unwrap();
    assert!(
        rollup.total_cost_usd <= CELL_BUDGET_USD + 1e-9,
        "total {}",
        rollup.total_cost_usd
    );
    assert!(
        rollup.total_cost_usd > CELL_BUDGET_USD * 0.9,
        "the pool should use the phase budget: {}",
        rollup.total_cost_usd
    );
    assert!(!rollup.over_budget);
}

/// R23 D3: the cap counts the API bill, not the compute-inclusive
/// effective cost.
#[tokio::test]
async fn r23_d3_the_cap_uses_api_spend_not_effective() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "r23d3api".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: CELL_BUDGET_USD,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        |_| {
            let mut o = outcome(true, &[(0, true), (1, true), (2, true)]);
            o.api_cost_usd = 0.01;
            o.effective_cost_usd = 0.99;
            async move { Ok(o) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        rollup.stages.len(),
        3,
        "the effective cost must not stop the ladder"
    );
    assert!((rollup.total_cost_usd - 0.03).abs() < 1e-9);
    assert!(!rollup.over_budget);
    assert!((rollup.stages[0].effective_cost_usd - 0.99).abs() < 1e-9);
}

/// R25 R23-8 acceptance: the per-ticket API field is populated and its
/// sum over the stages equals the rollup's API total.
#[tokio::test]
async fn r23_8_stage_api_cost_sums_to_total() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "r23d8api".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: CELL_BUDGET_USD,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let spend = 0.01 * (attempt.stage.id as f64 + 1.0);
            let mut o = outcome(true, &[(0, true), (1, true), (2, true)]);
            o.api_cost_usd = spend;
            o.effective_cost_usd = spend + 0.005;
            async move { Ok(o) }
        },
        None,
    )
    .await
    .unwrap();
    let sum: f64 = rollup.stages.iter().map(|s| s.api_cost_usd).sum();
    assert!(
        (sum - rollup.total_cost_usd).abs() < 1e-9,
        "stage API {sum} vs total {}: {:?}",
        rollup.total_cost_usd,
        rollup.stages
    );
    assert!(
        rollup
            .stages
            .iter()
            .all(|s| s.api_cost_usd > 0.0 && s.effective_cost_usd > s.api_cost_usd),
        "each stage carries its API bill and the secondary effective cost"
    );
}

/// R23 D3: a paid launch refuses a model with no list price; eval-only
/// runs are exempt because they do not spend.
#[tokio::test]
async fn r23_d3_a_model_without_a_list_price_is_refused() {
    let dead = "http://127.0.0.1:9/models";
    assert!(
        require_model_list_price_at("openai/gpt-6-luna", dead, false)
            .await
            .is_ok()
    );
    assert!(
        require_model_list_price_at("made-up/unknown-model", dead, false)
            .await
            .is_err()
    );
    assert!(
        require_model_list_price_at("made-up/unknown-model", dead, true)
            .await
            .is_ok()
    );
}

/// R24-7 acceptance: the paid-launch refusal names the real source.
#[tokio::test]
async fn r24_7_refusal_names_the_price_snapshot() {
    let dead = "http://127.0.0.1:9/models";
    let err = require_model_list_price_at("made-up/unknown-model", dead, false)
        .await
        .expect_err("unknown model must be refused");
    assert!(
        err.contains("src/cost/model_prices.json"),
        "refusal does not name the snapshot: {err}"
    );
    assert!(
        !err.contains("OpenRouter catalog or offline table"),
        "stale price source: {err}"
    );
}

/// R23 D5: the Board ladder entry carries the stop reason from the
/// stage record.
#[test]
fn r23_d5_ladder_carries_the_stop_reason() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let mut rollup = chain_rollup(1, false);
    rollup.career = None;
    let mut rec = failed_record(0, "Genesis", "own stage: boom", "");
    rec.stop_reason = Some("dollars".to_string());
    rollup.stages = vec![rec];
    let e = ladder(&lc.spec, &rollup, None);
    let e0 = e.iter().find(|x| x.id == 0).expect("ticket 0");
    assert_eq!(e0.status, "fail");
    assert_eq!(e0.stop_reason.as_deref(), Some("dollars"));
}

/// R23-1: the last ticket's full share leaves its lite a part; the lite
/// still runs after the full spends its whole share.
#[tokio::test]
async fn r23_1_last_ticket_keeps_its_lite() {
    let lc = fixture_campaign_last_lite();
    let opts = CampaignRunOptions {
        campaign_id: "r23_1lite".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 0.20,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<(u32, StageVariant, f64)>::new());
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            seen.borrow_mut()
                .push((attempt.stage.id, attempt.variant, attempt.budget_usd));
            let mut out = if attempt.stage.id == 2 && attempt.variant == StageVariant::Full {
                outcome(false, &[(2, false)])
            } else {
                outcome(true, &[(0, true), (1, true), (2, true)])
            };
            out.api_cost_usd = if attempt.stage.id == 2 && attempt.variant == StageVariant::Full {
                attempt.budget_usd
            } else {
                0.0
            };
            out.effective_cost_usd = out.api_cost_usd;
            out.max_call_usd = 0.001;
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    let seen = seen.into_inner();
    let full = seen
        .iter()
        .find(|(id, v, _)| *id == 2 && *v == StageVariant::Full)
        .map(|(_, _, b)| *b)
        .expect("last full attempt");
    // Headroom before ticket 2 is 0.20 - 0.02 = 0.18; the full share is
    // 0.18 / (1 + 0.4), not the whole headroom.
    assert!(full < 0.18 - 1e-9, "full share {full} ate the lite part");
    let lite = seen
        .iter()
        .find(|(id, v, _)| *id == 2 && *v == StageVariant::Lite)
        .copied()
        .expect("the last ticket's lite must run");
    assert!(lite.2 > 0.0, "lite budget {}", lite.2);
    assert!(rollup
        .stages
        .iter()
        .any(|r| r.id == 2 && r.variant == "lite"));
}

/// R23-1: a failed need removes the blocked ticket from the pool weight,
/// so it does not dilute the runnable tickets before it.
#[test]
fn r23_1_a_failed_need_does_not_dilute_the_pool() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let stage_list: Vec<&StageSpec> = lc
        .spec
        .stages
        .iter()
        .filter(|s| phase.stages.contains(&s.id))
        .collect();
    let mut outcomes = HashMap::new();
    outcomes.insert(11u32, LadderOutcome::Failed);
    let got = runnable_weight_sum(&stage_list, &outcomes);
    // 11 is decided; 14 needs 11 and is blocked; 12, 13 and 15 remain.
    let expected: f64 = stage_list
        .iter()
        .filter(|s| s.id != 11 && s.id != 14)
        .map(|s| stage_weight(s))
        .sum();
    assert!(
        (got - expected).abs() < 1e-9,
        "got {got} expected {expected}"
    );
    assert!(expected > 0.0);
}

#[test]
fn test_real_sdb_sql_spec_has_ticket_12() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    validate_spec(&lc.spec, &lc.root).unwrap();
    assert_eq!(lc.spec.stages.len(), 21);
    let s = lc.spec.stages.iter().find(|s| s.id == 12).unwrap();
    assert_eq!(s.title, "Concurrency and isolation");
    assert_eq!(s.kind, "engine");
    assert_eq!(s.tier, 4);
    assert_eq!(s.needs, vec![1, 2, 3, 6]);
    assert_eq!(s.budget_usd, 0.20);
    assert_eq!(s.max_turns, TURN_GUARD);
    assert_eq!(s.acceptance.sql_stages, (0..=12u32).collect::<Vec<u32>>());
    let lite = s.lite.as_ref().expect("ticket 12 has a lite variant");
    assert_eq!(lite.profile, vec!["concurrency-global-lock".to_string()]);
    assert!(lite.budget_usd < s.budget_usd);
    // Scenario-only: graded, though above the differential ceiling.
    assert!(crate::verifier::sql::driver::scenario_only(12));
    assert!(!crate::verifier::sql::driver::differential_supported(12));
    assert!(crate::verifier::sql::driver::stage_supported(12));
    assert!(!crate::verifier::sql::driver::stage_supported(21));
    // Every stage accepts itself and only declared ids.
    for st in &lc.spec.stages {
        assert!(
            st.acceptance.sql_stages.contains(&st.id),
            "stage {} acceptance misses itself: {:?}",
            st.id,
            st.acceptance.sql_stages
        );
    }
    // Ticket 13 (schema change): differential, tier 4, needs 4.
    let s13 = lc.spec.stages.iter().find(|s| s.id == 13).unwrap();
    assert_eq!(s13.title, "Schema change");
    assert_eq!(s13.kind, "engine");
    assert_eq!(s13.tier, 4);
    assert_eq!(s13.needs, vec![4]);
    assert_eq!(s13.budget_usd, 0.12);
    assert_eq!(s13.max_turns, TURN_GUARD);
    assert!(s13.lite.is_none(), "ticket 13 has no lite variant");
    assert_eq!(s13.acceptance.sql_stages, (0..=13u32).collect::<Vec<u32>>());
    assert!(crate::verifier::sql::driver::differential_supported(13));
    assert!(crate::verifier::sql::driver::stage_supported(13));
    // Ticket 14 (observability): scenario-only, tier 5, needs 11.
    let s14 = lc.spec.stages.iter().find(|s| s.id == 14).unwrap();
    assert_eq!(s14.title, "Observability");
    assert_eq!(s14.kind, "ops");
    assert_eq!(s14.tier, 5);
    assert_eq!(s14.needs, vec![6, 11]);
    assert_eq!(s14.budget_usd, 0.12);
    assert_eq!(s14.max_turns, TURN_GUARD);
    assert!(s14.lite.is_none(), "ticket 14 has no lite variant");
    assert_eq!(s14.acceptance.sql_stages, (0..=14u32).collect::<Vec<u32>>());
    assert!(crate::verifier::sql::driver::scenario_only(14));
    assert!(!crate::verifier::sql::driver::differential_supported(14));
    assert!(crate::verifier::sql::driver::stage_supported(14));
    // Ticket 15 (tests and CI gate): scenario-only, tier 5, needs 7/8,
    // with a coverage-60 lite variant.
    let s15 = lc.spec.stages.iter().find(|s| s.id == 15).unwrap();
    assert_eq!(s15.title, "Tests and CI gate");
    assert_eq!(s15.kind, "ops");
    assert_eq!(s15.tier, 5);
    assert_eq!(s15.needs, vec![7, 8]);
    assert_eq!(s15.budget_usd, 0.20);
    assert_eq!(s15.max_turns, TURN_GUARD);
    assert_eq!(s15.acceptance.sql_stages, (0..=15u32).collect::<Vec<u32>>());
    let lite15 = s15.lite.as_ref().expect("ticket 15 has a lite variant");
    assert_eq!(lite15.profile, vec!["coverage-60".to_string()]);
    assert_eq!(lite15.budget_usd, 0.12);
    assert_eq!(lite15.max_turns, TURN_GUARD);
    assert!(crate::verifier::sql::driver::scenario_only(15));
    assert!(!crate::verifier::sql::driver::differential_supported(15));
    assert!(crate::verifier::sql::driver::stage_supported(15));
    assert!(!crate::verifier::sql::driver::stage_supported(21));
    // Ticket 16 (larger than memory): scenario-only, tier 6, needs 2/4,
    // no lite variant.
    let s16 = lc.spec.stages.iter().find(|s| s.id == 16).unwrap();
    assert_eq!(s16.title, "Larger than memory");
    assert_eq!(s16.kind, "resilience");
    assert_eq!(s16.tier, 6);
    assert_eq!(s16.needs, vec![2, 4, 6, 7, 8, 11]);
    assert_eq!(s16.budget_usd, 0.20);
    assert_eq!(s16.max_turns, TURN_GUARD);
    assert!(s16.lite.is_none(), "ticket 16 has no lite variant");
    assert_eq!(s16.acceptance.sql_stages, (0..=16u32).collect::<Vec<u32>>());
    assert!(crate::verifier::sql::driver::scenario_only(16));
    assert!(!crate::verifier::sql::driver::differential_supported(16));
    assert!(crate::verifier::sql::driver::stage_supported(16));
    // Ticket 17 (incident: disk full): scenario-only, tier 6, needs 5,
    // no lite variant.
    let s17 = lc.spec.stages.iter().find(|s| s.id == 17).unwrap();
    assert_eq!(s17.title, "Incident: disk full");
    assert_eq!(s17.kind, "resilience");
    assert_eq!(s17.tier, 6);
    assert_eq!(s17.needs, vec![5]);
    assert_eq!(s17.budget_usd, 0.15);
    assert_eq!(s17.max_turns, TURN_GUARD);
    assert!(s17.lite.is_none(), "ticket 17 has no lite variant");
    assert_eq!(s17.acceptance.sql_stages, (0..=17u32).collect::<Vec<u32>>());
    assert!(crate::verifier::sql::driver::scenario_only(17));
    assert!(!crate::verifier::sql::driver::differential_supported(17));
    assert!(crate::verifier::sql::driver::stage_supported(17));
    // Ticket 18 (hostile input and limits): scenario-only, tier 6,
    // needs 0, no lite variant.
    let s18 = lc.spec.stages.iter().find(|s| s.id == 18).unwrap();
    assert_eq!(s18.title, "Hostile input and limits");
    assert_eq!(s18.kind, "resilience");
    assert_eq!(s18.tier, 6);
    assert_eq!(s18.needs, vec![1, 6, 7, 8]);
    assert_eq!(s18.budget_usd, 0.15);
    assert_eq!(s18.max_turns, TURN_GUARD);
    assert!(s18.lite.is_none(), "ticket 18 has no lite variant");
    assert_eq!(s18.acceptance.sql_stages, (0..=18u32).collect::<Vec<u32>>());
    assert!(crate::verifier::sql::driver::scenario_only(18));
    assert!(!crate::verifier::sql::driver::differential_supported(18));
    assert!(crate::verifier::sql::driver::stage_supported(18));
    // Ticket 19 (replication and failover): scenario-only, tier 7,
    // needs 6, with a two-node PROMOTE lite variant.
    let s19 = lc.spec.stages.iter().find(|s| s.id == 19).unwrap();
    assert_eq!(s19.title, "Replication and failover");
    assert_eq!(s19.kind, "distributed");
    assert_eq!(s19.tier, 7);
    assert_eq!(s19.needs, vec![6]);
    assert_eq!(s19.budget_usd, 0.20);
    assert_eq!(s19.max_turns, TURN_GUARD);
    assert_eq!(s19.acceptance.sql_stages, (0..=19u32).collect::<Vec<u32>>());
    let lite19 = s19.lite.as_ref().expect("ticket 19 has a lite variant");
    assert_eq!(lite19.profile, vec!["two-nodes-promote".to_string()]);
    assert_eq!(lite19.budget_usd, 0.12);
    assert_eq!(lite19.max_turns, TURN_GUARD);
    assert!(crate::verifier::sql::driver::scenario_only(19));
    assert!(!crate::verifier::sql::driver::differential_supported(19));
    assert!(crate::verifier::sql::driver::stage_supported(19));
    // Ticket 20 (PostgreSQL wire protocol): scenario-only, tier 7,
    // needs 1, with a simple-query lite variant.
    let s20 = lc.spec.stages.iter().find(|s| s.id == 20).unwrap();
    assert_eq!(s20.title, "PostgreSQL wire protocol");
    assert_eq!(s20.kind, "distributed");
    assert_eq!(s20.tier, 7);
    assert_eq!(s20.needs, vec![1, 6, 7]);
    assert_eq!(s20.budget_usd, 0.20);
    assert_eq!(s20.max_turns, TURN_GUARD);
    assert_eq!(s20.acceptance.sql_stages, (0..=20u32).collect::<Vec<u32>>());
    let lite20 = s20.lite.as_ref().expect("ticket 20 has a lite variant");
    assert_eq!(lite20.profile, vec!["pg-simple".to_string()]);
    assert_eq!(lite20.budget_usd, 0.12);
    assert_eq!(lite20.max_turns, TURN_GUARD);
    assert!(crate::verifier::sql::driver::scenario_only(20));
    assert!(!crate::verifier::sql::driver::differential_supported(20));
    assert!(crate::verifier::sql::driver::stage_supported(20));
    assert!(!crate::verifier::sql::driver::stage_supported(21));
    // Max points: ticket 12 (26 -> 30), 13 (30 -> 34), 14 (34 -> 39),
    // 15 (39 -> 44), 16 (44 -> 50), 17 (50 -> 56), 18 (56 -> 62),
    // 19 (62 -> 69), 20 (69 -> 76).
    assert_eq!(crate::campaign::max_points(&lc.spec.stages), 76.0);
}

#[test]
fn test_http_acceptance_validation() {
    let mut lc = fixture_campaign();
    lc.spec.task = "http".into();
    assert!(validate_spec(&lc.spec, &lc.root).is_err());
    lc.spec.stages[0].acceptance.http_stages = vec![1, 2];
    lc.spec.stages[1].acceptance.http_stages = vec![1, 2, 3, 4];
    lc.spec.stages[2].acceptance.http_stages = vec![1, 2, 3, 4, 5, 6];
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
    // The http suite tops out at stage 10.
    lc.spec.stages[2].acceptance.http_stages = vec![1, 11];
    let err = validate_spec(&lc.spec, &lc.root).unwrap_err().to_string();
    assert!(err.contains("out of range 1..=10"), "{err}");
}

#[test]
fn test_queue_acceptance_validation() {
    let mut lc = fixture_campaign();
    lc.spec.task = "queue".into();
    // Empty queue acceptance fails closed.
    assert!(validate_spec(&lc.spec, &lc.root).is_err());
    lc.spec.stages[0].acceptance.queue_stages = vec![1];
    lc.spec.stages[1].acceptance.queue_stages = vec![1, 2];
    lc.spec.stages[2].acceptance.queue_stages = vec![1, 2, 3, 4, 5, 6, 7, 8];
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
    // The queue suite tops out at stage 9.
    lc.spec.stages[2].acceptance.queue_stages = vec![1, 10];
    let err = validate_spec(&lc.spec, &lc.root).unwrap_err().to_string();
    assert!(err.contains("out of range 1..=9"), "{err}");
}

/// SPEC.md for ticket 2 carries genesis (the launch contract and
/// protocol a fresh session would otherwise never see), ticket 1, then
/// the current ticket last; a later ticket never leaks in.
#[test]
fn test_sql_spec_md_holds_every_ticket_so_far() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    let s2 = lc.spec.stages.iter().find(|s| s.id == 2).unwrap();
    let passed_outcomes: Vec<(u32, LadderOutcome)> = lc
        .spec
        .stages
        .iter()
        .filter(|s| s.id < 2)
        .map(|s| (s.id, LadderOutcome::Full))
        .collect();
    let md = lc
        .render_spec_md(s2, &lc.prompt_path(s2), &passed_outcomes)
        .unwrap();
    let pos = |needle: &str| {
        md.find(needle)
            .unwrap_or_else(|| panic!("SPEC.md misses {needle:?}"))
    };
    assert!(pos("Launch contract") < pos("# Ticket 1"));
    assert!(pos("# Ticket 1") < pos("ticket 2 (current)"));
    assert!(md.contains("ref-sql"), "the reference is named");
    assert!(!md.contains("# Ticket 3"), "no later ticket");
    // Genesis has no predecessors: just its own ticket.
    let s0 = lc.spec.stages.iter().find(|s| s.id == 0).unwrap();
    let md0 = lc.render_spec_md(s0, &lc.prompt_path(s0), &[]).unwrap();
    assert!(md0.contains("ticket 0 (current)") && !md0.contains("# Ticket 1"));
}

#[test]
fn test_render_spec_md_marks_failed_tickets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    let s2 = lc.spec.stages.iter().find(|s| s.id == 2).unwrap();
    // Ticket 1 failed and was rolled back; ticket 2 is the current one.
    // Its section collapses and the full contract never appears: a
    // later ticket must not read a failed predecessor as done.
    let outcomes = vec![(0, LadderOutcome::Full), (1, LadderOutcome::Failed)];
    let md = lc
        .render_spec_md(s2, &lc.prompt_path(s2), &outcomes)
        .unwrap();
    assert!(md.contains("FAILED, rolled back"), "{md}");
    assert!(md.contains("not graded and not required"));
    assert!(
        !md.contains("# Ticket 1"),
        "the failed ticket's contract must be collapsed"
    );
    assert!(md.contains("# Ticket 0"), "a passed ticket stays in full");
    // A lite pass is named as such.
    let outcomes = vec![(0, LadderOutcome::Full), (1, LadderOutcome::Lite)];
    let md = lc
        .render_spec_md(s2, &lc.prompt_path(s2), &outcomes)
        .unwrap();
    assert!(md.contains("# Ticket 1"));
    assert!(md.contains("(passed on the lite variant)"));
}

#[test]
fn test_attempt_preamble_names_failed_tickets_and_effective_suite() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    let s2 = lc.spec.stages.iter().find(|s| s.id == 2).unwrap();
    let outcomes = vec![(0, LadderOutcome::Full), (1, LadderOutcome::Failed)];
    let accepted = Acceptance::accepted_for(s2.acceptance.stages_for("sql"), &[0], 2);
    let p = attempt_preamble(
        &lc.spec.stages,
        s2,
        StageVariant::Full,
        true,
        &outcomes,
        &accepted,
    )
    .expect("a failure needs a note");
    assert!(p.contains("Ticket 1 ("), "{p}");
    assert!(p.contains("failed and was rolled back"));
    assert!(p.contains("not graded and not required"));
    // The list comes from accepted_for: it cannot drift from the suite
    // the verifier actually runs.
    assert!(
        p.contains(&format!(
            "acceptance suite is tickets {}",
            fmt_stage_ids(&accepted)
        )),
        "{p}"
    );
    assert!(p.contains("rolled back to ticket 0"));
    // A clean ladder carries no note.
    let clean = vec![(0, LadderOutcome::Full), (1, LadderOutcome::Full)];
    assert!(attempt_preamble(
        &lc.spec.stages,
        s2,
        StageVariant::Full,
        false,
        &clean,
        &accepted
    )
    .is_none());
}

#[test]
fn test_attempt_preamble_lite_retry_says_rolled_back() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    let s11 = lc.spec.stages.iter().find(|s| s.id == 11).unwrap();
    // Tickets 0-10 passed; the full attempt at 11 failed and the lite
    // variant is next (item 11: the attempt opens by saying so).
    let outcomes: Vec<(u32, LadderOutcome)> = lc
        .spec
        .stages
        .iter()
        .filter(|s| s.id < 11)
        .map(|s| (s.id, LadderOutcome::Full))
        .collect();
    let passed: Vec<u32> = outcomes.iter().map(|(id, _)| *id).collect();
    let accepted = Acceptance::accepted_for(s11.acceptance.stages_for("sql"), &passed, 11);
    let p = attempt_preamble(
        &lc.spec.stages,
        s11,
        StageVariant::Lite,
        true,
        &outcomes,
        &accepted,
    )
    .expect("a lite retry needs a note");
    assert!(p.contains("lite variant"), "{p}");
    assert!(p.contains("failed; its work was rolled back"));
    assert!(p.contains("acceptance suite is tickets"));
}

/// Round-13 R1: the note must never start with a dash — both harnesses
/// take the prompt as a positional argument and a `-`-leading value was
/// read as a flag, so the agent ran 0 turns.
#[test]
fn test_attempt_preamble_never_starts_with_a_dash() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    let s2 = lc.spec.stages.iter().find(|s| s.id == 2).unwrap();
    let outcomes = vec![(0, LadderOutcome::Full), (1, LadderOutcome::Failed)];
    let accepted = Acceptance::accepted_for(s2.acceptance.stages_for("sql"), &[0], 2);
    let p = attempt_preamble(
        &lc.spec.stages,
        s2,
        StageVariant::Full,
        true,
        &outcomes,
        &accepted,
    )
    .expect("a failure needs a note");
    assert!(p.starts_with("Harness note"), "{p}");
    assert!(
        !p.starts_with('-'),
        "a dash-leading prompt is an unknown flag: {p}"
    );
    assert!(p.trim_end().ends_with("End of harness note."), "{p}");
}

#[test]
fn test_fmt_stage_ids() {
    assert_eq!(fmt_stage_ids(&[0, 1, 3, 4, 5, 6, 7, 10]), "0-1, 3-7 and 10");
    assert_eq!(fmt_stage_ids(&[0]), "0");
    assert_eq!(fmt_stage_ids(&[0, 2]), "0 and 2");
    assert_eq!(fmt_stage_ids(&[]), "");
}

#[test]
fn test_truncate_line_is_one_line_capped() {
    let long = format!("{}\nsecond line", "x".repeat(300));
    let t = truncate_line(&long, 160);
    assert!(!t.contains('\n'), "one line only");
    assert!(t.chars().count() <= 160, "len {}", t.chars().count());
    assert!(t.ends_with('…'));
    assert_eq!(truncate_line("  short  \nsecond", 160), "short");
}

#[tokio::test]
async fn test_ladder_cap_counts_api_dollars_not_the_effective_ratio() {
    // R23 D3: the cap counts API spend. The old effective->API ratio is
    // gone, so an attempt whose effective cost is 10x its API spend
    // still does not stop the ladder at the first ticket.
    let lc = ladder_fixture();
    let opts = CampaignRunOptions {
        campaign_id: "budget1".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 0.20,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<(u32, f64)>::new());
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let id = attempt.stage.id;
            seen.borrow_mut().push((id, attempt.budget_usd));
            let mut out = outcome(true, &[(id, true)]);
            out.api_cost_usd = 0.05;
            out.effective_cost_usd = 0.50;
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    let seen = seen.into_inner();
    assert!(
        seen.len() >= 3,
        "the API cap must allow more than the old effective cap: {seen:?}"
    );
    assert!(
        rollup.total_cost_usd <= 0.20 + 1e-9,
        "API cap respected: {}",
        rollup.total_cost_usd
    );
    assert!(rollup.over_budget, "the API cap is the stop");
}

#[tokio::test]
async fn test_a_full_attempts_largest_call_skips_its_lite() {
    let lc = ladder_fixture();
    let opts = CampaignRunOptions {
        campaign_id: "cap-lite".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 0.20,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<(u32, StageVariant)>::new());
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let id = attempt.stage.id;
            let variant = attempt.variant;
            seen.borrow_mut().push((id, variant));
            let mut out = if id == 0 {
                outcome(true, &[(0, true)])
            } else {
                outcome(false, &[(1, false)])
            };
            if id == 0 {
                out.api_cost_usd = 0.01;
                out.effective_cost_usd = 0.01;
                out.max_call_usd = 0.001;
            } else if id == 1 {
                // R23 D3: the cap counts API spend, so this fixture must
                // set the API bill as well as the largest call.
                out.api_cost_usd = 0.10;
                out.effective_cost_usd = 0.10;
                // The full attempt's largest call raises the reserve past
                // the remaining headroom.
                out.max_call_usd = 0.15;
            }
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    let seen = seen.into_inner();
    assert!(
        !seen
            .iter()
            .any(|(id, v)| *id == 1 && *v == StageVariant::Lite),
        "the lite must be skipped: {seen:?}"
    );
    assert!(rollup.cap_reached, "the reserve stops the cell");
    assert_eq!(rollup.stages.len(), 2);
}

#[tokio::test]
async fn test_ladder_floor_stop_sets_cap_reached_not_over_budget() {
    // Round-11 finding 2: stopping with headroom below the floor is
    // "cap reached", not "over budget".
    let lc = ladder_fixture();
    let opts = CampaignRunOptions {
        campaign_id: "budget2".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 0.05,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let id = attempt.stage.id;
            let mut out = outcome(true, &[(id, true)]);
            out.effective_cost_usd = 0.04; // overshoots its tiny budget
            out.api_cost_usd = 0.012;
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    assert!(
        rollup.cap_reached && !rollup.over_budget,
        "a floor stop is not an overshoot: {rollup:?}"
    );
    assert_eq!(stop_reason(&rollup), "budget cap");
    assert_eq!(rollup.stages.len(), 1);
}

#[test]
fn test_git_commit_stage_never_tracks_feedback_report() {
    let dir = std::env::temp_dir().join(format!("sdb-git-d1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A legacy workspace: the report was tracked before this rule.
    std::fs::write(dir.join("FEEDBACK_REPORT.md"), "stale round 0").unwrap();
    std::fs::write(dir.join("main.go"), "package main").unwrap();
    git_commit_stage(&dir, 0, "genesis", "pass").unwrap();
    let tracked = || {
        String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["ls-files"])
                .current_dir(&dir)
                .output()
                .unwrap()
                .stdout,
        )
        .to_string()
    };
    assert!(tracked().contains("main.go"));
    assert!(
        !tracked().contains("FEEDBACK_REPORT.md"),
        "tracked: {}",
        tracked()
    );
    // The pipeline rewrites it for the next repair round; the next
    // commit must not pick it up either.
    std::fs::write(dir.join("FEEDBACK_REPORT.md"), "round 1").unwrap();
    git_commit_stage(&dir, 1, "next", "pass").unwrap();
    assert!(!tracked().contains("FEEDBACK_REPORT.md"));
    assert!(std::fs::read_to_string(dir.join(".gitignore"))
        .unwrap()
        .contains("FEEDBACK_REPORT.md"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_sql_acceptance_validation() {
    let mut lc = fixture_campaign();
    lc.spec.task = "sql".into();
    // Empty sql acceptance fails closed.
    assert!(validate_spec(&lc.spec, &lc.root).is_err());
    // The sdb-sql ladder is 0-based: id 0 is valid.
    lc.spec.stages[0].acceptance.sql_stages = vec![0];
    lc.spec.stages[1].acceptance.sql_stages = vec![0, 1];
    lc.spec.stages[2].acceptance.sql_stages = vec![0, 1, 2, 3];
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
    // 21 tickets, 0-20: id 20 is the last valid one, 21 is out.
    lc.spec.stages[2].acceptance.sql_stages = vec![0, 20];
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
    lc.spec.stages[2].acceptance.sql_stages = vec![0, 21];
    let err = validate_spec(&lc.spec, &lc.root).unwrap_err().to_string();
    assert!(err.contains("out of range 0..=20"), "{err}");
}

#[test]
fn test_proxy_acceptance_validation() {
    let mut lc = fixture_campaign();
    lc.spec.task = "proxy".into();
    // A proxy campaign whose proxy acceptance is empty fails closed
    // (the redis list is ignored for task 'proxy').
    assert!(validate_spec(&lc.spec, &lc.root).is_err());

    lc.spec.stages[0].acceptance.proxy_stages = vec![1];
    lc.spec.stages[1].acceptance.proxy_stages = vec![1, 2, 3];
    lc.spec.stages[2].acceptance.proxy_stages = vec![1, 2, 3, 4, 6, 7, 8, 9];
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());

    // The proxy suite tops out at stage 9: id 10 is out of range.
    lc.spec.stages[2].acceptance.proxy_stages = vec![1, 10];
    let err = validate_spec(&lc.spec, &lc.root).unwrap_err().to_string();
    assert!(err.contains("out of range 1..=9"), "{err}");
    lc.spec.stages[2].acceptance.proxy_stages = vec![1, 2, 3, 4, 6, 7, 8, 9];
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
}

#[test]
fn test_malformed_specs_fail_closed() {
    let lc = fixture_campaign();
    let good = serde_json::to_value(&lc.spec).unwrap();

    // Missing prompt file.
    std::fs::remove_file(lc.root.join("stages/1.md")).unwrap();
    assert!(validate_spec(&lc.spec, &lc.root).is_err());
    std::fs::write(lc.root.join("stages/1.md"), "x").unwrap();

    // Out-of-order ids fail (gaps are allowed: ENOSPC slot 4 can be
    // designed before it ships).
    let mut v = good.clone();
    v["stages"][1]["id"] = serde_json::json!(7);
    let bad: CampaignSpec = serde_json::from_value(v).unwrap();
    assert!(validate_spec(&bad, &lc.root).is_err());

    // A gap (0, 4, 5) is a valid ladder position.
    let mut v = good.clone();
    v["stages"][1]["id"] = serde_json::json!(4);
    v["stages"][2]["id"] = serde_json::json!(5);
    let gap: CampaignSpec = serde_json::from_value(v).unwrap();
    assert!(validate_spec(&gap, &lc.root).is_ok());

    // First id must be 0.
    let mut v = good.clone();
    v["stages"][0]["id"] = serde_json::json!(1);
    v["stages"][1]["id"] = serde_json::json!(2);
    v["stages"][2]["id"] = serde_json::json!(3);
    let bad: CampaignSpec = serde_json::from_value(v).unwrap();
    assert!(validate_spec(&bad, &lc.root).is_err());

    // Duplicate acceptance ids.
    let mut v = good.clone();
    v["stages"][0]["acceptance"]["redis_stages"] = serde_json::json!([1, 1]);
    let bad: CampaignSpec = serde_json::from_value(v).unwrap();
    assert!(validate_spec(&bad, &lc.root).is_err());

    // Out-of-range acceptance id (10-16 are valid stages now).
    let mut v = good.clone();
    v["stages"][0]["acceptance"]["redis_stages"] = serde_json::json!([17]);
    let bad: CampaignSpec = serde_json::from_value(v).unwrap();
    assert!(validate_spec(&bad, &lc.root).is_err());

    // Stages 10-16 are valid ids.
    let mut v = good.clone();
    v["stages"][0]["acceptance"]["redis_stages"] =
        serde_json::json!([1, 10, 11, 12, 13, 14, 15, 16]);
    let ok: CampaignSpec = serde_json::from_value(v).unwrap();
    assert!(validate_spec(&ok, &lc.root).is_ok());

    // Unsupported task.
    let mut v = good;
    v["task"] = serde_json::json!("http");
    let bad: CampaignSpec = serde_json::from_value(v).unwrap();
    assert!(validate_spec(&bad, &lc.root).is_err());
}

#[test]
fn test_ladder_validation_and_scoring() {
    let lc = fixture_campaign();
    let good = serde_json::to_value(&lc.spec).unwrap();

    // Tiers + needs + a lite variant validate.
    let mut v = good.clone();
    v["stages"][0]["tier"] = serde_json::json!(1);
    v["stages"][1]["tier"] = serde_json::json!(2);
    v["stages"][1]["needs"] = serde_json::json!([0]);
    v["stages"][2]["tier"] = serde_json::json!(3);
    v["stages"][2]["needs"] = serde_json::json!([0, 1]);
    v["stages"][2]["lite"] = serde_json::json!({
        "prompt": "stages/1.md",
        "profile": ["inner_join"],
        "budget_usd": 0.05,
        "max_turns": 8
    });
    let spec: CampaignSpec = serde_json::from_value(v.clone()).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_ok());
    assert_eq!(spec.stages[2].lite.as_ref().unwrap().weight, 0.4);

    // Tier 0 is invalid.
    let mut bad = v.clone();
    bad["stages"][0]["tier"] = serde_json::json!(0);
    let spec: CampaignSpec = serde_json::from_value(bad).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_err());

    // A need that names an unknown stage is invalid.
    let mut bad = v.clone();
    bad["stages"][1]["needs"] = serde_json::json!([7]);
    let spec: CampaignSpec = serde_json::from_value(bad).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_err());

    // A forward need (self or later) is invalid.
    let mut bad = v.clone();
    bad["stages"][1]["needs"] = serde_json::json!([1]);
    let spec: CampaignSpec = serde_json::from_value(bad).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_err());
    let mut bad = v.clone();
    bad["stages"][1]["needs"] = serde_json::json!([2]);
    let spec: CampaignSpec = serde_json::from_value(bad).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_err());

    // Lite invariants: missing prompt, bad weight, zero budget/turns.
    let mut bad = v.clone();
    bad["stages"][2]["lite"]["prompt"] = serde_json::json!("stages/missing.md");
    let spec: CampaignSpec = serde_json::from_value(bad).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_err());
    let mut bad = v.clone();
    bad["stages"][2]["lite"]["weight"] = serde_json::json!(1.5);
    let spec: CampaignSpec = serde_json::from_value(bad).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_err());
    let mut bad = v.clone();
    bad["stages"][2]["lite"]["budget_usd"] = serde_json::json!(0.0);
    let spec: CampaignSpec = serde_json::from_value(bad).unwrap();
    assert!(validate_spec(&spec, &lc.root).is_err());

    // Scoring: tier x (1.0 full | 0.4 lite), failed/skipped zero, and a
    // missing outcome counts as skipped.
    let spec: CampaignSpec = serde_json::from_value(v).unwrap();
    let score = score_ladder(
        &spec.stages,
        &[
            (0, LadderOutcome::Full),
            (1, LadderOutcome::Lite),
            (2, LadderOutcome::Failed),
        ],
    );
    assert_eq!(score.max_points, 6.0);
    assert!(
        (score.points - (1.0 + 2.0 * 0.4)).abs() < 1e-9,
        "{}",
        score.points
    );

    let score = score_ladder(&spec.stages, &[(0, LadderOutcome::Full)]);
    assert!((score.points - 1.0).abs() < 1e-9);
    assert_eq!(score.rows[1].2, LadderOutcome::Skipped);
}

#[test]
fn test_regression_detection() {
    let mut best = HashMap::new();
    best.insert(1, true);
    best.insert(2, true);
    best.insert(3, false);
    // Stage 1 regressed (was green), stage 3 was never green.
    let (ids, sev) = detect_regressions(&best, &[stage_result(1, false), stage_result(3, false)]);
    assert_eq!(ids, vec![1]);
    assert_eq!(sev, 3, "handshake regression is core severity");
    // Still-green and newly-green are not regressions.
    let (ids, _) = detect_regressions(&best, &[stage_result(1, true), stage_result(3, true)]);
    assert!(ids.is_empty());
    // A scenario-only stage that never ran because an earlier stage
    // failed is not a regression of that scenario (round-15 L1).
    let skipped = crate::verifier::StageResult {
        stage: 12,
        name: "Concurrency and isolation".into(),
        passed: false,
        error: Some("not run: an earlier stage failed".into()),
    };
    let mut best12 = HashMap::new();
    best12.insert(12, true);
    let (ids, sev) = detect_regressions(&best12, &[skipped]);
    assert!(ids.is_empty(), "{ids:?}");
    assert_eq!(sev, 0);
    // A differential stage skipped after an earlier failure is the same
    // (round-16 L-d).
    let skipped_differential = crate::verifier::StageResult {
        stage: 13,
        name: "Schema change".into(),
        passed: false,
        error: Some("skipped due to failure in earlier stages".into()),
    };
    let mut best13 = HashMap::new();
    best13.insert(13, true);
    let (ids, sev) = detect_regressions(&best13, &[skipped_differential]);
    assert!(ids.is_empty(), "{ids:?}");
    assert_eq!(sev, 0);
}

#[tokio::test]
async fn test_orchestrator_pass_stop_and_rollup() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "c1".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let mut calls = 0;
    let snaps = std::cell::RefCell::new(Vec::new());
    let cb = |r: &CampaignRollup| snaps.borrow_mut().push(r.stages.len());
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            calls += 1;
            let stage = attempt.stage;
            let out = if stage.id < 2 {
                outcome(true, &[(1, true), (2, true), (3, stage.id >= 1)])
            } else {
                outcome(false, &[(1, true), (2, true), (3, true), (5, false)])
            };
            async move { Ok(out) }
        },
        Some(&cb),
    )
    .await
    .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(
        *snaps.borrow(),
        vec![1, 2, 3],
        "progressive rollup fires after every ticket"
    );
    assert_eq!(
        rollup.reached,
        Some(1),
        "ladder stops at first failed ticket"
    );
    assert_eq!(rollup.regression_free_tickets, 2);
    assert!(!rollup.over_budget);
    assert!(rollup.tickets_per_dollar().unwrap() > 0.0);
}

/// A 5-stage continue-ladder fixture: stage 1 has a lite (weight 0.4),
/// stage 2 has no lite, stage 3 needs stage 2, stage 4 is independent.
fn ladder_fixture() -> LoadedCampaign {
    let dir = tmp_dir("ladder");
    let stages_dir = dir.join("stages");
    std::fs::create_dir_all(&stages_dir).unwrap();
    for i in 0..6 {
        std::fs::write(stages_dir.join(format!("{i}.md")), "# ticket").unwrap();
    }
    let st = |id: u32, tier: u32, needs: Vec<u32>, lite: Option<LiteSpec>| StageSpec {
        id,
        kind: "feature".into(),
        title: format!("Stage {id}"),
        prompt: format!("stages/{id}.md"),
        acceptance: Acceptance {
            redis_stages: vec![1],
            ..Default::default()
        },
        budget_usd: 0.1,
        max_turns: 5,
        feedback_rounds: 1,
        tier,
        needs,
        lite,
    };
    let spec = CampaignSpec {
        name: "ladder".into(),
        version: 1,
        task: "redis".into(),
        ladder: LadderMode::Continue,
        title: "ladder".into(),
        description: String::new(),
        stages: vec![
            st(0, 1, vec![], None),
            st(
                1,
                2,
                vec![0],
                Some(LiteSpec {
                    prompt: "stages/5.md".into(),
                    profile: vec!["inner_join".into()],
                    budget_usd: 0.05,
                    max_turns: 3,
                    weight: 0.4,
                }),
            ),
            st(2, 3, vec![0], None),
            st(3, 1, vec![2], None),
            st(4, 1, vec![], None),
        ],
        phases: Vec::new(),
    };
    std::fs::write(
        dir.join("campaign.json"),
        serde_json::to_string_pretty(&spec).unwrap(),
    )
    .unwrap();
    LoadedCampaign { spec, root: dir }
}

/// P1: the launcher argv carries the phase and chain.
#[test]
fn launcher_args_carry_the_phase_and_chain() {
    let a = launcher_args_with_phase(
        "m",
        "high",
        0.99,
        CELL_WALL_MIN,
        Some(2),
        Some("prev"),
        None,
        1,
    );
    assert!(a.windows(2).any(|w| w[0] == "--phase" && w[1] == "2"));
    assert!(a.windows(2).any(|w| w[0] == "--chain" && w[1] == "prev"));
    let b = launcher_args("m", "high", 0.99, CELL_WALL_MIN);
    assert!(!b.iter().any(|s| s == "--phase"));
    assert!(!b.iter().any(|s| s == "--chain"));
}

/// R23 D4: the wall is an eight-hour guard everywhere; the per-ticket
/// timeout guard is 120 minutes, and the cell script ships both.
#[test]
fn r23_d4_defaults_are_eight_hours() {
    assert_eq!(CELL_WALL_MIN, 480);
    assert_eq!(TICKET_WALL_GUARD_MIN, 120);
    let args = launcher_args("m", "high", CELL_BUDGET_USD, CELL_WALL_MIN);
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--wall-min" && w[1] == "480"));
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--timeout-min" && w[1] == "120"));
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/sql-cell.sh"),
    )
    .expect("the cell script");
    assert!(script.contains("WALL=\"${1:-480}\""), "{script}");
    assert!(script.contains("--timeout-min 120"), "{script}");
}

#[test]
fn shipped_career_phases_partition_and_score() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    validate_spec(&lc.spec, &lc.root).expect("phases validate");
    assert_eq!(lc.spec.phases.len(), 4);
    let sums: Vec<f64> = lc
        .spec
        .phases
        .iter()
        .map(|p| phase_max_points(p, &lc.spec.stages))
        .collect();
    assert_eq!(sums, vec![10.0, 12.0, 22.0, 32.0]);
    assert_eq!(sums.iter().sum::<f64>(), 76.0);
    for phase in &lc.spec.phases {
        for id in &phase.stages {
            assert!(phase_of(&lc.spec, *id).is_some(), "stage {id} has no phase");
        }
    }
    // Dropping a stage from a phase breaks the partition.
    let mut bad = lc.spec.clone();
    bad.phases[0].stages.pop();
    assert!(validate_spec(&bad, &lc.root).is_err());
}

fn chain_rollup(phase: u32, finished: bool) -> CampaignRollup {
    CampaignRollup {
        campaign_id: "prev".into(),
        name: "sdb-sql".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        task: "sql".into(),
        started_at: "2026-10-21T00:00:00Z".into(),
        finished_at: if finished {
            "2026-10-21T01:00:00Z".into()
        } else {
            String::new()
        },
        stages: Vec::new(),
        reached: Some(6),
        regression_free_tickets: 0,
        total_cost_usd: 0.5,
        billed_usd: 0.0,
        health: None,
        over_budget: false,
        cap_reached: false,
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        sqlite_version: None,
        cap_price_basis: None,
        sandbox_image: None,
        budget_usd: 0.99,
        wall_min: 150,
        worker: 0,
        concurrent_cells: 1,
        grading_wait_s: 0.0,
        points: 10.0,
        max_points: 10.0,
        phase: Some(phase),
        chain_from: None,
        career: Some(CareerRollup {
            points: 10.0,
            total_cost_usd: 0.5,
            cells: vec!["prev".into()],
            perf: None,
        }),
        perf: None,
        ladder_state: None,
        harness_fault: None,
        rehearsal: false,
        smoke: false,
        refork: None,
    }
}

/// S3: a phase cell's ladder lists all 21 tickets: earlier phases are
/// carried with their outcome, later ones are `later_phase`, and the
/// carried passes satisfy the cell's own `needs`.
#[test]
fn s3_phase_ladder_carries_earlier_and_marks_later_phases() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let mut rollup = chain_rollup(2, false);
    rollup.career = None;
    rollup.stages = vec![failed_record(7, "t7", "own stage: boom", "")];
    let carried = phase1_state();
    let e = phase_ladder(&lc.spec, &rollup, Some(8), Some(&carried));
    assert_eq!(e.len(), 21);
    assert_eq!(
        (e[0].status.as_str(), e[0].variant.as_str()),
        ("carried", "full")
    );
    assert_eq!(
        (e[4].status.as_str(), e[4].variant.as_str()),
        ("carried", "failed")
    );
    assert_eq!(e[7].status, "fail");
    assert_eq!(e[8].status, "running");
    // 9 needs 3, passed in phase 1: runnable, not "skipped: needs 3".
    assert_eq!(e[9].status, "not_reached");
    // 10 needs 7, which failed here.
    assert_eq!(e[10].status, "skipped");
    assert!(e[11..].iter().all(|x| x.status == "later_phase"));
    // A full-ladder cell keeps the plain ladder; 9 needs 3, which was
    // not attempted in this live cell, so it is waiting (F3), not
    // skipped. 10 keeps `skipped` because 7 actually failed.
    rollup.phase = None;
    let plain = phase_ladder(&lc.spec, &rollup, None, Some(&carried));
    assert_eq!(plain[9].status, "waiting");
    assert_eq!(plain[9].cause.as_deref(), Some("needs 3"));
    assert_eq!(plain[10].status, "skipped");
    assert!(plain.iter().all(|x| x.status != "later_phase"));
}

/// F3: an unmet need that is still pending shows `waiting` while the
/// cell runs, and only becomes `skipped` when the need actually fails.
#[test]
fn f3_phase_ladder_waits_for_a_pending_need() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let mut rollup = chain_rollup(3, false);
    rollup.career = None;
    let carried = phase1_state();
    // Ticket 14 needs 11; 11 is the running ticket and has no record.
    let e = phase_ladder(&lc.spec, &rollup, Some(11), Some(&carried));
    let t14 = e.iter().find(|x| x.id == 14).expect("ticket 14");
    assert_eq!(t14.status, "waiting", "{t14:?}");
    assert_eq!(t14.cause.as_deref(), Some("needs 11"));
    // R22-2: 11's full failed, but its lite is still pending, so 14
    // stays waiting rather than reading skipped for the whole lite.
    rollup
        .stages
        .push(failed_record(11, "Indexes", "own stage: boom", ""));
    let e = phase_ladder(&lc.spec, &rollup, None, Some(&carried));
    let t14 = e.iter().find(|x| x.id == 14).expect("ticket 14");
    assert_eq!(t14.status, "waiting", "{t14:?}");
    assert_eq!(t14.cause.as_deref(), Some("needs 11"));
    // Once the lite has also failed, 14 is decided: skipped.
    let mut lite = failed_record(11, "Indexes", "own stage: boom", "");
    lite.variant = "lite".to_string();
    rollup.stages.push(lite);
    let e = phase_ladder(&lc.spec, &rollup, None, Some(&carried));
    let t14 = e.iter().find(|x| x.id == 14).expect("ticket 14");
    assert_eq!(t14.status, "skipped", "{t14:?}");
    assert_eq!(t14.cause.as_deref(), Some("skipped: needs 11"));
}

/// R22-2 acceptance: a failed full whose lite is still pending leaves
/// its dependent waiting; a failed lite decides it as skipped.
#[test]
fn r22_2_a_full_failure_waits_for_the_lite() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let mut rollup = chain_rollup(3, false);
    rollup.career = None;
    let carried = phase1_state();
    // Ticket 14 needs 11; 11's full has failed and its lite is pending.
    rollup
        .stages
        .push(failed_record(11, "Indexes", "own stage: boom", ""));
    let e = phase_ladder(&lc.spec, &rollup, None, Some(&carried));
    let t14 = e.iter().find(|x| x.id == 14).expect("ticket 14");
    assert_eq!(t14.status, "waiting", "{t14:?}");
    assert_eq!(t14.cause.as_deref(), Some("needs 11"));
    // The lite record exists and failed: the need is decided.
    let mut lite = failed_record(11, "Indexes", "own stage: boom", "");
    lite.variant = "lite".to_string();
    rollup.stages.push(lite);
    let e = phase_ladder(&lc.spec, &rollup, None, Some(&carried));
    let t14 = e.iter().find(|x| x.id == 14).expect("ticket 14");
    assert_eq!(t14.status, "skipped", "{t14:?}");
    assert_eq!(t14.cause.as_deref(), Some("skipped: needs 11"));
}

fn phase1_state() -> LadderState {
    let mut st = LadderState::default();
    for id in [0, 1, 2, 3, 5, 6] {
        st.passed.push(id);
        st.outcomes.insert(id, LadderOutcome::Full);
        st.best.insert(id, true);
    }
    st.outcomes.insert(4, LadderOutcome::Failed);
    st.best.insert(4, false);
    st.dirty = true;
    st.last_green = Some("abc1234".into());
    st
}

/// R45 F2: a phase rollup and its career rollup carry the perf block,
/// and the perf run never touches the candidate's spend.
#[test]
fn r45_f2_rollup_carries_perf() {
    use crate::verifier::sql::perfscore::{PerfScore, WorkloadResult};
    let mut rollup = chain_rollup(1, true);
    let cost = rollup.total_cost_usd;
    let perf = PerfScore::from_workloads(vec![WorkloadResult {
        id: "W1".into(),
        metric: "latency".into(),
        reference: 2.0,
        candidate: 1.0,
        ratio: 2.0,
        correct: true,
        numbers: Default::default(),
        note: None,
        harness_fault: false,
    }]);
    rollup.set_perf(perf.clone());
    assert_eq!(rollup.perf.as_ref().and_then(|p| p.score), perf.score);
    assert_eq!(
        rollup
            .career
            .as_ref()
            .and_then(|c| c.perf.as_ref())
            .and_then(|p| p.score),
        perf.score
    );
    assert_eq!(rollup.total_cost_usd, cost, "perf must not spend");
    let json = serde_json::to_value(&rollup).unwrap();
    assert!(json["perf"]["score"].is_number());
    assert!(json["career"]["perf"]["score"].is_number());
    let back: CampaignRollup = serde_json::from_value(json).unwrap();
    assert_eq!(back.perf.and_then(|p| p.score), perf.score);
}

/// R43 F1: the candidate-visible view (the 21 rendered SPEC.md files
/// plus the sandbox policy, the single-run SQL task prompt and the
/// agent prompt files) is byte-identical to 2cbea35. The expected
/// SHA-256 values were recorded by running this same renderer on
/// 2cbea35 (the pre-deletion master), where the raw files and
/// `render_spec_md` were unchanged; the deletion round does not touch
/// them.
#[test]
fn r43_f1_candidate_view_unchanged() {
    use sha2::{Digest, Sha256};
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let mut outcomes: Vec<(u32, LadderOutcome)> = Vec::new();
    let mut actual: Vec<String> = Vec::new();
    for stage in &lc.spec.stages {
        let text = lc
            .render_spec_md(stage, &lc.prompt_path(stage), &outcomes)
            .expect("the SPEC renders");
        actual.push(format!("{:x}", Sha256::digest(text.as_bytes())));
        outcomes.push((stage.id, LadderOutcome::Full));
    }
    let root = crate::config::get_repo_root();
    for rel in ["extensions/bench-policy.js", "tasks/sql/prompt.md"] {
        let bytes = std::fs::read(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        actual.push(format!("{:x}", Sha256::digest(&bytes)));
    }
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("src/prompts"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let mut h = Sha256::new();
    for p in files {
        h.update(p.file_name().unwrap().to_string_lossy().as_bytes());
        h.update([0]);
        h.update(std::fs::read(&p).unwrap());
    }
    actual.push(format!("{:x}", h.finalize()));
    let expected: Vec<&str> = vec![
        "2aa4b2071ebbf7964fe3473c088614d1e55d0949bb6c3adab8c9f97a07ef0130",
        "dfeab661f07a21de492cf230b21abbd9a8f13237a08691148e39378264aaa319",
        "fc0b7a86addb8b044241e4190f13e1e00fa714122c3d2c44bfc7614e05e36082",
        "599d0b2604dff20b0ab9a8c1894ab7979d9708c2dcfcc87aba51d0628fad3777",
        "af59b17d0bfd1b8cb203f7eec211f772ba8e89f4d61253da2ab1cd6b2640ad79",
        "0af5ef606946bd71f61c79c1200cb62e6fd4db3b2d31a4ed62e9f94bcee04a35",
        "0e54e193ae7c44276e1b9aa5ed19698c5000efb89e86720e5699dd8bb6f0715d",
        "b98365478c0d41d9924f92adcd2a40c68b03852ece368cbce8963341e7260975",
        "28bdd84e7433e877cda1a3600fe7b099bfe10956f1f2ef4443e0fa18725c4eee",
        "fb55d4ca84ed8eb9f50a555c03f8e016a5226bda98188551ac3721577fe1edb5",
        "370396b5004db43974ca4a1ab32d27e8f347e4aa867d771eca57654fce8e3b52",
        "af02bfa6591d19be7f92616a6dca9c8618909b5c63995d0ae2d19471fa40bc90",
        "455ddb2c7ee798f84366e61cda273c0d1f479fa0158b03fa461efa8f6285ebd1",
        "9247f706f0e31a401cdff64fd47224d5408471b4d3b09dc5767afd1ee52e098d",
        "81a888d76bb5e8020e3553511e584677cf2d7b17621ecedb21c9f3ee4ef231e3",
        "25d7a04238f15039353307162b8e8019ff908634cc6fa4670eea642fc67d2f8c",
        "b4b9624a4c743a8b9511f0cdb1c68b1d29d637450d7a611a8739f8a40e3eddca",
        "fa97240be1b7c29bbbfb15e851beccd8a34498cdc0a97765c0afdc5061e150d8",
        "701a8032c1916d1f7060fb220bcf1c48389422636fa65e2ee7a07244d6e198ac",
        "bebd820d7353a19e64c8be6f609d8ae5561ccc3a3e82d469182e0d3a54fc036b",
        "f1b075923b284348f7150bb467c1cd7fb8b2c98a8aaa0fef0c7f615d73d5f021",
        "f00669b1c73447c62d748a27c9a92c8ab01be0d5b63fceda23292fc6a01b59b6",
        "7a5f2c36a2b44a27f0bb892519f89cd98750e1fb11dc3c7ff4fb7398fad2f567",
        "2b3c28db6e2f7250fdbea58271659407da102675c9d8448f0f86491030484b04",
    ];
    let actual_refs: Vec<&str> = actual.iter().map(String::as_str).collect();
    assert_eq!(
        actual_refs, expected,
        "candidate view changed; actual {actual:?}"
    );
}

/// R36-3/R51 F7: ten Board requests for a live cell run the image
/// lookup at most once. The cell's status image and a finished ticket's
/// manifest are free; only a cell with neither calls the injected
/// fallback, through a local cache. No process-wide state.
#[test]
fn r43_f3_board_no_inspect_per_request() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let runs = tmp_dir("stubimage");
    let inspects = AtomicUsize::new(0);
    let fallback = || {
        inspects.fetch_add(1, Ordering::Relaxed);
        Some("sha256:cached-image".to_string())
    };

    // The cell's own status carries the run's image.
    let v = serde_json::json!({"live": {"sandbox_image": "sha256:run-image"}});
    for _ in 0..10 {
        let stub = live_rollup_stub_at_with("campaign_stub_status", &v, &runs, &fallback);
        assert_eq!(stub.sandbox_image.as_deref(), Some("sha256:run-image"));
    }
    assert_eq!(
        inspects.load(Ordering::Relaxed),
        0,
        "status image must not inspect"
    );

    // A finished ticket's manifest carries it when the status predates
    // the field.
    let cell = runs.join("campaign_stub_manifest");
    std::fs::create_dir_all(cell.join("s0_full")).unwrap();
    std::fs::write(
        cell.join("s0_full/manifest.json"),
        serde_json::json!({"sandbox_image": "sha256:manifest-image"}).to_string(),
    )
    .unwrap();
    for _ in 0..10 {
        let stub = live_rollup_stub_at_with(
            "campaign_stub_manifest",
            &serde_json::json!({}),
            &runs,
            &fallback,
        );
        assert_eq!(stub.sandbox_image.as_deref(), Some("sha256:manifest-image"));
    }
    assert_eq!(
        inspects.load(Ordering::Relaxed),
        0,
        "manifest image must not inspect"
    );

    // Neither source: one local cache fill serves all ten requests.
    let cache = ImageCache::new();
    let calls = AtomicUsize::new(0);
    let lookup = || {
        calls.fetch_add(1, Ordering::Relaxed);
        Some("sha256:slow".to_string())
    };
    let cached = || cache.resolve(&lookup);
    for _ in 0..10 {
        let stub = live_rollup_stub_at_with(
            "campaign_stub_none",
            &serde_json::json!({"live": {}}),
            &runs,
            &cached,
        );
        assert_eq!(stub.sandbox_image.as_deref(), Some("sha256:slow"));
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1, "ten requests, one lookup");
    let _ = std::fs::remove_dir_all(&runs);
}

/// R51 F7: the cache mutex is not held across the lookup, so two threads
/// on an empty cache inspect concurrently; the value is kept after.
#[test]
fn r51_f7_cache_lock_not_held_across_inspect() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache = ImageCache::new();
    let calls = AtomicUsize::new(0);
    let lookup = || {
        calls.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(250));
        Some("sha256:slow".to_string())
    };
    let start = std::time::Instant::now();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        let a = s.spawn(|| {
            barrier.wait();
            cache.resolve(&lookup)
        });
        let b = s.spawn(|| {
            barrier.wait();
            cache.resolve(&lookup)
        });
        assert_eq!(a.join().unwrap().as_deref(), Some("sha256:slow"));
        assert_eq!(b.join().unwrap().as_deref(), Some("sha256:slow"));
    });
    let elapsed = start.elapsed();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "both threads must miss the empty cache; a held lock serializes them"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(450),
        "the two lookups serialized: {elapsed:?}"
    );
    let before = calls.load(Ordering::Relaxed);
    assert_eq!(cache.resolve(&lookup).as_deref(), Some("sha256:slow"));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        before,
        "the cache did not keep the value"
    );
}

/// R49 F4 (R45-3/R45-6): a scored phase 2 followed by a harness-fault
/// phase 3 keeps phase 2's score on the career; phase 3 keeps its own
/// scoreless block.
#[test]
fn r49_f4_career_keeps_last_scored_perf() {
    use crate::verifier::sql::perfscore::{PerfScore, WorkloadResult};
    let mut rollup = chain_rollup(1, true);
    let scored = PerfScore::from_workloads(vec![WorkloadResult {
        id: "W1".into(),
        metric: "latency".into(),
        reference: 2.0,
        candidate: 1.0,
        ratio: 2.0,
        correct: true,
        numbers: Default::default(),
        note: None,
        harness_fault: false,
    }]);
    rollup.set_perf(scored.clone());
    assert_eq!(
        rollup
            .career
            .as_ref()
            .and_then(|c| c.perf.as_ref())
            .and_then(|p| p.score),
        Some(2.0)
    );
    let fault = PerfScore::harness_fault(3, "the reference died");
    rollup.set_perf(fault.clone());
    assert!(
        rollup.perf.as_ref().unwrap().score.is_none(),
        "the phase keeps its scoreless block"
    );
    assert_eq!(
        rollup
            .career
            .as_ref()
            .and_then(|c| c.perf.as_ref())
            .and_then(|p| p.score),
        Some(2.0),
        "the career keeps the last scored phase"
    );
}

/// R48 F2 (D2 a): the grading-lock wait is recorded in the rollup and
/// subtracted from the wall cap.
#[test]
fn r48_f2_wait_is_recorded_not_charged() {
    use std::time::Duration;
    assert!(wall_exceeded(Duration::from_secs(300), Some(5), 0.0));
    assert!(!wall_exceeded(Duration::from_secs(300), Some(5), 120.0));
    assert!(!wall_exceeded(Duration::from_secs(300), Some(5), 300.0));
    assert!(!wall_exceeded(Duration::from_secs(200), Some(5), 0.0));
    let mut rollup = chain_rollup(1, true);
    rollup.grading_wait_s = 120.0;
    let v = serde_json::to_value(&rollup).unwrap();
    assert_eq!(v["grading_wait_s"].as_f64(), Some(120.0));
}

/// R48 F5: a phase rollup says its lane, the lane count and the lock
/// wait it carried.
#[test]
fn r48_f5_rollup_says_how_it_ran() {
    let mut rollup = chain_rollup(1, true);
    rollup.worker = 3;
    rollup.concurrent_cells = 3;
    rollup.grading_wait_s = 42.5;
    let v = serde_json::to_value(&rollup).unwrap();
    assert_eq!(v["worker"], 3);
    assert_eq!(v["concurrent_cells"], 3);
    assert_eq!(v["grading_wait_s"].as_f64(), Some(42.5));
    let back: CampaignRollup = serde_json::from_value(v).unwrap();
    assert_eq!(back.worker, 3);
    assert_eq!(back.concurrent_cells, 3);
    assert_eq!(back.grading_wait_s, 42.5);
}

/// R45 F3: the rendered ticket-0 contract names every workload, the
/// sandbox limits and the formula; the later phase-openers repeat what
/// that phase adds.
#[test]
fn r45_f3_prompt_names_the_perf_score() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let stage = lc.spec.stages.iter().find(|s| s.id == 0).unwrap();
    let text = lc
        .render_spec_md(stage, &lc.prompt_path(stage), &[])
        .expect("the ticket-0 SPEC renders");
    for needle in [
        "## Performance",
        "W1",
        "W2",
        "W3",
        "W4",
        "W5",
        "2 CPUs",
        "2 GiB",
        "geometric mean",
        "[0.1, 100]",
        "wrong answer",
        "flush",
        "Phase 1 measures W1 and W4",
    ] {
        assert!(text.contains(needle), "ticket 0 missing {needle:?}");
    }
    for (id, needle) in [
        (7, "adds **W3**"),
        (11, "adds **W2**"),
        (16, "all five workloads"),
    ] {
        let st = lc.spec.stages.iter().find(|s| s.id == id).unwrap();
        let t = std::fs::read_to_string(lc.prompt_path(st)).unwrap();
        assert!(t.contains(needle), "ticket {id} missing {needle:?}");
    }
}

/// R45 F4 (kept current by round 58): a `.43` phase never chains into
/// the current rules; a same-version phase does.
#[test]
fn r45_f4_no_chain_across_the_bump() {
    assert_eq!(crate::report::rules::RULES_VERSION, "2026-10-19.45");
    let state = phase1_state();
    let mut prev = chain_rollup(1, true);
    prev.rules_version = "2026-10-19.43".to_string();
    let err = validate_chain(
        &prev,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ran rules 2026-10-19.43"), "{msg}");
    assert!(msg.contains("not 2026-10-19.45"), "{msg}");
    validate_chain(
        &chain_rollup(1, true),
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .expect("a current-version phase chains");
}

/// R49 F5: a `.44` phase never chains into the current `.45` rules; a
/// current phase chains normally.
#[test]
fn r49_f5_no_chain_across_the_bump() {
    assert_eq!(crate::report::rules::RULES_VERSION, "2026-10-19.45");
    let state = phase1_state();
    let mut prev = chain_rollup(1, true);
    prev.rules_version = "2026-10-19.44".to_string();
    let err = validate_chain(
        &prev,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ran rules 2026-10-19.44"), "{msg}");
    assert!(msg.contains("not 2026-10-19.45"), "{msg}");
    validate_chain(
        &chain_rollup(1, true),
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .expect("a .44 phase chains onto a .44 phase");
}

#[test]
fn validate_chain_refuses_wrong_phase_model_or_unfinished_cells() {
    let state = phase1_state();
    let prev = chain_rollup(1, true);
    validate_chain(
        &prev,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .expect("the matching chain validates");
    let err = validate_chain(
        &chain_rollup(0, true),
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("expected phase 1"), "{err}");
    let err = validate_chain(
        &prev,
        Some(&state),
        2,
        "other",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("used model"), "{err}");
    let err = validate_chain(
        &chain_rollup(1, false),
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("did not finish"), "{err}");
    let err = validate_chain(
        &prev,
        None,
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no ladder state"), "{err}");
    let mut faulted = chain_rollup(1, true);
    faulted.harness_fault = Some("provider billing".into());
    let err = validate_chain(
        &faulted,
        Some(&state),
        2,
        "m",
        "omp",
        "high",
        crate::report::rules::RULES_VERSION,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("non-result"), "{err}");
}

#[tokio::test]
async fn a_phase_filters_its_stages_and_skips_needs_from_the_chain() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let opts = CampaignRunOptions {
        campaign_id: "phase3".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let state = phase1_state();
    let seen = std::cell::RefCell::new(Vec::<u32>::new());
    let rollup = run_campaign_phase(
        &lc,
        &opts,
        Some(phase),
        Some(&state),
        Some("prev"),
        Some(&CareerRollup {
            points: 10.0,
            total_cost_usd: 0.5,
            cells: vec!["prev".into()],
            perf: None,
        }),
        |attempt| {
            seen.borrow_mut().push(attempt.stage.id);
            let id = attempt.stage.id;
            async move { Ok(outcome(false, &[(id, false)])) }
        },
        None,
    )
    .await
    .unwrap();
    let seen = seen.into_inner();
    assert!(!seen.is_empty(), "stage 12 must run");
    assert!(
        seen.iter().all(|id| *id == 4 || *id == 12),
        "only the catch-up 4 and stage 12 are runnable: {seen:?}"
    );
    assert_eq!(seen.first().copied(), Some(4), "the catch-up runs first");
    assert_eq!(rollup.phase, Some(3));
    assert_eq!(rollup.chain_from.as_deref(), Some("prev"));
    // Q9: the shown ceiling includes the attempted catch-up's 0.4 x
    // tier, so it is above the phase's own 22 points.
    assert!(rollup.max_points > 22.0, "Q9 ceiling {}", rollup.max_points);
    assert!((rollup.points - 0.0).abs() < 1e-9);
    assert_eq!(
        rollup.career.as_ref().unwrap().cells,
        vec!["prev", "phase3"]
    );
    assert!((rollup.career.as_ref().unwrap().points - 10.0).abs() < 1e-9);
    for id in [11u32, 13, 14, 15] {
        let r = rollup.stages.iter().find(|r| r.id == id).expect("record");
        assert!(!r.passed);
        assert!((r.effective_cost_usd - 0.0).abs() < 1e-9, "stage {id} cost");
        assert_eq!(r.status, "skipped", "stage {id}");
    }
    assert!(
        rollup.stages.iter().all(|r| !(7..=10).contains(&r.id)),
        "phase 2 must stay untouched: {:?}",
        rollup.stages.iter().map(|r| r.id).collect::<Vec<_>>()
    );
}

fn failed_record(id: u32, title: &str, line: &str, detail: &str) -> StageRecord {
    StageRecord {
        id,
        title: title.into(),
        kind: "relational".into(),
        passed: false,
        pass_rate: 0.0,
        passed_stages: 0,
        total_stages: 1,
        regressed: Vec::new(),
        regression_severity: 0,
        api_cost_usd: 0.1,
        billed_usd: 0.0,
        effective_cost_usd: 0.1,
        turns: Some(10),
        duration_seconds: Some(60.0),
        status: "failed".into(),
        stop_reason: None,
        variant: "full".into(),
        tier: 3,
        points: 0.0,
        run_id: "r".into(),
        git_commit: None,
        git_stat: None,
        rounds: Vec::new(),
        failure: Some(FailureSummary {
            kind: "own_stage".into(),
            stage: Some(id),
            line: line.into(),
            detail: detail.into(),
            stderr_tail: None,
        }),
        grading_wait_s: 0.0,
    }
}

fn phase_state_with_failed(ids: &[u32]) -> LadderState {
    let mut st = LadderState::default();
    for id in 0..=20 {
        if ids.contains(&id) {
            st.outcomes.insert(id, LadderOutcome::Failed);
            st.best.insert(id, false);
        } else {
            st.passed.push(id);
            st.outcomes.insert(id, LadderOutcome::Full);
            st.best.insert(id, true);
        }
    }
    st.dirty = true;
    st.last_green = Some("abc1234".into());
    st
}

#[test]
fn phase_hints_name_blocked_tickets_and_include_the_repro() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let state = phase_state_with_failed(&[8]);
    let mut prev = chain_rollup(2, true);
    prev.stages.push(failed_record(
        8,
        "Joins",
        "LEFT JOIN result had the wrong row count",
        "seed 42: SELECT ... LEFT JOIN ... expected 3 rows, got 4",
    ));
    let hint = phase_hints(&prev, &lc.spec, phase, &state).expect("hint block");
    assert!(hint.contains("Ticket 8"), "{hint}");
    assert!(hint.contains("LEFT JOIN result"), "{hint}");
    assert!(hint.contains("expected 3 rows"), "{hint}");
    assert!(hint.contains("ticket 15"), "{hint}");
    assert!(hint.contains("SPEC.md section"), "{hint}");
    assert!(hint.contains("pts"), "{hint}");
}

#[test]
fn catch_up_targets_order_by_unblocked_points_then_id() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 4).unwrap();
    // Q2: 11 needs 4, so with 4 failed, 11 is not an eligible target.
    let state = phase_state_with_failed(&[4, 8, 11]);
    let outcomes: HashMap<u32, LadderOutcome> =
        state.outcomes.iter().map(|(k, v)| (*k, *v)).collect();
    let passed: std::collections::BTreeSet<u32> = state.passed.iter().copied().collect();
    let targets = catch_up_targets(&lc.spec, phase, &outcomes, &passed);
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert_eq!(targets[0].0, 4, "{targets:?}");
    assert_eq!(targets[1].0, 8, "{targets:?}");

    // With 4 passed, 2/8/11 are eligible; 4 blocks the whole phase-4
    // closure (32 pts), 8 blocks t16/t18, 11 blocks t16.
    let state = phase_state_with_failed(&[2, 8, 11]);
    let outcomes: HashMap<u32, LadderOutcome> =
        state.outcomes.iter().map(|(k, v)| (*k, *v)).collect();
    let passed: std::collections::BTreeSet<u32> = state.passed.iter().copied().collect();
    let targets = catch_up_targets(&lc.spec, phase, &outcomes, &passed);
    assert_eq!(targets.len(), 3);
    assert_eq!(targets[0].0, 8, "{targets:?}");
    assert_eq!(targets[1].0, 2, "{targets:?}");
    assert_eq!(targets[2].0, 11, "{targets:?}");
    assert!(targets[0].1 >= targets[1].1 && targets[1].1 >= targets[2].1);
    assert_eq!(targets.iter().map(|(id, _)| *id).collect::<Vec<_>>(), {
        let mut ids: Vec<u32> = targets.iter().map(|(id, _)| *id).collect();
        ids.sort_by(|a, b| {
            let pa = targets.iter().find(|(i, _)| i == a).unwrap().1;
            let pb = targets.iter().find(|(i, _)| i == b).unwrap().1;
            pb.partial_cmp(&pa).unwrap().then(a.cmp(b))
        });
        ids
    });
}

#[tokio::test]
async fn a_catch_up_scores_four_tenths_and_unblocks_the_phase() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let mut state = phase_state_with_failed(&[8]);
    state.hints = Some("HINT BLOCK".into());
    let opts = CampaignRunOptions {
        campaign_id: "catchup".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<(u32, Option<StageVariant>, Option<String>)>::new());
    let rollup = run_campaign_phase(
        &lc,
        &opts,
        Some(phase),
        Some(&state),
        Some("prev"),
        None,
        |attempt| {
            seen.borrow_mut().push((
                attempt.stage.id,
                Some(attempt.variant),
                attempt.hints.clone(),
            ));
            let id = attempt.stage.id;
            async move { Ok(outcome(true, &[(id, true)])) }
        },
        None,
    )
    .await
    .unwrap();
    let seen = seen.into_inner();
    assert_eq!(seen[0].0, 8, "the catch-up runs first: {seen:?}");
    assert_eq!(seen[0].1, Some(StageVariant::Lite));
    assert_eq!(seen[0].2.as_deref(), Some("HINT BLOCK"));
    assert!(
        seen.iter().any(|(id, _, _)| *id == 15),
        "t15 unblocked: {seen:?}"
    );
    let rec = rollup
        .stages
        .iter()
        .find(|r| r.id == 8)
        .expect("catch-up record");
    assert_eq!(rec.status, "catch-up");
    let tier = lc.spec.stages.iter().find(|s| s.id == 8).unwrap().tier as f64;
    assert!((rec.points - tier * 0.4).abs() < 1e-9, "{}", rec.points);
    assert!(rollup.stages.iter().any(|r| r.id == 15 && r.passed));
}

#[tokio::test]
async fn a_catch_up_is_capped_at_two_and_skipped_without_headroom() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 4).unwrap();
    let state = phase_state_with_failed(&[2, 4, 8, 11]);
    let opts = CampaignRunOptions {
        campaign_id: "catchup2".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let rollup = run_campaign_phase(
        &lc,
        &opts,
        Some(phase),
        Some(&state),
        Some("prev"),
        None,
        |attempt| {
            let id = attempt.stage.id;
            async move { Ok(outcome(false, &[(id, false)])) }
        },
        None,
    )
    .await
    .unwrap();
    let catch_ups = rollup
        .stages
        .iter()
        .filter(|r| r.status == "catch-up")
        .count();
    assert_eq!(
        catch_ups, CATCH_UP_SLOTS,
        "at most {CATCH_UP_SLOTS} catch-ups"
    );

    // A $0.03 cap cannot fit the $0.02 reserve plus $0.02 floor, so the
    // catch-up (and the phase) never starts.
    let opts = CampaignRunOptions {
        campaign_id: "catchup3".into(),
        ..opts
    };
    let tight = CampaignRunOptions {
        budget_usd: 0.03,
        ..opts
    };
    let seen = std::cell::RefCell::new(Vec::<u32>::new());
    let rollup = run_campaign_phase(
        &lc,
        &tight,
        Some(phase),
        Some(&state),
        Some("prev"),
        None,
        |attempt| {
            seen.borrow_mut().push(attempt.stage.id);
            let id = attempt.stage.id;
            async move { Ok(outcome(true, &[(id, true)])) }
        },
        None,
    )
    .await
    .unwrap();
    assert!(seen.into_inner().is_empty(), "no headroom, no catch-up");
    assert!(rollup.cap_reached);
}

#[tokio::test]
async fn q1_a_skipped_4_5_6_chain_uses_three_slots() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let mut state = phase_state_with_failed(&[4]);
    for id in [5u32, 6u32] {
        state.outcomes.insert(id, LadderOutcome::Skipped);
        state.best.insert(id, false);
        state.passed.retain(|x| *x != id);
    }
    let opts = CampaignRunOptions {
        campaign_id: "q1chain".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<u32>::new());
    let rollup = run_campaign_phase(
        &lc,
        &opts,
        Some(phase),
        Some(&state),
        Some("prev"),
        None,
        |attempt| {
            seen.borrow_mut().push(attempt.stage.id);
            let id = attempt.stage.id;
            async move { Ok(outcome(true, &[(id, true)])) }
        },
        None,
    )
    .await
    .unwrap();
    let seen = seen.into_inner();
    assert_eq!(&seen[..3], &[4, 5, 6], "greedy 4 -> 5 -> 6: {seen:?}");
    assert!(seen.contains(&12) && seen.contains(&14), "{seen:?}");
    for id in [12u32, 14u32] {
        let r = rollup.stages.iter().find(|r| r.id == id).expect("record");
        assert!(r.passed, "ticket {id} unblocked");
    }
    assert!(
        rollup.points <= rollup.max_points + 1e-9,
        "Q9 points {} > max {}",
        rollup.points,
        rollup.max_points
    );
}

#[tokio::test]
async fn q3_a_catch_up_harness_fault_stops_the_phase() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let state = phase_state_with_failed(&[4]);
    let opts = CampaignRunOptions {
        campaign_id: "q3fault".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<u32>::new());
    let rollup = run_campaign_phase(
        &lc,
        &opts,
        Some(phase),
        Some(&state),
        Some("prev"),
        None,
        |attempt| {
            seen.borrow_mut().push(attempt.stage.id);
            let mut out = outcome(false, &[(attempt.stage.id, false)]);
            out.harness_fault = Some("never reached first turn".into());
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(seen.into_inner(), vec![4], "no main-loop attempts");
    let fault = rollup.harness_fault.expect("a harness fault");
    assert!(fault.contains("ticket 4"), "{fault}");
    assert!(fault.contains("never reached"), "{fault}");
}

/// F2: a graded catch-up crash is a candidate failure, not a cell fault.
#[tokio::test]
async fn f2_a_graded_catch_up_crash_does_not_stop_the_cell() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let state = phase_state_with_failed(&[4]);
    let opts = CampaignRunOptions {
        campaign_id: "f2catchup".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<u32>::new());
    let rollup = run_campaign_phase(
        &lc,
        &opts,
        Some(phase),
        Some(&state),
        Some("prev"),
        None,
        |attempt| {
            seen.borrow_mut().push(attempt.stage.id);
            let mut out = outcome(false, &[(attempt.stage.id, false)]);
            out.agent_failed = true;
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    let seen = seen.into_inner();
    assert_eq!(seen.first(), Some(&4), "ticket 4 is the catch-up: {seen:?}");
    assert!(
        rollup.harness_fault.is_none(),
        "a graded catch-up crash is not a harness fault"
    );
    let rec = rollup
        .stages
        .iter()
        .find(|r| r.id == 4 && r.status == "catch-up")
        .expect("the catch-up record");
    assert!(!rec.passed);
}

#[test]
fn f2_mark_agent_crash_records_the_output_limit() {
    let mut failure = FailureSummary {
        kind: "agent".into(),
        stage: Some(10),
        line: "agent stopped without READY".into(),
        detail: String::new(),
        stderr_tail: None,
    };
    assert!(!mark_agent_crash(&mut failure, "ordinary console"));
    assert_eq!(failure.kind, "agent");
    assert!(mark_agent_crash(
        &mut failure,
        "[TOKENS] Turn usage - input: 1, output: 65536, cached: 0\n\
             [STDERR] 400 Provider returned error"
    ));
    assert_eq!(failure.kind, "agent_crash");
    assert!(failure.line.contains("65,536"), "{}", failure.line);
}

/// R22-1 acceptance: a recovered JSON error plus a later unrelated
/// crash is not an output-limit crash.
#[test]
fn r22_1_a_recovered_json_error_is_not_an_agent_crash() {
    let mut failure = FailureSummary {
        kind: "agent".into(),
        stage: Some(10),
        line: "agent stopped without READY".into(),
        detail: String::new(),
        stderr_tail: None,
    };
    let console = "\
[LIVE] Agent initialized. Max turns: 24, Budget: $0.15
[TOKENS] Turn usage - input: 2, output: 128, cached: 0
[RESULT] edit (OK)
  > Validation failed for tool \"edit\": Tool call arguments are not valid JSON.
  > Parse Error: Unterminated string
[TOKENS] Turn usage - input: 2, output: 256, cached: 0
[STDERR] 500 Provider returned error
[OMP ERROR] Agent execution failed
";
    assert!(!mark_agent_crash(&mut failure, console));
    assert_eq!(failure.kind, "agent");
    assert_eq!(failure.line, "agent stopped without READY");
}

/// R25-1 acceptance: a session whose largest turn is followed by more
/// turns and then a 400 is not an output-limit crash.
#[test]
fn r25_1_a_400_after_another_turn_is_not_an_output_limit_crash() {
    let mut failure = FailureSummary {
        kind: "agent".into(),
        stage: Some(10),
        line: "agent stopped without READY".into(),
        detail: String::new(),
        stderr_tail: None,
    };
    let console = "\
[LIVE] Agent initialized. Max turns: 24, Budget: $0.15
[TOKENS] Turn usage - input: 2, output: 32768, cached: 0
[LIVE] Turn 2/24 | Spent: $0.10 | Tokens: 40000
[TOKENS] Turn usage - input: 2, output: 512, cached: 0
[STDERR] 400 Provider returned error
[OMP ERROR] Agent execution failed
";
    assert!(!mark_agent_crash(&mut failure, console));
    assert_eq!(failure.kind, "agent");
    assert_eq!(failure.line, "agent stopped without READY");
}

/// R22-1 acceptance: a 32,768-token truncation with its provider 400
/// reports 32,768, not the old hard-coded 65,536.
#[test]
fn r22_1_a_32_768_token_truncation_reports_the_real_number() {
    let mut failure = FailureSummary {
        kind: "agent".into(),
        stage: Some(10),
        line: "agent stopped without READY".into(),
        detail: String::new(),
        stderr_tail: None,
    };
    let console = "\
[LIVE] Agent initialized. Max turns: 24, Budget: $0.15
[TOKENS] Turn usage - input: 2, output: 32768, cached: 0
[STDERR] 400 Provider returned error
[OMP ERROR] Agent execution failed
";
    assert!(mark_agent_crash(&mut failure, console));
    assert_eq!(failure.kind, "agent_crash");
    assert!(failure.line.contains("32,768"), "{}", failure.line);
    assert!(!failure.line.contains("65,536"), "{}", failure.line);
}

#[test]
fn q7_hints_use_the_carried_last_failure() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 3).unwrap();
    let mut state = phase_state_with_failed(&[8]);
    state.last_failures.insert(
        8,
        FailureSummary {
            kind: "own_stage".into(),
            stage: Some(8),
            line: "old joins failure".into(),
            detail: "old seed 7 repro".into(),
            stderr_tail: None,
        },
    );
    let prev = chain_rollup(2, true); // no stage-8 record
    let hint = phase_hints(&prev, &lc.spec, phase, &state).expect("hint");
    assert!(hint.contains("old joins failure"), "{hint}");
    assert!(hint.contains("old seed 7 repro"), "{hint}");
    assert!(hint.contains("# Ticket 8"), "{hint}");
}

#[test]
fn s2_a_superseded_branch_is_used_and_newest_wins() {
    let dir = tmp_dir("s2_scan");
    let runs = dir.join("runs");
    let old = runs.join("campaign_old");
    let retry = runs.join("campaign_retry");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&retry).unwrap();
    std::fs::write(
        old.join("campaign.json"),
        serde_json::json!({
            "chain_from": "campaign_p1",
            "started_at": "2026-10-21T00:10:00Z",
            "finished_at": "2026-10-21T00:20:00Z"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        retry.join("campaign.json"),
        serde_json::json!({
            "chain_from": "campaign_p1",
            "started_at": "2026-10-21T00:30:00Z",
            "finished_at": "2026-10-21T00:40:00Z",
            "refork": {"of": "campaign_old", "reason": "retry"}
        })
        .to_string(),
    )
    .unwrap();
    // Newest chain successor: the retry, not the first directory hit.
    assert_eq!(
        chain_already_used(&runs, "campaign_p1").as_deref(),
        Some("campaign_retry")
    );
    // S2: the superseded branch old is named by retry.refork.of and is
    // therefore used; continuing it needs --refork.
    assert_eq!(
        chain_already_used(&runs, "campaign_old").as_deref(),
        Some("campaign_retry")
    );
    let err = resolve_refork(Some("campaign_retry"), Some("campaign_old"), None).unwrap_err();
    assert!(
        err.contains("campaign_retry") && err.contains("--refork"),
        "{err}"
    );
    let r = resolve_refork(
        Some("campaign_retry"),
        Some("campaign_old"),
        Some("deliberate fork"),
    )
    .unwrap()
    .expect("refork");
    assert_eq!(r.of, "campaign_retry");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn s2_refork_of_uses_the_newest_successor() {
    // The scan's newest choice is covered in main_campaign's chain test;
    // this pins the refork record's `of` semantics.
    let r = resolve_refork(Some("newest"), Some("old"), Some("retry"))
        .unwrap()
        .expect("refork");
    assert_eq!(r.of, "newest");
    let err = resolve_refork(Some("newest"), Some("old"), None).unwrap_err();
    assert!(err.contains("newest") && err.contains("--refork"), "{err}");
}

#[test]
fn q6_refork_resolution_and_recording() {
    assert_eq!(resolve_refork(None, Some("root"), None), Ok(None));
    let err = resolve_refork(Some("child"), Some("root"), None).unwrap_err();
    assert!(err.contains("child") && err.contains("--refork"), "{err}");
    let r = resolve_refork(Some("child"), Some("root"), Some("retry it"))
        .unwrap()
        .expect("refork");
    assert_eq!((r.of.as_str(), r.reason.as_str()), ("child", "retry it"));
    let err = resolve_refork(None, None, Some("x")).unwrap_err();
    assert!(err.contains("requires --chain"), "{err}");
    let err = resolve_refork(None, Some("root"), Some("x")).unwrap_err();
    assert!(err.contains("not used"), "{err}");
    let err = resolve_refork(Some("child"), Some("root"), Some("  ")).unwrap_err();
    assert!(err.contains("non-empty"), "{err}");
}

#[test]
fn q4_a_catch_up_covers_later_passes() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let passed: Vec<u32> = vec![0, 1, 2, 3, 5, 6, 7, 8, 9, 10];
    let accepted = catch_up_accepted(vec![0, 1, 2, 3, 4], &passed);
    for id in 7..=10 {
        assert!(
            accepted.contains(&id),
            "acceptance misses {id}: {accepted:?}"
        );
    }
    let target = lc.spec.stages.iter().find(|s| s.id == 4).unwrap();
    let outcomes: Vec<(u32, LadderOutcome)> =
        passed.iter().map(|id| (*id, LadderOutcome::Full)).collect();
    let md = lc
        .render_spec_md_catch_up(target, &lc.prompt_path(target), &outcomes, &passed)
        .expect("catch-up SPEC.md");
    for id in 7..=10 {
        assert!(
            md.contains(&format!("Ticket {id}")),
            "SPEC misses {id}: {md}"
        );
    }
    assert!(
        md.rfind("catch-up target").unwrap() > md.find("Ticket 7").unwrap(),
        "the target must be last"
    );
}

#[tokio::test]
async fn the_29_t8_and_t11_failures_catch_up_at_phase_starts() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let opts = CampaignRunOptions {
        campaign_id: "replay".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    for (phase_id, failed) in [(3u32, 8u32), (4u32, 11u32)] {
        let phase = lc.spec.phases.iter().find(|p| p.id == phase_id).unwrap();
        let state = phase_state_with_failed(&[failed]);
        let first = std::cell::RefCell::new(None::<(u32, Option<StageVariant>)>);
        let _ = run_campaign_phase(
            &lc,
            &opts,
            Some(phase),
            Some(&state),
            Some("prev"),
            None,
            |attempt| {
                if first.borrow().is_none() {
                    *first.borrow_mut() = Some((attempt.stage.id, Some(attempt.variant)));
                }
                let id = attempt.stage.id;
                async move { Ok(outcome(true, &[(id, true)])) }
            },
            None,
        )
        .await
        .unwrap();
        let (id, variant) = first.into_inner().expect("a catch-up attempt");
        assert_eq!(id, failed, "phase {phase_id} catch-up");
        assert_eq!(variant, Some(StageVariant::Lite), "phase {phase_id} lite");
    }
}

#[tokio::test]
async fn a_phase2_attempt_sees_phase1s_pass_lite_and_rollback() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    let phase = lc.spec.phases.iter().find(|p| p.id == 2).unwrap();
    let opts = CampaignRunOptions {
        campaign_id: "phase2".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 1.0,
        wall_min: None,
    };
    let mut state = phase1_state();
    state.liters.insert(5, vec!["crash-acked-only".into()]);
    let first = std::cell::RefCell::new(None);
    let rollup = run_campaign_phase(
        &lc,
        &opts,
        Some(phase),
        Some(&state),
        Some("prev"),
        None,
        |attempt| {
            if first.borrow().is_none() {
                *first.borrow_mut() = Some((
                    attempt.passed.clone(),
                    attempt.passed_lite.clone(),
                    attempt.rollback_first,
                ));
            }
            async move { Ok(outcome(true, &[(7, true)])) }
        },
        None,
    )
    .await
    .unwrap();
    let (passed, passed_lite, rollback) = first.into_inner().expect("an attempt");
    assert!(passed.contains(&5) && passed.contains(&6), "{passed:?}");
    assert!(passed_lite
        .iter()
        .any(|(s, p)| *s == 5 && p[0] == "crash-acked-only"));
    assert!(rollback, "the chain was dirty");
    assert_eq!(rollup.phase, Some(2));
    assert!((rollup.max_points - 12.0).abs() < 1e-9);
}

/// The shipped sdb-sql campaign must run in `continue` mode: the
/// proposal's ladder continues after a failure (and `needs` decides
/// what is blocked). It shipped without the field once, silently
/// pass-stopping at the first red ticket and hiding the round-3
/// finding-1 fix from every cell.
#[test]
fn shipped_sdb_sql_campaign_continues() {
    let lc = load_campaign("sdb-sql").expect("the sdb-sql campaign loads");
    assert_eq!(
        lc.spec.ladder,
        LadderMode::Continue,
        "campaigns/sdb-sql/campaign.json must declare \"ladder\": \"continue\""
    );
}

/// A stage that failed (full and lite) or was skipped contributes
/// nothing to the later tickets' suites: the declared acceptance list
/// is intersected with the passed stages. One failure must not sink
/// every later ticket.
#[test]
fn accepted_stages_are_the_passed_ones() {
    // Declared [0..=7]; stages 0-4 passed, 5 failed, 6 skipped.
    let declared: Vec<u32> = (0..=7).collect();
    let passed = vec![0, 1, 2, 3, 4];
    assert_eq!(
        Acceptance::accepted_for(&declared, &passed, 7),
        vec![0, 1, 2, 3, 4, 7]
    );
    // The current stage is always included, even before it passed.
    assert_eq!(Acceptance::accepted_for(&declared, &[], 0), vec![0]);
    // A previously passed stage that regressed stays graded.
    assert_eq!(
        Acceptance::accepted_for(&declared, &passed, 5),
        vec![0, 1, 2, 3, 4, 5]
    );
}

#[tokio::test]
async fn test_orchestrator_continue_ladder_lite_needs_and_points() {
    let lc = ladder_fixture();
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
    let opts = CampaignRunOptions {
        campaign_id: "ladder1".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let seen = std::cell::RefCell::new(Vec::<(u32, StageVariant, bool)>::new());
    let passed_lite = std::cell::RefCell::new(Vec::<Vec<(u32, Vec<String>)>>::new());
    let passed_seen = std::cell::RefCell::new(Vec::<Vec<u32>>::new());
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let id = attempt.stage.id;
            let variant = attempt.variant;
            seen.borrow_mut()
                .push((id, variant, attempt.rollback_first));
            passed_lite.borrow_mut().push(attempt.passed_lite.clone());
            passed_seen.borrow_mut().push(attempt.passed.clone());
            let passed = match (id, variant) {
                (0, StageVariant::Full) => true,
                (1, StageVariant::Full) => false,
                (1, StageVariant::Lite) => true,
                (2, StageVariant::Full) => false,
                (4, StageVariant::Full) => true,
                other => panic!("unexpected attempt {other:?}"),
            };
            let out = outcome(passed, &[(1, passed)]);
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();

    // Attempt order: full 0, full 1, lite 1 (rollback), full 2, full 4
    // (rollback); stage 3 is skipped by `needs`.
    let seen = seen.into_inner();
    assert_eq!(
        seen,
        vec![
            (0, StageVariant::Full, false),
            (1, StageVariant::Full, false),
            (1, StageVariant::Lite, true),
            (2, StageVariant::Full, false),
            (4, StageVariant::Full, true),
        ],
        "attempt order/rollback flags"
    );
    // The lite pass of stage 1 is carried into every later attempt, so
    // a later ticket's verifier keeps grading stage 1 at the variant
    // the model actually passed (finding: a lite stage-5 pass used to
    // make ticket 6 unwinnable).
    let prof = vec![(1u32, vec!["inner_join".to_string()])];
    let carried = passed_lite.into_inner();
    assert_eq!(carried[0], vec![], "nothing lite before the pass");
    assert_eq!(carried[1], vec![], "the full attempt carries nothing");
    assert_eq!(
        carried[2],
        vec![],
        "the lite attempt itself carries nothing"
    );
    assert_eq!(carried[3], prof, "the stage-2 attempt keeps stage 1 lite");
    assert_eq!(carried[4], prof, "the stage-4 attempt keeps stage 1 lite");
    // The passed set grows with every pass, lite passes included
    // (stage 1 passed lite; stage 3 was skipped).
    let passed = passed_seen.into_inner();
    assert_eq!(passed[0], Vec::<u32>::new(), "nothing before stage 0");
    assert_eq!(passed[1], vec![0]);
    assert_eq!(passed[2], vec![0u32], "the lite attempt sees the same");
    assert_eq!(passed[3], vec![0, 1], "a lite pass counts as passed");
    assert_eq!(
        passed[4],
        vec![0, 1],
        "stage 2 failed and is excluded; stage 3 was skipped"
    );
    // Records: 0 full pass, 1 full fail + 1 lite pass, 2 failed,
    // 3 skipped, 4 full pass.
    let by_id: Vec<(u32, &str, bool, f64)> = rollup
        .stages
        .iter()
        .map(|r| (r.id, r.variant.as_str(), r.passed, r.points))
        .collect();
    assert_eq!(
        by_id,
        vec![
            (0, "full", true, 1.0),
            (1, "full", false, 0.0),
            (1, "lite", true, 0.8),
            (2, "full", false, 0.0),
            (3, "full", false, 0.0), // skipped record
            (4, "full", true, 1.0),
        ]
    );
    assert_eq!(rollup.stages[4].status, "skipped");
    assert_eq!(rollup.reached, Some(4));
    assert!((rollup.points - 2.8).abs() < 1e-9, "{}", rollup.points);
    assert!((rollup.max_points - 8.0).abs() < 1e-9);
    // The lite pass satisfies `needs` (stage 2 ran) and the lite's
    // points are tier x weight.
    assert_eq!(rollup.regression_free_tickets, 3);
    assert!(!rollup.over_budget && !rollup.over_wall);
}

#[tokio::test]
async fn test_orchestrator_budget_stop_is_not_a_failure() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "c2".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        // One stage costs 0.01; the reserve is $0.02 and the floor is
        // $0.02, so a $0.04 cap starts exactly one ticket and the
        // second cannot fit the reserve plus floor (round-17 P0).
        budget_usd: 0.04,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        |_stage| async { Ok(outcome(true, &[(1, true), (2, true)])) },
        None,
    )
    .await
    .unwrap();
    // Round-11 finding 2: this is the floor stop, not an overshoot.
    assert!(rollup.cap_reached && !rollup.over_budget);
    assert_eq!(stop_reason(&rollup), "budget cap");
    assert_eq!(rollup.stages.len(), 1);
    assert_eq!(rollup.reached, Some(0));
}

#[tokio::test]
async fn test_orchestrator_records_regression_and_agent_failure() {
    let lc = fixture_campaign();
    let opts = CampaignRunOptions {
        campaign_id: "c3".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    // Stage 0 green; stage 1 regresses stage 1 (was green, now red).
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let stage = attempt.stage;
            let out = if stage.id == 0 {
                outcome(true, &[(1, true), (2, true)])
            } else {
                let mut o = outcome(false, &[(1, false), (2, true)]);
                o.agent_failed = true;
                o
            };
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    let s1 = &rollup.stages[1];
    assert_eq!(s1.regressed, vec![1]);
    // F2: an agent exit after grading is a candidate failure, not a
    // harness fault.
    assert_eq!(s1.status, "failed");
    assert_eq!(rollup.regression_free_tickets, 1);
}

/// F2: the round was graded and the agent then exited non-zero. The
/// result is a candidate failure, so the lite still runs.
#[tokio::test]
async fn f2_a_graded_agent_crash_fails_and_runs_the_lite() {
    let lc = fixture_campaign_with_lite();
    let opts = CampaignRunOptions {
        campaign_id: "f2crash".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let rollup = run_campaign(
        &lc,
        &opts,
        |attempt| {
            let out = if attempt.stage.id == 0 {
                outcome(true, &[(1, true), (2, true)])
            } else if attempt.stage.id == 1 && attempt.variant == StageVariant::Full {
                let mut o = outcome(false, &[(1, false)]);
                o.agent_failed = true;
                o
            } else if attempt.stage.id == 1 && attempt.variant == StageVariant::Lite {
                outcome(true, &[(1, true)])
            } else {
                outcome(true, &[(1, true), (2, true)])
            };
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    let full = rollup
        .stages
        .iter()
        .find(|r| r.id == 1 && r.variant == "full")
        .expect("full ticket 1");
    assert_eq!(full.status, "failed");
    let lite = rollup
        .stages
        .iter()
        .find(|r| r.id == 1 && r.variant == "lite")
        .expect("the lite must run after a graded crash");
    assert_eq!(lite.status, "passed");
    assert!(lite.points > 0.0, "the lite keeps its points: {lite:?}");
}

/// Round-14 N2: a harness fault (the agent never started, a billing
/// trip) stops the cell. It is not a ladder step that blocks `needs` and
/// lets the cell publish as a normal measurement.
#[tokio::test]
async fn test_orchestrator_stops_on_harness_fault() {
    let lc = fixture_campaign_with_lite();
    assert!(lc.spec.stages.len() > 2, "the fixture needs a ladder");
    let opts = CampaignRunOptions {
        campaign_id: "c4".into(),
        model: "m".into(),
        harness: "omp".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        wall_min: None,
    };
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let c = calls.clone();
    let rollup = run_campaign(
        &lc,
        &opts,
        move |attempt| {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = if attempt.stage.id == 0 {
                outcome(true, &[(1, true), (2, true)])
            } else {
                let mut o = outcome(false, &[]);
                o.infra_error = true;
                o.harness_fault = Some("the agent never reached its first turn".into());
                o
            };
            async move { Ok(out) }
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "no attempt after the fault (no lite, no later ticket)"
    );
    assert_eq!(rollup.stages.len(), 2);
    assert_eq!(rollup.stages[1].status, "fault");
    assert!(
        !rollup
            .stages
            .iter()
            .any(|r| r.id == 1 && r.variant == "lite"),
        "a never-started agent must not get a lite"
    );
    let why = rollup
        .harness_fault
        .as_deref()
        .expect("the stop is recorded");
    assert!(why.starts_with("ticket 1:"), "{why}");
    assert_eq!(stop_reason(&rollup), "harness fault");
    assert_eq!(rollup.reached, Some(0));
    // The rollup round-trips (old rollups have no field).
    let v = serde_json::to_value(&rollup).unwrap();
    let back: CampaignRollup = serde_json::from_value(v).unwrap();
    assert_eq!(back.harness_fault, rollup.harness_fault);
}

#[test]
fn test_git_commit_stage_helper() {
    let dir = tmp_dir("git");
    std::fs::write(dir.join("main.go"), "package main").unwrap();
    let (h1, stat1) = git_commit_stage(&dir, 0, "mvp", "pass").unwrap();
    assert!(!h1.is_empty());
    assert!(stat1.contains("stage 0: mvp [pass]") || stat1.contains("mvp"));
    std::fs::write(dir.join("ttl.go"), "package main").unwrap();
    let (h2, _) = git_commit_stage(&dir, 1, "expiry", "pass").unwrap();
    assert_ne!(h1, h2);
    let patch = git_head_patch(&dir).unwrap();
    assert!(patch.contains("ttl.go"), "stage patch must carry the diff");
    let _ = std::fs::remove_dir_all(&dir);
}

/// R68 F4: the SPEC.md branches for a skipped and an unattempted ticket
/// (the failed branch already has its own test).
#[test]
fn r68_f4_render_spec_md_marks_skipped_and_unattempted() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    let s3 = lc.spec.stages.iter().find(|s| s.id == 3).unwrap();
    let outcomes = vec![(0, LadderOutcome::Full), (1, LadderOutcome::Skipped)];
    let md = lc
        .render_spec_md(s3, &lc.prompt_path(s3), &outcomes)
        .unwrap();
    assert!(md.contains("SKIPPED, not attempted"), "{md}");
    assert!(md.contains("was **skipped**"), "{md}");
    assert!(md.contains("not attempted"), "{md}");
    assert!(!md.contains("# Ticket 1"), "a skipped ticket is collapsed");
    assert!(
        !md.contains("# Ticket 2"),
        "an unattempted ticket is collapsed"
    );
    assert!(md.contains("# Ticket 0"), "a passed ticket stays in full");
}

/// R68 F4: the harness note's skipped and rollback-anchor branches.
#[test]
fn r68_f4_attempt_preamble_names_skipped_and_rollback_anchor() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("campaigns");
    let lc = load_campaign_from(&root, "sdb-sql").unwrap();
    let s4 = lc.spec.stages.iter().find(|s| s.id == 4).unwrap();
    let accepted = Acceptance::accepted_for(s4.acceptance.stages_for("sql"), &[0, 2], 4);
    let outcomes = vec![
        (0, LadderOutcome::Full),
        (1, LadderOutcome::Skipped),
        (2, LadderOutcome::Full),
    ];
    let p = attempt_preamble(
        &lc.spec.stages,
        s4,
        StageVariant::Full,
        true,
        &outcomes,
        &accepted,
    )
    .expect("a skipped ticket needs a note");
    assert!(p.contains("Ticket 1 ("), "{p}");
    assert!(p.contains("skipped"), "{p}");
    assert!(p.contains("rolled back to ticket 2's commit"), "{p}");
    // Two skipped tickets exercise the plural list.
    let outcomes = vec![
        (0, LadderOutcome::Full),
        (1, LadderOutcome::Skipped),
        (2, LadderOutcome::Skipped),
    ];
    let p = attempt_preamble(
        &lc.spec.stages,
        s4,
        StageVariant::Full,
        true,
        &outcomes,
        &accepted,
    )
    .expect("two skipped tickets need a note");
    assert!(p.contains("Tickets 1 (") && p.contains(" and 2 ("), "{p}");
    assert!(p.contains("were skipped"), "{p}");
    // No passed ticket: the rollback anchor is the starting state.
    let outcomes = vec![(0, LadderOutcome::Failed), (1, LadderOutcome::Skipped)];
    let p = attempt_preamble(
        &lc.spec.stages,
        s4,
        StageVariant::Full,
        true,
        &outcomes,
        &accepted,
    )
    .expect("failures need a note");
    assert!(p.contains("rolled back to the starting state"), "{p}");
}

/// R68 F4: every career-phase validation guard fails closed.
#[test]
fn r68_f4_validate_spec_phase_errors() {
    let mut lc = fixture_campaign();
    let phase = |id: u32, title: &str, stages: Vec<u32>| PhaseSpec {
        id,
        title: title.into(),
        stages,
    };
    let check = |lc: &LoadedCampaign, want: &str| {
        let err = validate_spec(&lc.spec, &lc.root).unwrap_err().to_string();
        assert!(err.contains(want), "wanted {want:?}, got {err}");
    };

    lc.spec.phases = vec![phase(2, "p", vec![0, 1, 2])];
    check(&lc, "numbered 1..=n");
    lc.spec.phases = vec![phase(1, "", vec![0, 1, 2])];
    check(&lc, "empty title");
    lc.spec.phases = vec![phase(1, "p", vec![])];
    check(&lc, "has no stages");
    lc.spec.phases = vec![phase(1, "p", vec![0, 1, 9])];
    check(&lc, "unknown stage 9");
    lc.spec.phases = vec![phase(1, "p", vec![0, 1]), phase(2, "q", vec![1, 2])];
    check(&lc, "more than one phase");
    lc.spec.phases = vec![phase(1, "p", vec![1, 0])];
    check(&lc, "contiguous and in order");
    lc.spec.phases = vec![phase(1, "p", vec![0, 1])];
    check(&lc, "do not partition");
    lc.spec.phases = vec![phase(1, "p", vec![0, 1, 2])];
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
}

/// R68 F4: the tier, needs and lite validation guards.
#[test]
fn r68_f4_validate_spec_lite_and_needs_errors() {
    let mut lc = fixture_campaign();
    let check = |lc: &LoadedCampaign, want: &str| {
        let err = validate_spec(&lc.spec, &lc.root).unwrap_err().to_string();
        assert!(err.contains(want), "wanted {want:?}, got {err}");
    };
    let lite = |budget: f64, turns: u32, weight: f64, prompt: &str| LiteSpec {
        prompt: prompt.into(),
        profile: Vec::new(),
        budget_usd: budget,
        max_turns: turns,
        weight,
    };

    lc.spec.stages[0].tier = 0;
    check(&lc, "tier must be >= 1");
    lc.spec.stages[0].tier = 1;
    lc.spec.stages[1].needs = vec![9];
    check(&lc, "needs unknown stage 9");
    lc.spec.stages[1].needs = vec![1];
    check(&lc, "not an earlier stage");
    lc.spec.stages[1].needs.clear();

    lc.spec.stages[1].lite = Some(lite(0.0, 10, 0.4, "stages/1-lite.md"));
    check(&lc, "lite budget");
    lc.spec.stages[1].lite = Some(lite(0.1, 0, 0.4, "stages/1-lite.md"));
    check(&lc, "lite max_turns");
    lc.spec.stages[1].lite = Some(lite(0.1, 10, 1.0, "stages/1-lite.md"));
    check(&lc, "lite weight");
    lc.spec.stages[1].lite = Some(lite(0.1, 10, 0.4, "stages/missing.md"));
    check(&lc, "lite prompt missing");
    std::fs::write(lc.root.join("stages/1-lite.md"), "# lite").unwrap();
    lc.spec.stages[1].lite = Some(lite(0.1, 10, 0.4, "stages/1-lite.md"));
    assert!(validate_spec(&lc.spec, &lc.root).is_ok());
}

/// R74 F1: the health counter on saved excerpts of the real 2026-11-08
/// logs. The minimax phase is the 25-errors-in-73-turns provider fault;
/// the luna-pro phase carries ref-sql refusals; the deepseek excerpt is
/// clean.
#[test]
fn r74_f1_health_from_logs() {
    let minimax = health_from_live_log(include_str!("fixtures/health/minimax-20261108-w1.txt"));
    assert_eq!(minimax.turns, 73, "minimax turns");
    assert_eq!(minimax.tool_errors, 25, "minimax provider tool errors");
    assert_eq!(minimax.ref_queries, 11, "minimax ref-sql queries");
    assert_eq!(minimax.ref_refused, 0, "minimax ref-sql refusals");
    assert!(
        minimax.turn_secs_p50 > 0.0 && minimax.turn_secs_max >= minimax.turn_secs_p50,
        "turn seconds: {:?}",
        minimax
    );

    let luna = health_from_live_log(include_str!("fixtures/health/luna-pro-20261108-w3.txt"));
    assert_eq!(luna.turns, 264, "luna turns");
    assert_eq!(luna.tool_errors, 0, "luna has no provider tool errors");
    assert!(luna.ref_queries > 0, "luna queried ref-sql");
    assert!(luna.ref_refused > 0, "luna saw ref-sql refusals");
    assert!(
        luna.ref_refused < luna.ref_queries,
        "the refusals are a minority: {:?}",
        luna
    );

    let deep = health_from_live_log(include_str!("fixtures/health/deepseek-20261031.txt"));
    assert_eq!(deep.turns, 60, "deepseek turns");
    assert_eq!(deep.tool_errors, 0, "deepseek has no provider tool errors");
    assert_eq!(deep.ref_refused, 0, "deepseek has no ref-sql refusals");
    assert_eq!(deep.empty_responses, 0, "deepseek has no empty responses");

    // The billed/list ratio is set by the rollup writer, not the log.
    let mut billed = minimax.clone();
    billed.billed_ratio = Some(1.1);
    assert_eq!(billed.billed_ratio, Some(1.1));
}
