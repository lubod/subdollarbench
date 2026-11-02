use super::*;

#[test]
fn test_regime_of() {
    let base = BenchmarkRunResult {
        id: "r".to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: Some(70000.0),
        reference_throughput_req_sec: Some(71000.0),
        baseline_source: Some("batch".to_string()),
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.0,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: String::new(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    assert_eq!(BenchmarkRunResult::regime_of(&base), "legacy");
    let mut pinned = base;
    let image = "docker.io/library/redis@sha256:becdda6c7f4b3fb42e42fd7f120bbf5c54c4caaaf16f26da24e4563d2c1f0576";
    pinned.ref_image = Some(image.to_string());
    assert_eq!(
        BenchmarkRunResult::regime_of(&pinned).as_str(),
        &image["docker.io/library/redis@sha256:".len()..][..12]
    );
    pinned.ref_drift = Some(true);
    assert!(BenchmarkRunResult::regime_of(&pinned).ends_with("+drift"));
}

fn stab_run(pass: f64, tp: Option<f64>) -> BenchmarkRunResult {
    BenchmarkRunResult {
        id: "t".to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: pass,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: tp,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.0,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: String::new(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    }
}

#[test]
fn test_canonical_best_tie_break_pin() {
    // Tie-break contract: pass rate first, then throughput else
    // efficiency. Pinned because merge + review-run converge here —
    // a silent change swaps which trial's workspace gets reviewed.
    let mut a = stab_run(100.0, Some(50000.0));
    a.id = "trial_a".to_string();
    let mut b = stab_run(100.0, Some(60000.0));
    b.id = "trial_b".to_string();
    // Higher throughput wins the tied-100% pair regardless of order:
    // max_by returns the LAST max, so callers must pass the same
    // subruns order (documented on canonical_best).
    assert_eq!(
        BenchmarkRunResult::canonical_best(&[a.clone(), b.clone()]).map(|r| r.id.as_str()),
        Some("trial_b")
    );
    // Pass rate dominates throughput.
    let mut c = stab_run(50.0, Some(999999.0));
    c.id = "trial_c".to_string();
    assert_eq!(
        BenchmarkRunResult::canonical_best(&[c.clone(), b.clone()]).map(|r| r.id.as_str()),
        Some("trial_b")
    );
    // No throughput anywhere: falls back to efficiency_score.
    let mut d = stab_run(100.0, None);
    d.id = "trial_d".to_string();
    d.efficiency_score = 10.0;
    let mut e = stab_run(100.0, None);
    e.id = "trial_e".to_string();
    e.efficiency_score = 20.0;
    assert_eq!(
        BenchmarkRunResult::canonical_best(&[d, e]).map(|r| r.id.as_str()),
        Some("trial_e")
    );
    // Empty: None.
    assert!(BenchmarkRunResult::canonical_best(&[]).is_none());
}

#[test]
fn test_quarantine_filter_gate() {
    // The gate exists even with an empty quarantine list (dns removal
    // emptied it 2026-10-05): nothing quarantined, lookup is
    // case-insensitive, and the list stays the single choke point.
    assert!(!is_quarantined("redis"));
    assert!(!is_quarantined("REDIS"));
    assert!(!is_quarantined("dns"));
    assert!(QUARANTINED_TASKS.is_empty());
}

#[test]
fn test_assess_stability() {
    // Identical trials -> stable.
    let r = [
        stab_run(100.0, Some(35000.0)),
        stab_run(100.0, Some(35600.0)),
    ];
    assert_eq!(assess_stability(&r).0, Stability::Stable);
    // Deterministic partial pass -> stable.
    let r = [
        stab_run(83.3, None),
        stab_run(83.3, None),
        stab_run(83.3, None),
    ];
    assert_eq!(assess_stability(&r).0, Stability::Stable);
    // Pass spread -> unstable.
    let r = [
        stab_run(100.0, Some(82000.0)),
        stab_run(0.0, None),
        stab_run(100.0, Some(83000.0)),
    ];
    let (s, reason) = assess_stability(&r);
    assert_eq!(s, Stability::Unstable);
    assert!(reason.contains("pass spread"));
    // Throughput CV > 25% -> unstable; > 50% flags the probe.
    let r = [
        stab_run(100.0, Some(852.0)),
        stab_run(100.0, Some(110687.0)),
    ];
    let (s, reason) = assess_stability(&r);
    assert_eq!(s, Stability::Unstable);
    assert!(reason.contains("suspect measurement"));
    // CV 10-25% -> watch.
    let r = [
        stab_run(100.0, Some(44000.0)),
        stab_run(100.0, Some(57000.0)),
    ];
    assert_eq!(assess_stability(&r).0, Stability::Watch);
    // Single trial -> stable.
    assert_eq!(
        assess_stability(&[stab_run(100.0, None)]).0,
        Stability::Stable
    );
}

#[test]
fn test_merge_subruns_best_of_and_pass_at_k() {
    let make_run =
        |id: &str, tp: f64, dur: f64, turns: u32, cost: f64, pass: f64| BenchmarkRunResult {
            id: id.to_string(),
            model: "test-model".to_string(),
            // redis: an anchored task, so the fixture tests merge math
            // rather than the denominator scrub (pinned by
            // the gate test in cost/calculator.rs).
            task: "redis".to_string(),
            language: "Python".to_string(),
            effort: Some("auto".to_string()),
            effort_effective: None,
            status: None,
            harness: None,
            pass_rate: pass,
            passed_stages: if pass >= 100.0 { 6 } else { 3 },
            total_stages: 6,
            throughput_req_sec: Some(tp),
            reference_throughput_req_sec: Some(19000.0),
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            prompt_tokens: 1000,
            cached_tokens: 2000,
            completion_tokens: 500,
            total_tokens: 3500,
            total_cost_usd: cost,
            cost_source: None,
            cost_formula_usd: None,
            effective_cost_usd: Some(cost * 2.0),
            duration_seconds: Some(dur),
            turns: Some(turns),
            savings_percent: 50.0,
            efficiency_score: 100.0,
            throughput_score: Some(tp / 10.0),
            timestamp: "2026-09-12T08:00:00Z".to_string(),
            method_version: Some("0.2.0".to_string()),
            rules_version: None,
            graded_under: None,
            subruns: Vec::new(),
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
            avg_pass_rate: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            frontier_fraction: None,
            protocol: None,
            solved: None,
            solved_at: None,
            best_stage_score: None,
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        };

    let t1 = make_run("run_trial1", 5000.0, 40.0, 8, 0.001, 100.0);
    let t2 = make_run("run_trial2", 6000.0, 30.0, 7, 0.002, 100.0);
    let t3 = make_run("run_trial3", 7000.0, 50.0, 9, 0.003, 100.0);

    let merged =
        BenchmarkRunResult::merge_subruns("run_parent", vec![t1, t2, t3], None, None).unwrap();
    assert_eq!(merged.id, "run_parent");
    // Grader pass-through (420/420 incident): the parent grader comes
    // ONLY from the explicit param — never from trial stamps. A re-merge
    // of stored rows whose trials carry a newer stamp must not launder
    // it onto the parent.
    let stale_trials = vec![
        make_run("s1", 5000.0, 40.0, 8, 0.001, 100.0),
        make_run("s2", 6000.0, 30.0, 7, 0.002, 100.0),
    ];
    let kept =
        BenchmarkRunResult::merge_subruns("p", stale_trials, None, Some("grader-a".to_string()))
            .unwrap();
    assert_eq!(kept.graded_under.as_deref(), Some("grader-a"));
    let dropped = BenchmarkRunResult::merge_subruns(
        "p",
        vec![make_run("s3", 5000.0, 40.0, 8, 0.001, 100.0)],
        None,
        None,
    )
    .unwrap();
    assert_eq!(dropped.graded_under, None);
    assert_eq!(merged.total_trials, Some(3));
    assert_eq!(merged.pass_rate, 100.0);
    assert_eq!(merged.pass_at_1, Some(100.0));
    assert_eq!(merged.pass_at_k, Some(100.0));
    assert_eq!(merged.avg_pass_rate, Some(100.0));
    assert_eq!(merged.subruns.len(), 3);
    // Fair expected scoring: median TP (6000), mean cost over ALL
    // trials (0.002 — all pass here, so identical to passing-only).
    assert_eq!(merged.throughput_req_sec.unwrap(), 6000.0);
    assert_eq!(merged.duration_seconds.unwrap(), 40.0);
    assert_eq!(merged.turns.unwrap(), 8);
    assert_eq!(merged.total_cost_usd, 0.002);

    // Mixed outcome: 2 of 3 pass. Median TP of passing (5500); mean
    // cost over ALL trials ((0.001+0.002+0.003)/3 = 0.002) — failed
    // spend counts. Duration/turns also all-trial: (40+30+50)/3 = 40s,
    // (8+7+9)/3 = 8 turns. Score scaled by Pass@1 (66.7%).
    let f1 = make_run("run_a", 5000.0, 40.0, 8, 0.001, 100.0);
    let f2 = make_run("run_b", 6000.0, 30.0, 7, 0.002, 100.0);
    let f3 = make_run("run_c", 0.0, 50.0, 9, 0.003, 0.0);
    let mixed =
        BenchmarkRunResult::merge_subruns("run_mixed", vec![f1, f2, f3], None, None).unwrap();
    assert!((mixed.pass_rate - 66.66666666666667).abs() < 1e-9);
    assert_eq!(mixed.throughput_req_sec.unwrap(), 5500.0);
    assert_eq!(mixed.total_cost_usd, 0.002);
    assert_eq!(mixed.duration_seconds.unwrap(), 40.0);
    assert_eq!(mixed.turns.unwrap(), 8);
    assert!((mixed.avg_pass_rate.unwrap() - 66.66666666666667).abs() < 1e-9);
    assert!((mixed.pass_at_1.unwrap() - 66.66666666666667).abs() < 1e-6);
    assert!(mixed.efficiency_score > 0.0);
    assert!(mixed.throughput_score.unwrap() > 0.0);
    assert_eq!(mixed.solved, Some(true));
    assert_eq!(mixed.solved_at, Some(1));
    assert_eq!(mixed.best_stage_score, Some(100.0));

    // Sequential capability protocol test with unsolved trials
    let mut u1 = make_run("u1", 0.0, 40.0, 8, 0.001, 25.0);
    u1.protocol = Some("sequential_cap".to_string());
    let mut u2 = make_run("u2", 0.0, 30.0, 7, 0.002, 50.0);
    u2.protocol = Some("sequential_cap".to_string());
    let unsolved =
        BenchmarkRunResult::merge_subruns("run_unsolved", vec![u1, u2], None, None).unwrap();
    assert_eq!(unsolved.protocol.as_deref(), Some("sequential_cap"));
    assert_eq!(unsolved.solved, Some(false));
    assert_eq!(unsolved.solved_at, None);
    assert_eq!(unsolved.best_stage_score, Some(50.0));
}

#[test]
fn test_merge_scrubs_unscorable_denominator() {
    let mk = |id: &str| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "proxy".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 5,
        total_stages: 5,
        throughput_req_sec: Some(9_000.0),
        reference_throughput_req_sec: Some(30_000.0),
        baseline_source: Some("live".to_string()),
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 100,
        cached_tokens: 0,
        completion_tokens: 50,
        total_tokens: 150,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.02),
        duration_seconds: Some(10.0),
        turns: Some(5),
        savings_percent: 0.0,
        efficiency_score: 1.0,
        throughput_score: Some(3.0),
        timestamp: "2026-10-02T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    let m = BenchmarkRunResult::merge_subruns("p", vec![mk("a"), mk("b")], None, None).unwrap();
    // Candidate rate survives (real measurement); denominator, source
    // and score are gone (no valid comparison exists).
    assert_eq!(m.throughput_req_sec, Some(9_000.0));
    assert_eq!(m.reference_throughput_req_sec, None);
    assert_eq!(m.baseline_source, None);
    assert_eq!(m.throughput_score, None);
}

#[test]
fn test_merge_median_skips_zero_throughput() {
    let mk = |id: &str, tp: Option<f64>| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: tp,
        reference_throughput_req_sec: Some(70_000.0),
        baseline_source: Some("live".to_string()),
        ref_drift: None,
        ref_image: Some("docker.io/library/redis@sha256:abc".to_string()),
        prompt_tokens: 1000,
        cached_tokens: 0,
        completion_tokens: 500,
        total_tokens: 1500,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.02),
        duration_seconds: Some(10.0),
        turns: Some(5),
        savings_percent: 0.0,
        efficiency_score: 1.0,
        throughput_score: None,
        timestamp: "2026-10-02T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    // One crashed trial (0.0) + two real readings: median of the real two.
    let m = BenchmarkRunResult::merge_subruns(
        "p",
        vec![
            mk("a", Some(0.0)),
            mk("b", Some(60_000.0)),
            mk("c", Some(70_000.0)),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.throughput_req_sec, Some(65_000.0));
}

#[test]
fn test_wilson_interval_95() {
    // Thin cells are honestly wide; full cells narrow.
    let (lo3, hi3) = wilson_interval_95(3, 3);
    assert!(lo3 < 50.0, "n=3 3/3 lower bound {lo3}");
    assert!((hi3 - 100.0).abs() < 1e-9);
    let (lo0, hi0) = wilson_interval_95(3, 0);
    assert!((lo0 - 0.0).abs() < 1e-9);
    assert!(hi0 > 50.0, "n=3 0/3 upper bound {hi0}");
    let (lo9, hi9) = wilson_interval_95(9, 8);
    assert!(lo9 > 55.0 && lo9 < 90.0, "n=9 8/9 lower {lo9}");
    assert!(hi9 > 95.0, "n=9 8/9 upper {hi9}");
    assert_eq!(wilson_interval_95(0, 0), (0.0, 0.0));
    // Merge stores the interval: 2/3 -> P@1 66.7 with wide bounds.
    let mk = |id: &str, pass: f64| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "wal".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: pass,
        passed_stages: 6,
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.01),
        duration_seconds: Some(10.0),
        turns: Some(5),
        savings_percent: 0.0,
        efficiency_score: 1.0,
        throughput_score: None,
        timestamp: "2026-09-12T08:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    let m = BenchmarkRunResult::merge_subruns(
        "w",
        vec![mk("a", 100.0), mk("b", 100.0), mk("c", 0.0)],
        None,
        None,
    )
    .unwrap();
    let (lo, hi) = (m.pass_at_1_lo.unwrap(), m.pass_at_1_hi.unwrap());
    assert!((m.pass_at_1.unwrap() - 66.66666666666667).abs() < 1e-6);
    assert!(lo > 15.0 && lo < 40.0, "2/3 lower {lo}");
    assert!(hi > 85.0, "2/3 upper {hi}");
}

/// A drifted batch bracket must not produce a throughput score: the
/// denominator those trials were divided by is known-untrustworthy, so the
/// ratio is wrong. The candidate's own throughput reading is still valid
/// and must survive, and the efficiency score is unaffected.
#[test]
fn drift_suppresses_throughput_score_but_keeps_measurement() {
    let trial = |id: &str| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: Some(35_000.0),
        reference_throughput_req_sec: Some(70_000.0),
        baseline_source: Some("batch".to_string()),
        ref_drift: None,
        ref_image: Some("docker.io/library/redis@sha256:abc".to_string()),
        prompt_tokens: 1000,
        cached_tokens: 0,
        completion_tokens: 500,
        total_tokens: 1500,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.02),
        duration_seconds: Some(60.0),
        turns: Some(10),
        savings_percent: 0.0,
        efficiency_score: 50.0,
        throughput_score: None,
        timestamp: "2026-09-30T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    let subs = vec![trial("a"), trial("b"), trial("c")];

    let clean =
        BenchmarkRunResult::merge_subruns("clean", subs.clone(), Some(false), None).unwrap();
    assert!(
        clean.throughput_score.unwrap_or(0.0) > 0.0,
        "an un-drifted batch reference must still score"
    );
    assert_eq!(clean.throughput_req_sec, Some(35_000.0));

    let drifted =
        BenchmarkRunResult::merge_subruns("drift", subs.clone(), Some(true), None).unwrap();
    assert_eq!(
        drifted.throughput_score, None,
        "a drifted bracket must not yield a ranked throughput score"
    );
    assert_eq!(
        drifted.throughput_req_sec,
        Some(35_000.0),
        "the candidate's own reading is still valid and must be reported"
    );
    assert_eq!(drifted.ref_drift, Some(true));
    assert!(
        drifted.efficiency_score > 0.0,
        "efficiency is cost/pass-based and must survive drift quarantine"
    );

    // Same trials, unknown bracket: not scored, not silently trusted.
    let unknown = BenchmarkRunResult::merge_subruns("unknown", subs, None, None).unwrap();
    assert!(unknown.throughput_score.unwrap_or(0.0) > 0.0);
}

/// The n=1 early return must honor the batch bracket too: with
/// stop-on-pass, a 100% cell is usually a single trial, so the drifted
/// denominator bug hit exactly the most common passing shape. (Live:
/// ling/opencode w3 ranked #2 on a 142k->201k drifted bracket.)
#[test]
fn single_trial_drift_is_not_dropped() {
    let trial = |id: &str| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: Some("completed".to_string()),
        harness: Some("opencode".to_string()),
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: Some(35_000.0),
        reference_throughput_req_sec: Some(142_000.0),
        baseline_source: Some("batch".to_string()),
        ref_drift: None,
        ref_image: Some("docker.io/library/redis@sha256:abc".to_string()),
        prompt_tokens: 1000,
        cached_tokens: 0,
        completion_tokens: 500,
        total_tokens: 1500,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.02),
        duration_seconds: Some(60.0),
        turns: Some(10),
        savings_percent: 0.0,
        efficiency_score: 50.0,
        throughput_score: Some(1.0),
        timestamp: "2026-09-30T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };

    let clean = BenchmarkRunResult::merge_subruns("one-clean", vec![trial("a")], Some(false), None)
        .unwrap();
    assert_eq!(clean.ref_drift, Some(false));
    assert!(
        clean.throughput_score.unwrap_or(0.0) > 0.0,
        "an un-drifted single trial must still score"
    );

    let drifted =
        BenchmarkRunResult::merge_subruns("one-drift", vec![trial("a")], Some(true), None).unwrap();
    assert_eq!(
        drifted.ref_drift,
        Some(true),
        "n=1 must carry the batch bracket verdict, not drop it"
    );
    assert_eq!(
        drifted.throughput_score, None,
        "a drifted bracket must not yield a ranked throughput score at n=1"
    );
    assert_eq!(
        drifted.throughput_req_sec,
        Some(35_000.0),
        "the candidate's own reading is still valid and must be reported"
    );
    assert!(
        drifted.efficiency_score > 0.0,
        "efficiency is cost/pass-based and must survive drift quarantine"
    );

    // Unknown bracket stays unknown: not silently trusted, not invented.
    let unknown =
        BenchmarkRunResult::merge_subruns("one-unknown", vec![trial("a")], None, None).unwrap();
    assert_eq!(unknown.ref_drift, None);
    assert!(unknown.throughput_score.unwrap_or(0.0) > 0.0);
}

#[test]
fn test_compute_pass_at_k() {
    assert_eq!(compute_pass_at_k(0, 0, 1), 0.0);
    assert_eq!(compute_pass_at_k(5, 0, 1), 0.0);
    assert_eq!(compute_pass_at_k(5, 5, 1), 1.0);
    assert_eq!(compute_pass_at_k(5, 5, 5), 1.0);
    assert_eq!(compute_pass_at_k(3, 1, 3), 1.0);
    let p1 = compute_pass_at_k(3, 1, 1);
    assert!((p1 - 0.3333).abs() < 0.01);
}

/// The pass@k denominator must be the number of trials that produced a
/// verdict. When infra failures counted as failures, a 3-trial cell with
/// one real pass and two Docker outages reported 33% pass@1 instead of
/// 50%, and the model was ranked as if it had failed twice.
#[test]
fn test_infra_failures_are_excluded_from_the_denominator() {
    let requested: u32 = 3;
    let infra: u32 = 1;
    let scored = requested - infra;
    let passing = 1usize;
    let p1 = compute_pass_at_k(scored as usize, passing, 1);
    assert!(
        (p1 - 0.5).abs() < 1e-9,
        "expected 50% over 2 scored trials, got {p1}"
    );
    // The old behaviour, for contrast: same data, 3 in the denominator.
    let wrong = compute_pass_at_k(requested as usize, passing, 1);
    assert!(wrong < p1, "counting infra failures must understate pass@1");

    // Underflow safety: more infra failures than trials must not wrap.
    let over = 5u32.saturating_sub(9);
    assert_eq!(over, 0);
    assert_eq!(compute_pass_at_k(over as usize, 0, 1), 0.0);
}

#[test]
fn test_language_detection() {
    let temp = std::env::temp_dir().join(format!("test_lang_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);

    // Go
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("main.go"), "package main").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Go");
    let _ = fs::remove_dir_all(&temp);

    // Rust nested
    fs::create_dir_all(temp.join("rust-pkg/src")).unwrap();
    fs::write(temp.join("rust-pkg/Cargo.toml"), "[package]").unwrap();
    fs::write(temp.join("rust-pkg/src/main.rs"), "fn main() {}").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Rust");
    let _ = fs::remove_dir_all(&temp);

    // Python
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("server.py"), "print('hi')").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Python");
    let _ = fs::remove_dir_all(&temp);

    // Node
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("index.js"), "console.log(1)").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Node.js");
    let _ = fs::remove_dir_all(&temp);

    // Empty dir
    fs::create_dir_all(&temp).unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Unknown");
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_language_detection_c_and_more() {
    let temp = std::env::temp_dir().join(format!("test_c_lang_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);

    // C/C++
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("main.c"), "int main() {}").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "C/C++");
    let _ = fs::remove_dir_all(&temp);

    // Go with go.mod
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("go.mod"), "module test").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Go");
    let _ = fs::remove_dir_all(&temp);

    // Python with requirements.txt
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("requirements.txt"), "flask").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Python");
    let _ = fs::remove_dir_all(&temp);

    // Rust with Cargo.toml
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("Cargo.toml"), "[package]").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Rust");
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_print_table_runs_without_panic() {
    let r = BenchmarkRunResult {
        id: "run_test".to_string(),
        model: "model_test".to_string(),
        task: "redis".to_string(),
        language: "Rust".to_string(),
        effort: Some("auto".to_string()),
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: Some(60000.0),
        reference_throughput_req_sec: Some(71000.0),
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 1000,
        cached_tokens: 500,
        completion_tokens: 100,
        total_tokens: 1600,
        total_cost_usd: 0.005,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.008),
        duration_seconds: Some(60.0),
        turns: Some(4),
        savings_percent: 50.0,
        efficiency_score: 200.0,
        throughput_score: Some(538.0),
        timestamp: "2026-09-04T12:00:00Z".to_string(),
        method_version: Some("0.1.0".to_string()),
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    LeaderboardManager::print_table(&[r]);
    LeaderboardManager::print_table(&[]);
}

#[test]
fn test_finalize_filters_trial_junk_and_legacy_singles() {
    // Junk: loose `_trial` row with no subruns/count (killed-run debris).
    let junk = sample_junk_row("cell_trial3");
    // Legacy single: same unscored shape but bare id (pre-subrun format).
    // No subruns + no trial count = no evidence, never a cell.
    let legacy = sample_junk_row("legacy_single");
    let list = LeaderboardManager::finalize_results(vec![junk, legacy]);
    assert!(list.is_empty());
}

fn sample_junk_row(id: &str) -> BenchmarkRunResult {
    BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Rust".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 0.0,
        passed_stages: 0,
        total_stages: 4,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.0,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-09-28T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    }
}

#[test]
fn test_leaderboard_sorting_and_persistence() {
    let temp = std::env::temp_dir().join(format!("test_lb_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);

    // r1 has lower pass rate (50%), but high efficiency score (300.0) due to low cost
    let r1 = BenchmarkRunResult {
        id: "run_partial_pass".to_string(),
        model: "model_b".to_string(),
        task: "redis".to_string(),
        language: "Python".to_string(),
        effort: Some("low".to_string()),
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 50.0,
        passed_stages: 2,
        total_stages: 4,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 1000,
        cached_tokens: 500,
        completion_tokens: 200,
        total_tokens: 1700,
        total_cost_usd: 0.001,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.003),
        duration_seconds: Some(40.0),
        turns: Some(2),
        savings_percent: 25.0,
        efficiency_score: 0.0,
        throughput_score: Some(0.0),
        timestamp: "2026-09-04T06:00:00Z".to_string(),
        method_version: Some("0.1.0".to_string()),
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: Some(1),
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };

    // r2 has 100% pass rate, efficiency score (200.0)
    let r2 = BenchmarkRunResult {
        id: "run_full_pass".to_string(),
        model: "model_a".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: Some("high".to_string()),
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: Some(50000.0),
        reference_throughput_req_sec: Some(71000.0),
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 2000,
        cached_tokens: 1500,
        completion_tokens: 300,
        total_tokens: 3800,
        total_cost_usd: 0.005,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.008),
        duration_seconds: Some(50.0),
        turns: Some(3),
        savings_percent: 60.0,
        efficiency_score: 200.0,
        throughput_score: Some(481.0),
        timestamp: "2026-09-04T06:05:00Z".to_string(),
        method_version: Some("0.1.0".to_string()),
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: Some(1),
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };

    LeaderboardManager::save_result(temp.to_str().unwrap(), &r1).unwrap();
    LeaderboardManager::save_result(temp.to_str().unwrap(), &r2).unwrap();

    let list = LeaderboardManager::load_all(temp.to_str().unwrap());
    assert_eq!(list.len(), 2);
    // Full pass (100%) must rank higher than partial pass (50%) regardless of efficiency score!
    assert_eq!(list[0].id, "run_full_pass");
    assert_eq!(list[1].id, "run_partial_pass");

    let _ = fs::remove_dir_all(&temp);
}
#[test]
fn test_leaderboard_total_tokens_fallback_when_omitted() {
    let json_data = r#"{
        "id": "legacy_run",
        "model": "legacy_model",
        "task": "redis",
        "language": "Rust",
        "pass_rate": 100.0,
        "passed_stages": 4,
        "total_stages": 4,
        "prompt_tokens": 1200,
        "cached_tokens": 800,
        "completion_tokens": 300,
        "total_cost_usd": 0.01,
        "total_trials": 1,
        "savings_percent": 40.0,
        "efficiency_score": 100.0,
        "timestamp": "2026-09-04T12:00:00Z"
    }"#;

    let res: BenchmarkRunResult = serde_json::from_str(json_data).unwrap();
    assert_eq!(res.total_tokens, 0); // Defaulted by serde

    let temp = std::env::temp_dir().join(format!("test_lb_fallback_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp);
    fs::write(temp.join("legacy_run.json"), json_data).unwrap();

    let loaded = LeaderboardManager::load_all(temp.to_str().unwrap());
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].total_tokens, 2300); // 1200 + 800 + 300

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_reconsolidate_saved_results_empty() {
    let temp = std::env::temp_dir().join(format!("test_reconsolidate_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp);
    let count = LeaderboardManager::reconsolidate_saved_results(temp.to_str().unwrap()).unwrap();
    assert_eq!(count, 0);
    let _ = fs::remove_dir_all(&temp);
}

#[test]
#[ignore]
fn run_reconsolidate_historical_results() {
    let count = LeaderboardManager::reconsolidate_saved_results("results").unwrap();
    println!(
        "Reconsolidated {} historical multi-trial runs in results/",
        count
    );
}

/// Cheat exclusion (§K): DQ trials never enter aggregates but stay
/// listed; mixed parents rank on valid trials, all-cheat stay out.
#[test]
fn test_merge_excludes_cheat_trials_but_keeps_them_listed() {
    let mk = |id: &str, pass: f64, status: Option<&str>| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "pdf".to_string(),
        language: "Python".to_string(),
        effort: Some("high".to_string()),
        effort_effective: None,
        status: status.map(|s| s.to_string()),
        harness: None,
        pass_rate: pass,
        passed_stages: if pass >= 100.0 { 6 } else { 0 },
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-10-09T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: vec![],
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    // 8 honest (6x100 + 2x0) + 1 cheat at 0%: the cheat must not move
    // any aggregate, but must stay in subruns with its stamp.
    let mut subs = vec![];
    for i in 0..6 {
        subs.push(mk(&format!("t{i}"), 100.0, Some("completed")));
    }
    subs.push(mk("t6", 0.0, Some("failed_tests")));
    subs.push(mk("t7", 0.0, Some("failed_tests")));
    subs.push(mk("t8", 0.0, Some("disqualified_cheat")));
    let m = BenchmarkRunResult::merge_subruns("p", subs, None, None).unwrap();
    // 6/8 over valid trials — the cheat (0%) is not in the denominator.
    assert_eq!(m.total_trials, Some(8));
    assert!((m.pass_at_1.unwrap() - 75.0).abs() < 1e-9);
    assert!((m.avg_pass_rate.unwrap() - 75.0).abs() < 1e-9);
    assert_eq!(m.subruns.len(), 9, "cheat stays listed");
    assert!(m
        .subruns
        .iter()
        .any(|s| s.status.as_deref() == Some("disqualified_cheat")));
    assert_eq!(
        m.status, None,
        "mixed parent ranks (boards filter DQ rows, not parents)"
    );
}

#[test]
fn test_merge_all_cheat_parent_stays_disqualified() {
    let mk = |id: &str| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "pdf".to_string(),
        language: "Python".to_string(),
        effort: None,
        effort_effective: None,
        status: Some("disqualified_cheat".to_string()),
        harness: None,
        pass_rate: 0.0,
        passed_stages: 0,
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-10-09T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: vec![],
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    let m = BenchmarkRunResult::merge_subruns("p", vec![mk("a"), mk("b")], None, None).unwrap();
    assert_eq!(m.status.as_deref(), Some("disqualified_cheat"));
}

/// Non-result exclusion (2026-10-13): harness/agent faults never enter
/// aggregates but stay listed; parents rank on measured trials only.
#[test]
fn test_merge_excludes_fault_trials_but_keeps_them_listed() {
    let mk = |id: &str, pass: f64, status: Option<&str>| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: Some("high".to_string()),
        effort_effective: None,
        status: status.map(|s| s.to_string()),
        harness: None,
        pass_rate: pass,
        passed_stages: if pass >= 100.0 {
            6
        } else if pass > 0.0 {
            4
        } else {
            0
        },
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-10-13T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: vec![],
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    // Canary shape: two scored 0s + one scored 66.7 + one billing-fault
    // 0%. The fault must not enter the denominator or the mean.
    let m = BenchmarkRunResult::merge_subruns(
        "p",
        vec![
            mk("p_trial1", 0.0, Some("failed_tests")),
            mk("p_trial2", 0.0, Some("failed_tests")),
            mk("p_trial3", 66.66666666666667, Some("failed_tests")),
            mk("p_trial4", 0.0, Some("agent_failed")),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.total_trials, Some(3));
    assert_eq!(m.pass_at_1, Some(0.0));
    assert!((m.avg_pass_rate.unwrap() - 22.22222222222222).abs() < 1e-9);
    assert!((m.best_stage_score.unwrap() - 66.66666666666667).abs() < 1e-9);
    assert_eq!(m.subruns.len(), 4, "fault stays listed");
    assert_eq!(m.status, None, "measured parent ranks");
    assert_eq!(m.solved, Some(false));
    // Archived shape (2026-10-10): 3 scored passes in 9 with 3 faults
    // reads 3/6, not 3/9.
    let mut subs = vec![];
    for i in 1..=3 {
        subs.push(mk(&format!("q_trial{i}"), 100.0, Some("completed")));
    }
    for i in 4..=6 {
        subs.push(mk(&format!("q_trial{i}"), 0.0, Some("failed_tests")));
    }
    for i in 7..=9 {
        subs.push(mk(&format!("q_trial{i}"), 0.0, Some("agent_failed")));
    }
    let m2 = BenchmarkRunResult::merge_subruns("q", subs, None, None).unwrap();
    assert_eq!(m2.total_trials, Some(6));
    assert!((m2.pass_at_1.unwrap() - 50.0).abs() < 1e-9);
    assert_eq!(m2.subruns.len(), 9);
}

#[test]
fn test_merge_solved_at_uses_trial_numbers_across_fault_gap() {
    let mk = |id: &str, pass: f64, status: &str| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: Some(status.to_string()),
        harness: None,
        pass_rate: pass,
        passed_stages: if pass >= 100.0 { 6 } else { 0 },
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-10-13T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: vec![],
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    // Fault at trial 1, solve at trial 2: solved_at is the trial number
    // (2), not the aggregate position (1). n counts measured trials.
    let m = BenchmarkRunResult::merge_subruns(
        "p",
        vec![
            mk("p_trial1", 0.0, "agent_failed"),
            mk("p_trial2", 100.0, "completed"),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.total_trials, Some(1));
    assert_eq!(m.pass_at_1, Some(100.0));
    assert_eq!(m.solved, Some(true));
    assert_eq!(m.solved_at, Some(2));
    // Cheat + fault + solve: same rule, trial 3.
    let m2 = BenchmarkRunResult::merge_subruns(
        "q",
        vec![
            mk("q_trial1", 0.0, "disqualified_cheat"),
            mk("q_trial2", 0.0, "infra_error"),
            mk("q_trial3", 100.0, "completed"),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m2.total_trials, Some(1));
    assert_eq!(m2.solved_at, Some(3));
    assert_eq!(m2.subruns.len(), 3);
}

#[test]
fn test_merge_all_fault_parent_carries_neutral_status() {
    let mk = |id: &str, status: &str| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: Some(status.to_string()),
        harness: None,
        pass_rate: 0.0,
        passed_stages: 0,
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-10-13T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: vec![],
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    // Nothing measured: the parent carries the first fault's status
    // (neutral pill downstream), never failed_tests, and keeps every
    // trial listed for audit.
    let m = BenchmarkRunResult::merge_subruns(
        "p",
        vec![
            mk("p_trial1", "agent_failed"),
            mk("p_trial2", "infra_error"),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.status.as_deref(), Some("agent_failed"));
    assert!(!is_rankable_status(m.status.as_deref()));
    assert_eq!(m.subruns.len(), 2);
    // Trial-validity contract: legacy (None) + verdicts score; every
    // explicit non-verdict is excluded. Rankability matches validity
    // for verdicts and legacy; cheats stay visible but unranked.
    assert!(is_valid_trial(None));
    assert!(is_valid_trial(Some("completed")));
    assert!(is_valid_trial(Some("failed_tests")));
    for s in [
        Some("disqualified_cheat"),
        Some("agent_failed"),
        Some("agent_killed"),
        Some("infra_error"),
        Some("future_status"),
    ] {
        assert!(!is_valid_trial(s), "{s:?}");
    }
    assert!(is_rankable_status(None));
    assert!(is_rankable_status(Some("completed")));
    assert!(is_rankable_status(Some("failed_tests")));
    for s in [
        Some("disqualified_cheat"),
        Some("agent_failed"),
        Some("agent_killed"),
        Some("infra_error"),
        Some("future_status"),
    ] {
        assert!(!is_rankable_status(s), "{s:?}");
    }
}

#[test]
fn test_merge_repair_attempted_survives_without_conversion() {
    let mk = |id: &str, pass: f64, fb: Option<u32>| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: Some("failed_tests".to_string()),
        harness: None,
        pass_rate: pass,
        passed_stages: if pass >= 100.0 { 6 } else { 0 },
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-10-13T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: vec![],
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: fb,
    };
    // Repair ran twice per trial, never converted: the attempt must
    // survive on the parent even though `repaired` stays None.
    let m = BenchmarkRunResult::merge_subruns(
        "p",
        vec![mk("p_trial1", 0.0, Some(2)), mk("p_trial2", 66.0, Some(2))],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.repaired, None);
    assert_eq!(m.repair_attempted, Some(true));
    assert_eq!(m.feedback_rounds, Some(2));
    // Enabled but unused throughout: explicit false, not unknown.
    let m2 = BenchmarkRunResult::merge_subruns(
        "q",
        vec![mk("q_trial1", 0.0, Some(0)), mk("q_trial2", 0.0, Some(0))],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m2.repair_attempted, Some(false));
    assert_eq!(m2.feedback_rounds, Some(0));
    // Pre-field rows: unknown stays unknown.
    let m3 = BenchmarkRunResult::merge_subruns(
        "r",
        vec![mk("r_trial1", 0.0, None), mk("r_trial2", 0.0, None)],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m3.repair_attempted, None);
    assert_eq!(m3.feedback_rounds, None);
}

#[test]
fn test_finalize_excludes_disqualified_from_ranking() {
    // A cheat verdict is not a measurement: DQ rows never rank, but
    // honest rows in the same set are untouched.
    let mk = |id: &str, status: Option<&str>| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: status.map(|s| s.to_string()),
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 1.0,
        throughput_score: None,
        timestamp: "2026-10-09T00:00:00Z".to_string(),
        method_version: None,
        rules_version: Some("v1".to_string()),
        graded_under: None,
        subruns: vec![],
        total_trials: Some(1),
        pass_at_1: Some(100.0),
        pass_at_k: Some(100.0),
        avg_pass_rate: Some(100.0),
        pass_at_1_lo: Some(20.0),
        pass_at_1_hi: Some(100.0),
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    let out = LeaderboardManager::finalize_results(vec![
        mk("dq1", Some("disqualified_cheat")),
        mk("ok1", None),
    ]);
    assert_eq!(out.len(), 1, "DQ row must not rank");
    assert_eq!(out[0].id, "ok1");
}

#[test]
fn test_frontier_fractions() {
    // Best anchored tp per (task, rules) = 1.0; others fractioned.
    // Unanchored rows (no ref) keep None; losers fraction below 1.
    let mk = |id: &str, tp: Option<f64>, rt: Option<f64>| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: tp,
        reference_throughput_req_sec: rt,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 1.0,
        throughput_score: None,
        timestamp: String::new(),
        method_version: None,
        rules_version: Some("v1".to_string()),
        graded_under: None,
        subruns: vec![BenchmarkRunResult {
            id: format!("{id}_trial1"),
            model: "m".to_string(),
            task: "redis".to_string(),
            language: "Go".to_string(),
            effort: None,
            effort_effective: None,
            harness: None,
            status: None,
            pass_rate: 100.0,
            passed_stages: 4,
            total_stages: 4,
            throughput_req_sec: tp,
            reference_throughput_req_sec: rt,
            baseline_source: None,
            ref_drift: None,
            ref_image: None,
            prompt_tokens: 0,
            cached_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            total_cost_usd: 0.01,
            cost_source: None,
            cost_formula_usd: None,
            effective_cost_usd: None,
            duration_seconds: None,
            turns: None,
            savings_percent: 0.0,
            efficiency_score: 1.0,
            throughput_score: None,
            timestamp: String::new(),
            method_version: None,
            rules_version: Some("v1".to_string()),
            graded_under: None,
            subruns: vec![],
            total_trials: None,
            pass_at_1: None,
            pass_at_k: None,
            avg_pass_rate: None,
            pass_at_1_lo: None,
            pass_at_1_hi: None,
            stability: None,
            frontier_fraction: None,
            protocol: None,
            solved: None,
            solved_at: None,
            best_stage_score: None,
            repaired: None,
            repair_attempted: None,
            feedback_rounds: None,
        }],
        total_trials: Some(1),
        pass_at_1: Some(100.0),
        pass_at_k: Some(100.0),
        avg_pass_rate: Some(100.0),
        pass_at_1_lo: Some(20.0),
        pass_at_1_hi: Some(100.0),
        stability: Some("stable".to_string()),
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    let mut rows = vec![
        mk("fast", Some(40000.0), Some(70000.0)),
        mk("slow", Some(20000.0), Some(70000.0)),
        mk("unanchored", Some(99999.0), None),
    ];
    BenchmarkRunResult::recompute_frontier_fractions(&mut rows);
    assert_eq!(rows[0].frontier_fraction, Some(1.0));
    assert_eq!(rows[1].frontier_fraction, Some(0.5));
    assert_eq!(rows[2].frontier_fraction, None);
    // Loose trial rows (no subruns) never join the denominator set.
    assert!(rows
        .iter()
        .all(|r| !r.id.contains("_trial") || r.frontier_fraction.is_none()));
}

#[test]
fn test_single_trial_wilson_orders_below_nine() {
    // A lone 1/1 must not outrank a 9/9: n=1 Wilson lo (~20.7)
    // sits well below n=9 9/9 lo (~70.0).
    let (lo1, hi1) = wilson_interval_95(1, 1);
    let (lo9, hi9) = wilson_interval_95(9, 9);
    assert!(lo1 < lo9, "1/1 lo {lo1} should be below 9/9 lo {lo9}");
    assert!((hi1 - 100.0).abs() < 1e-9);
    assert!((hi9 - 100.0).abs() < 1e-9);
    let mk = |id: &str, pass: f64| BenchmarkRunResult {
        id: id.to_string(),
        model: "m".to_string(),
        task: "wal".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: pass,
        passed_stages: 6,
        total_stages: 6,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.01,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: Some(0.01),
        duration_seconds: Some(10.0),
        turns: Some(5),
        savings_percent: 0.0,
        efficiency_score: 1.0,
        throughput_score: None,
        timestamp: "2026-09-12T08:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    let one = BenchmarkRunResult::merge_subruns("one", vec![mk("a", 100.0)], None, None).unwrap();
    assert!(one.pass_at_1_lo.unwrap() > 0.0);
    let (olo, ohi) = (one.pass_at_1_lo.unwrap(), one.pass_at_1_hi.unwrap());
    assert!(
        (olo - lo1).abs() < 1e-9,
        "merged 1/1 lo {olo} != wilson {lo1}"
    );
    assert!((ohi - hi1).abs() < 1e-9);
    assert!(
        olo < lo9,
        "single-trial lo {olo} should rank below 9/9 lo {lo9}"
    );
    // A lone failure pins lo at 0 with a wide upper bound.
    let zero = BenchmarkRunResult::merge_subruns("zero", vec![mk("z", 0.0)], None, None).unwrap();
    assert!((zero.pass_at_1_lo.unwrap() - 0.0).abs() < 1e-9);
    assert!(zero.pass_at_1_hi.unwrap() > 50.0);
}

#[test]
fn test_canonical_results_dir_gate() {
    let canonical = crate::config::get_repo_root().join("results");
    let canon_str = canonical.to_str().unwrap();
    assert!(LeaderboardManager::is_canonical_results_dir(canon_str));
    assert!(LeaderboardManager::use_db(canon_str));
    // Without an override, temp dirs must stay on the JSON exports.
    if std::env::var("SDB_BENCH_DB").is_err() {
        let temp = std::env::temp_dir().join(format!("test_gate_{}", std::process::id()));
        assert!(!LeaderboardManager::is_canonical_results_dir(
            temp.to_str().unwrap()
        ));
        assert!(!LeaderboardManager::use_db(temp.to_str().unwrap()));
    }
}

#[test]
fn test_short_model_name() {
    assert_eq!(
        LeaderboardManager::short_model_name("openrouter/deepseek/deepseek-v4.1-flash"),
        "deepseek-v4.1-flash"
    );
    assert_eq!(
        LeaderboardManager::short_model_name("openrouter/meta/muse-spark-1.3-contributor"),
        "muse-spark-1.3-contributor"
    );
    assert_eq!(
        LeaderboardManager::short_model_name("openai/gpt-5.6-luna"),
        "gpt-5.6-luna"
    );
    assert_eq!(
        LeaderboardManager::short_model_name("gemini-3.8-flash"),
        "gemini-3.8-flash"
    );
}

#[test]
fn test_save_result_refuses_test_ids() {
    let temp = std::env::temp_dir().join(format!("test_guard_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    let mk = |id: &str| BenchmarkRunResult {
        id: id.to_string(),
        model: "probe-model".to_string(),
        task: "redis".to_string(),
        language: "Go".to_string(),
        effort: None,
        effort_effective: None,
        status: None,
        harness: None,
        pass_rate: 100.0,
        passed_stages: 4,
        total_stages: 4,
        throughput_req_sec: None,
        reference_throughput_req_sec: None,
        baseline_source: None,
        ref_drift: None,
        ref_image: None,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        total_cost_usd: 0.001,
        cost_source: None,
        cost_formula_usd: None,
        effective_cost_usd: None,
        duration_seconds: None,
        turns: None,
        savings_percent: 0.0,
        efficiency_score: 0.0,
        throughput_score: None,
        timestamp: "2026-09-26T00:00:00Z".to_string(),
        method_version: None,
        rules_version: None,
        graded_under: None,
        subruns: Vec::new(),
        total_trials: None,
        pass_at_1: None,
        pass_at_k: None,
        avg_pass_rate: None,
        pass_at_1_lo: None,
        pass_at_1_hi: None,
        stability: None,
        frontier_fraction: None,
        protocol: None,
        solved: None,
        solved_at: None,
        best_stage_score: None,
        repaired: None,
        repair_attempted: None,
        feedback_rounds: None,
    };
    // test_ ids are refused: Ok, but nothing written.
    LeaderboardManager::save_result(temp.to_str().unwrap(), &mk("test_probe_1")).unwrap();
    assert!(!temp.join("test_probe_1.json").exists());
    // Ordinary ids still save.
    LeaderboardManager::save_result(temp.to_str().unwrap(), &mk("real_run_1")).unwrap();
    assert!(temp.join("real_run_1.json").exists());
    let _ = fs::remove_dir_all(&temp);
}

// --- targeted coverage (wave 3) ---------------------------------------

/// Minimal fully-defaulted result for branch tests: a legacy redis row.
/// Mutate per test; ids stay outside the real runs/results namespaces.
fn cov_run(id: &str) -> BenchmarkRunResult {
    let mut r = stab_run(100.0, None);
    r.id = id.to_string();
    r
}

#[test]
fn test_format_duration_branches() {
    assert_eq!(format_duration(0.0), "-");
    assert_eq!(format_duration(-3.5), "-");
    assert_eq!(format_duration(59.4), "59s");
    assert_eq!(format_duration(60.0), "1m 0s");
    assert_eq!(format_duration(125.6), "2m 6s");
}

#[test]
fn test_legacy_rules_version_cutover() {
    // Strictly before the cutover instant is pre-cheat; the instant
    // itself and later is legacy-cheat.
    assert_eq!(
        legacy_rules_version("2026-09-17T23:59:59"),
        RULES_LEGACY_PRE_CHEAT
    );
    assert_eq!(
        legacy_rules_version("2026-09-18T00:00:00Z"),
        RULES_LEGACY_CHEAT
    );
    assert_eq!(
        legacy_rules_version("2026-09-19T12:00:00"),
        RULES_LEGACY_CHEAT
    );
    assert_eq!(RULES_LEGACY_PRE_CHEAT, "legacy-pre-cheat");
    assert_eq!(RULES_LEGACY_CHEAT, "legacy-cheat");
}

#[test]
fn test_harness_regime_rules_helpers() {
    let mut r = cov_run("helper_run");
    // Legacy rows are graded as OMP; explicit harness passes through.
    assert_eq!(BenchmarkRunResult::harness_of(&r), "omp");
    r.harness = Some("opencode".to_string());
    assert_eq!(BenchmarkRunResult::harness_of(&r), "opencode");

    // Rules stamp: unscored legacy, scored only, and grader->scorer.
    assert_eq!(BenchmarkRunResult::rules_of(&r), "legacy");
    r.rules_version = Some("2026-09-30.1".to_string());
    assert_eq!(BenchmarkRunResult::rules_of(&r), "2026-09-30.1");
    r.graded_under = Some("2026-09-27.2".to_string());
    assert_eq!(
        BenchmarkRunResult::rules_of(&r),
        "2026-09-27.2\u{2192}2026-09-30.1"
    );
    // Equal grader and scorer collapse to the plain stamp.
    r.graded_under = Some("2026-09-30.1".to_string());
    assert_eq!(BenchmarkRunResult::rules_of(&r), "2026-09-30.1");

    // Regime digest: unpinned tag, digest with sha256: prefix, +drift.
    r.ref_image = Some("registry.example/img:v1.2".to_string());
    assert_eq!(BenchmarkRunResult::regime_of(&r), "registry.exa");
    r.ref_image = Some("sha256:abcdef0123456789".to_string());
    assert_eq!(BenchmarkRunResult::regime_of(&r), "abcdef012345");
    r.ref_drift = Some(false);
    assert_eq!(BenchmarkRunResult::regime_of(&r), "abcdef012345");
    r.ref_drift = Some(true);
    assert_eq!(BenchmarkRunResult::regime_of(&r), "abcdef012345+drift");
    r.ref_image = None;
    assert_eq!(BenchmarkRunResult::regime_of(&r), "legacy");
}

#[test]
fn test_cost_per_solved_variants() {
    let mut r = cov_run("cps");
    r.pass_rate = 25.0;
    r.total_cost_usd = 0.5;
    r.effective_cost_usd = Some(0.25);
    // P@1 present: effective cost / (P@1/100).
    r.pass_at_1 = Some(50.0);
    assert_eq!(r.cost_per_solved(), Some(0.5));
    // P@1 absent: falls back to the headline pass rate.
    r.pass_at_1 = None;
    assert_eq!(r.cost_per_solved(), Some(1.0));
    // Unsolved: undefined, not zero.
    r.pass_at_1 = Some(0.0);
    assert_eq!(r.cost_per_solved(), None);
    r.pass_at_1 = None;
    r.pass_rate = 0.0;
    assert_eq!(r.cost_per_solved(), None);
    // Effective cost absent: falls back to API cost.
    r.effective_cost_usd = None;
    r.pass_rate = 100.0;
    assert_eq!(r.cost_per_solved(), Some(0.5));
}

#[test]
fn test_wilson_interval_exact_values() {
    // Pinned constants (z=1.96): a change in the maths must be visible.
    let (lo1, hi1) = wilson_interval_95(1, 1);
    assert!((lo1 - 20.6543).abs() < 1e-3, "1/1 lo {lo1}");
    assert!((hi1 - 100.0).abs() < 1e-9);
    let (lo2, hi2) = wilson_interval_95(2, 1);
    assert!((lo2 - 9.4529).abs() < 1e-3, "1/2 lo {lo2}");
    assert!((hi2 - 90.5471).abs() < 1e-3, "1/2 hi {hi2}");
    let (lo3, hi3) = wilson_interval_95(3, 1);
    assert!((lo3 - 6.1490).abs() < 1e-3, "1/3 lo {lo3}");
    assert!((hi3 - 79.2345).abs() < 1e-3, "1/3 hi {hi3}");
    // 0/3 clamps the lower bound at exactly zero.
    let (zlo, zhi) = wilson_interval_95(3, 0);
    assert_eq!(zlo, 0.0);
    assert!((zhi - 56.1506).abs() < 1e-3, "0/3 hi {zhi}");
}

#[test]
fn test_compute_pass_at_k_edge_branches() {
    // c > n and k > n are out of contract -> 0 (unless c == n).
    assert_eq!(compute_pass_at_k(3, 4, 1), 0.0);
    assert_eq!(compute_pass_at_k(3, 1, 4), 0.0);
    // k == 0 with a full pass still reports 1.0.
    assert_eq!(compute_pass_at_k(5, 5, 0), 1.0);
    assert_eq!(compute_pass_at_k(0, 0, 0), 0.0);
    // n - c < k: every draw of k must contain a pass.
    assert_eq!(compute_pass_at_k(9, 8, 9), 1.0);
    assert_eq!(compute_pass_at_k(9, 8, 2), 1.0);
    // Straight combinatorial product: 2 passes in 4, k=1 -> 50%.
    let p = compute_pass_at_k(4, 2, 1);
    assert!((p - 0.5).abs() < 1e-9, "{p}");
}

#[test]
fn test_assess_stability_zero_throughput_reads() {
    // Two explicit zero readings: mean is zero, so the CV gate is
    // skipped (no fabricated division) and the cell reads stable.
    let r = [stab_run(100.0, Some(0.0)), stab_run(100.0, Some(0.0))];
    assert_eq!(assess_stability(&r).0, Stability::Stable);
    assert_eq!(assess_stability(&r).1, "2 trials agree");
    // A single throughput reading is not enough for a CV: pass spread
    // alone decides.
    let r = [
        stab_run(100.0, Some(1000.0)),
        stab_run(100.0, None),
        stab_run(100.0, None),
    ];
    assert_eq!(assess_stability(&r).0, Stability::Stable);
}

#[test]
fn test_merge_provenance_unanimity_and_mixed_stamps() {
    let mk =
        |id: &str, cs: Option<&str>, ee: Option<&str>, cf: Option<f64>, proto: Option<&str>| {
            let mut r = cov_run(id);
            r.cost_source = cs.map(|s| s.to_string());
            r.effort_effective = ee.map(|s| s.to_string());
            r.cost_formula_usd = cf;
            r.protocol = proto.map(|s| s.to_string());
            r
        };
    // Unanimous: one provenance story, summed formula cost.
    let m = BenchmarkRunResult::merge_subruns(
        "prov_unanimous",
        vec![
            mk("a", Some("live"), Some("high"), Some(0.01), Some("fixed_n")),
            mk("b", Some("live"), Some("high"), Some(0.02), Some("fixed_n")),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.cost_source.as_deref(), Some("live"));
    assert_eq!(m.effort_effective.as_deref(), Some("high"));
    assert!((m.cost_formula_usd.unwrap() - 0.03).abs() < 1e-12);
    assert_eq!(m.protocol.as_deref(), Some("fixed_n"));

    // Disagreement: "mixed" on both provenance columns and protocol.
    let m = BenchmarkRunResult::merge_subruns(
        "prov_mixed",
        vec![
            mk("a", Some("live"), Some("high"), Some(0.01), Some("fixed_n")),
            mk(
                "b",
                Some("formula"),
                Some("#low"),
                Some(0.03),
                Some("sequential_cap"),
            ),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.cost_source.as_deref(), Some("mixed"));
    assert_eq!(m.effort_effective.as_deref(), Some("mixed"));
    assert_eq!(m.protocol.as_deref(), Some("mixed"));

    // First trial without provenance: no single story -> None even
    // though the second trial has one (unanimity over the recorded set).
    let m = BenchmarkRunResult::merge_subruns(
        "prov_first_none",
        vec![
            mk("a", None, None, None, None),
            mk("b", Some("live"), Some("high"), Some(0.01), Some("fixed_n")),
        ],
        None,
        None,
    )
    .unwrap();
    assert_eq!(m.cost_source, None);
    assert_eq!(m.effort_effective, None);
    assert_eq!(m.cost_formula_usd, None);
    // Protocol is read from the full subrun list, so the one recorded
    // value survives even when the first trial has none.
    assert_eq!(m.protocol.as_deref(), Some("fixed_n"));
    assert_eq!(m.total_trials, Some(2));
}

#[test]
fn test_merge_protocol_none_when_unrecorded() {
    let r1 = cov_run("np_trial1");
    let r2 = cov_run("np_trial2");
    let m = BenchmarkRunResult::merge_subruns("np", vec![r1, r2], None, None).unwrap();
    assert_eq!(m.protocol, None, "not recorded is never invented");
}

#[test]
fn test_single_trial_merge_recompute_and_drift() {
    // Partial-credit single trial: headline keeps its pass rate but the
    // unified score is base_eff(100%) x P@1(0) = 0.
    let mut r = cov_run("one_trial1");
    r.pass_rate = 50.0;
    r.passed_stages = 2;
    r.total_cost_usd = 0.02;
    r.effective_cost_usd = Some(0.01);
    r.throughput_score = Some(9.0);
    r.throughput_req_sec = None;
    r.reference_throughput_req_sec = None;
    let m = BenchmarkRunResult::merge_subruns("one", vec![r], None, None).unwrap();
    assert_eq!(m.pass_at_1, Some(0.0));
    assert_eq!(m.pass_rate, 50.0);
    assert_eq!(m.efficiency_score, 0.0);
    assert_eq!(
        m.throughput_score, None,
        "no measured rate -> no throughput score"
    );
    assert_eq!(m.solved, Some(false));
    assert_eq!(m.solved_at, None);
    assert_eq!(m.best_stage_score, Some(50.0));
    assert_eq!(m.total_trials, Some(1));

    // Anchored 1/1: score recomputes positive, solve lands at trial 1.
    let mut r = cov_run("two_trial1");
    r.effective_cost_usd = Some(0.01);
    r.throughput_req_sec = Some(30_000.0);
    r.reference_throughput_req_sec = Some(70_000.0);
    r.throughput_score = Some(1.0);
    let m = BenchmarkRunResult::merge_subruns("two", vec![r], None, None).unwrap();
    assert_eq!(m.pass_at_1, Some(100.0));
    assert!(m.throughput_score.unwrap() > 0.0);
    assert_eq!(m.solved, Some(true));
    assert_eq!(m.solved_at, Some(1));

    // The trial itself was already drift-flagged and the batch says
    // nothing: the trial's own verdict survives and suppresses scoring.
    let mut r = cov_run("three_trial1");
    r.effective_cost_usd = Some(0.01);
    r.throughput_req_sec = Some(30_000.0);
    r.reference_throughput_req_sec = Some(70_000.0);
    r.throughput_score = Some(1.0);
    r.ref_drift = Some(true);
    let m = BenchmarkRunResult::merge_subruns("three", vec![r], None, None).unwrap();
    assert_eq!(m.ref_drift, Some(true));
    assert_eq!(m.throughput_score, None);
}

#[test]
fn test_merge_no_valid_trial_picks_fault_over_cheat() {
    let mut cheat = cov_run("mix_trial1");
    cheat.status = Some("disqualified_cheat".to_string());
    let mut fault = cov_run("mix_trial2");
    fault.status = Some("agent_killed".to_string());
    let m = BenchmarkRunResult::merge_subruns("mix", vec![cheat, fault], None, None).unwrap();
    assert_eq!(
        m.status.as_deref(),
        Some("agent_killed"),
        "the neutral fault pill wins; cheat stays visible in subruns"
    );
    assert_eq!(m.subruns.len(), 2);
}

#[test]
fn test_finalize_regroups_loose_trials_and_prefers_parents() {
    let mk = |id: &str, pass: f64| {
        let mut r = cov_run(id);
        r.pass_rate = pass;
        r.passed_stages = if pass >= 100.0 { 4 } else { 0 };
        r.total_trials = Some(1);
        r.timestamp = "2026-09-28T00:00:00Z".to_string();
        r
    };
    // Two loose trials of one cell merge into a synthetic parent
    // ("cell_trialN" -> "cell"); a lone loose trial stays as-is.
    let out = LeaderboardManager::finalize_results(vec![
        mk("cell_trial1", 100.0),
        mk("cell_trial2", 0.0),
        mk("lonely_trial1", 50.0),
    ]);
    assert_eq!(out.len(), 2);
    let cell = out.iter().find(|r| r.id == "cell").expect("merged parent");
    assert_eq!(cell.total_trials, Some(2));
    assert!((cell.pass_at_1.unwrap() - 50.0).abs() < 1e-9);
    assert_eq!(cell.subruns.len(), 2);
    assert!(out.iter().any(|r| r.id == "lonely_trial1"));
    // 1/2 Wilson lo (9.45) loses to the lone 50% headline: rank_lo
    // falls back to pass_rate for legacy rows without Wilson fields.
    assert_eq!(out[0].id, "lonely_trial1");
    assert_eq!(out[1].id, "cell");

    // A child embedded in a stored parent is dropped as a loose row
    // before any regrouping.
    let mut parent = cov_run("parent_x");
    parent.subruns = vec![cov_run("kid_trial1")];
    parent.total_trials = Some(2);
    let out = LeaderboardManager::finalize_results(vec![parent, mk("kid_trial1", 100.0)]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, "parent_x");
}

#[test]
fn test_finalize_rank_tiebreaks() {
    let mk = |id: &str, p1: f64, lo: f64, cost: f64, tp: f64| {
        let mut r = cov_run(id);
        r.total_trials = Some(1);
        r.pass_at_1 = Some(p1);
        r.pass_at_1_lo = Some(lo);
        r.pass_at_1_hi = Some(100.0);
        r.avg_pass_rate = Some(p1);
        r.effective_cost_usd = Some(cost);
        r.throughput_req_sec = Some(tp);
        r.throughput_score = None;
        r.efficiency_score = 0.0;
        r
    };
    // Equal lo + p1 + score: cheaper wins; at equal cost, faster wins.
    let out = LeaderboardManager::finalize_results(vec![
        mk("tie_expensive", 50.0, 10.0, 0.5, 999_999.0),
        mk("tie_cheap_slow", 50.0, 10.0, 0.1, 1_000.0),
        mk("tie_cheap_fast", 50.0, 10.0, 0.1, 2_000.0),
    ]);
    let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["tie_cheap_fast", "tie_cheap_slow", "tie_expensive"]);
    // Equal lo but different P@1: the point estimate breaks the tie.
    let out = LeaderboardManager::finalize_results(vec![
        mk("p1_low", 20.0, 10.0, 0.1, 1_000.0),
        mk("p1_high", 80.0, 10.0, 0.1, 1_000.0),
    ]);
    assert_eq!(out[0].id, "p1_high");
}

#[test]
fn test_frontier_fractions_group_by_harness() {
    let mut a = cov_run("fr_a");
    a.subruns = vec![cov_run("fr_a_trial1")];
    a.throughput_req_sec = Some(10.0);
    a.reference_throughput_req_sec = Some(100.0);
    let mut b = cov_run("fr_b");
    b.subruns = vec![cov_run("fr_b_trial1")];
    b.harness = Some("opencode".to_string());
    b.throughput_req_sec = Some(20.0);
    b.reference_throughput_req_sec = Some(100.0);
    let mut c = cov_run("fr_c");
    c.subruns = vec![cov_run("fr_c_trial1")];
    c.throughput_req_sec = Some(40.0);
    c.reference_throughput_req_sec = Some(100.0);
    // Zero reference is not anchored. A loose row (no subruns) never
    // enters the denominator set, but still gets a numerator reading
    // against its own key when one exists, and None when it does not.
    let mut d = cov_run("fr_d");
    d.subruns = vec![cov_run("fr_d_trial1")];
    d.throughput_req_sec = Some(50.0);
    d.reference_throughput_req_sec = Some(0.0);
    let mut e = cov_run("fr_e");
    e.task = "http".to_string();
    e.throughput_req_sec = Some(60.0);
    e.reference_throughput_req_sec = Some(100.0);
    let mut f = cov_run("fr_f");
    f.throughput_req_sec = Some(20.0);
    f.reference_throughput_req_sec = Some(100.0);
    let mut rows = vec![a, b, c, d, e, f];
    BenchmarkRunResult::recompute_frontier_fractions(&mut rows);
    // omp group best is fr_c (40): fr_a = 0.25; opencode group best is
    // itself (its own harness key); the loose redis reading halves.
    assert_eq!(rows[0].frontier_fraction, Some(0.25));
    assert_eq!(rows[1].frontier_fraction, Some(1.0));
    assert_eq!(rows[2].frontier_fraction, Some(1.0));
    assert_eq!(rows[3].frontier_fraction, None);
    assert_eq!(rows[4].frontier_fraction, None);
    assert_eq!(rows[5].frontier_fraction, Some(0.5));
}

#[test]
fn test_throughput_units_and_badges() {
    assert_eq!(LeaderboardManager::throughput_unit("PDF"), "pages/s");
    assert_eq!(LeaderboardManager::throughput_unit("wal"), "msgs/s");
    assert_eq!(
        LeaderboardManager::throughput_unit("pubsub"),
        "deliveries/s"
    );
    assert_eq!(LeaderboardManager::throughput_unit("binproto"), "ops/s");
    assert_eq!(LeaderboardManager::throughput_unit("redis"), "req/s");
    assert_eq!(LeaderboardManager::throughput_unit("http"), "req/s");

    assert_eq!(
        LeaderboardManager::pricing_badge("muse-contributor"),
        " [contrib]"
    );
    assert_eq!(
        LeaderboardManager::pricing_badge("z-ai/ling-3.1-flash"),
        " [free]"
    );
    assert_eq!(
        LeaderboardManager::pricing_badge("space-bunny-1"),
        " [free]"
    );
    assert_eq!(
        LeaderboardManager::pricing_badge("vendor/model:free"),
        " [free]"
    );
    assert_eq!(LeaderboardManager::pricing_badge("openai/gpt-5"), "");
}

#[test]
fn test_model_cli_meta_vendors() {
    let cases: [(&str, &str, Color); 11] = [
        ("anthropic/claude-x", "✦", Color::Red),
        ("openai/gpt-5", "✳️", Color::Green),
        ("google/gemini-3", "✨", Color::Blue),
        ("deepseek/v4", "🐋", Color::Cyan),
        ("meta/llama-4", "🦙", Color::Magenta),
        ("qwen/qwen-3", "🔮", Color::DarkMagenta),
        ("mistral/codestral", "🌪️", Color::DarkYellow),
        ("minimax/m2", "⚡", Color::DarkRed),
        ("amazon/nova-pro", "☀️", Color::Yellow),
        ("cohere/command-r", "🌿", Color::DarkCyan),
        ("vllm/orca-local", "💻", Color::White),
    ];
    for (model, icon, color) in cases {
        assert_eq!(
            LeaderboardManager::get_model_cli_meta(model),
            (icon, color),
            "{model}"
        );
    }
    // Unknown vendor falls back to the robot badge.
    assert_eq!(
        LeaderboardManager::get_model_cli_meta("nobody/mystery"),
        ("🤖", Color::Cyan)
    );
}

#[test]
fn test_language_detection_skips_build_and_hidden_dirs() {
    let temp = std::env::temp_dir().join(format!("test_lang_skip_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(temp.join("target")).unwrap();
    fs::write(temp.join("target/main.rs"), "fn main() {}").unwrap();
    fs::create_dir_all(temp.join(".hidden")).unwrap();
    fs::write(temp.join(".hidden/main.go"), "package main").unwrap();
    fs::create_dir_all(temp.join("node_modules")).unwrap();
    fs::write(temp.join("node_modules/index.py"), "print(1)").unwrap();
    // Every source lives in a skipped directory: no language found.
    assert_eq!(LeaderboardManager::detect_language(&temp), "Unknown");
    // A visible source next to them is still detected.
    fs::write(temp.join("main.js"), "console.log(1)").unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Node.js");
    let _ = fs::remove_dir_all(&temp);

    // Polyglot priority: Rust > Go > C/C++ > Node.js > Python.
    let temp = std::env::temp_dir().join(format!("test_lang_prio_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).unwrap();
    for (name, body) in [
        ("main.py", "print(1)"),
        ("main.js", "1;"),
        ("main.c", "int main(){}"),
        ("main.go", "package main"),
        ("main.rs", "fn main() {}"),
    ] {
        fs::write(temp.join(name), body).unwrap();
    }
    assert_eq!(LeaderboardManager::detect_language(&temp), "Rust");
    fs::remove_file(temp.join("main.rs")).unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Go");
    fs::remove_file(temp.join("main.go")).unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "C/C++");
    fs::remove_file(temp.join("main.c")).unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Node.js");
    fs::remove_file(temp.join("main.js")).unwrap();
    assert_eq!(LeaderboardManager::detect_language(&temp), "Python");
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_load_all_skips_invalid_and_non_json_files() {
    let temp = std::env::temp_dir().join(format!("test_lb_badjson_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).unwrap();
    let mut good = cov_run("good_row");
    good.total_trials = Some(1);
    fs::write(
        temp.join("good_row.json"),
        serde_json::to_string(&good).unwrap(),
    )
    .unwrap();
    fs::write(temp.join("bad.json"), "{ not valid json").unwrap();
    fs::write(temp.join("notes.txt"), "ignored extension").unwrap();
    fs::create_dir_all(temp.join("subdir.json")).unwrap();
    let out = LeaderboardManager::load_all(temp.to_str().unwrap());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, "good_row");
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_merge_repaired_flag_on_converted_repair() {
    let mut a = cov_run("rep_trial1");
    a.status = Some("completed".to_string());
    a.repaired = Some(true);
    let mut b = cov_run("rep_trial2");
    b.status = Some("failed_tests".to_string());
    b.pass_rate = 0.0;
    let m = BenchmarkRunResult::merge_subruns("rep", vec![a, b], None, None).unwrap();
    assert_eq!(m.repaired, Some(true));
    assert_eq!(m.repair_attempted, Some(true));
}

#[test]
fn test_merge_empty_and_missing_path_degradation() {
    assert!(BenchmarkRunResult::merge_subruns("p", Vec::new(), None, None).is_none());
    // A path with no readable files is "Unknown", never a panic.
    assert_eq!(
        LeaderboardManager::detect_language(Path::new("/nonexistent/cov_lang_dir")),
        "Unknown"
    );
    // A results dir that does not exist loads as empty.
    assert!(LeaderboardManager::load_all_from_dir("/nonexistent/cov_results_dir").is_empty());
}

#[test]
fn test_consolidate_without_save_merges_and_stamps_protocol() {
    assert!(LeaderboardManager::consolidate_multi_trial_run("p", &[], false, None, None).is_none());
    let mut s1 = cov_run("cov_cons_trial1");
    s1.protocol = Some("sequential_cap".to_string());
    s1.timestamp = "2026-10-01T00:00:00Z".to_string();
    let mut s2 = cov_run("cov_cons_trial2");
    s2.protocol = Some("sequential_cap".to_string());
    s2.pass_rate = 0.0;
    s2.timestamp = "2026-10-01T00:01:00Z".to_string();
    let subs = vec![s1.clone(), s2.clone()];
    // save_results=false keeps the merge in memory: no repo writes.
    let merged = LeaderboardManager::consolidate_multi_trial_run(
        "cov_cons_parent",
        &subs,
        false,
        Some(false),
        None,
    )
    .unwrap();
    assert_eq!(merged.id, "cov_cons_parent");
    assert_eq!(merged.protocol.as_deref(), Some("sequential_cap"));
    assert_eq!(merged.total_trials, Some(2));
    assert_eq!(
        merged.graded_under,
        Some(crate::report::rules::fingerprint()),
        "consolidation stamps the current grader"
    );
    // An explicit protocol wins; batch drift passes through.
    let merged = LeaderboardManager::consolidate_multi_trial_run(
        "cov_cons_parent",
        &subs,
        false,
        Some(true),
        Some("fixed_n".to_string()),
    )
    .unwrap();
    assert_eq!(merged.protocol.as_deref(), Some("fixed_n"));
    assert_eq!(merged.ref_drift, Some(true));
}
