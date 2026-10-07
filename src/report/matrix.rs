//! Comparison-wave honesty gates (Phase 2: scoring honesty, no reruns).
//!
//! Runs are only comparable within one `rules_version` regime and one
//! uniform effort set. The `matrix` planner refuses to emit mixed waves,
//! and `matrix-report` refuses to rank mixed result sets. The effective-cost
//! formula itself is untouched: these gates only decide what may be planned
//! or compared together.

use std::collections::{BTreeMap, BTreeSet};

use super::leaderboard::{legacy_rules_version, BenchmarkRunResult, LeaderboardManager};

/// Planner gate: one comparison wave requires a uniform effort unless the
/// caller explicitly opts into per-effort-columns analysis
/// (`--per-effort-columns`), where efforts are displayed as separate columns
/// and never pooled into one ranking.
pub fn validate_matrix_plan(efforts: &[String], per_effort_columns: bool) -> anyhow::Result<()> {
    let uniq: BTreeSet<String> = efforts
        .iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect();
    if uniq.is_empty() {
        anyhow::bail!(
            "matrix: no efforts specified — one comparison wave requires a uniform --efforts value (e.g. --efforts high)"
        );
    }
    if uniq.len() > 1 && !per_effort_columns {
        anyhow::bail!(
            "matrix: refusing mixed-effort wave (efforts: {}) — one comparison wave requires uniform effort; rerun with a single --efforts value or pass --per-effort-columns for explicit per-effort-columns analysis",
            uniq.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

/// Comparability key: score stamp, plus the grader stamp when it differs
/// (`graded→scored`). Fresh runs agree; re-scored history groups apart —
/// a DNS row graded 09-27 under pre-echo-zero verifiers never ranks with
/// rows both graded AND scored under the current regime, even after its
/// cost maths were recomputed. Falls back to the legacy timestamp stamp.
pub fn regime_of_version(r: &BenchmarkRunResult) -> String {
    let scored = r
        .rules_version
        .clone()
        .unwrap_or_else(|| legacy_rules_version(&r.timestamp).to_string());
    match r.graded_under.as_deref() {
        Some(g) if g != scored => format!("{g}→{scored}"),
        _ => scored,
    }
}

/// Report gate: refuse to rank a result set that mixes `rules_version`
/// regimes, harnesses, or where models bring mismatched effort sets to one
/// wave. Identical multi-effort sets across every model are fine: that is
/// the explicit per-effort-columns comparison (columns, never pooled).
/// Harnesses are always strict: no per-harness-columns mode exists, filter
/// to one harness before comparing.
pub fn validate_comparison_wave(results: &[BenchmarkRunResult]) -> anyhow::Result<()> {
    if results.is_empty() {
        return Ok(());
    }
    let regimes: BTreeSet<String> = results.iter().map(regime_of_version).collect();
    if regimes.len() > 1 {
        anyhow::bail!(
            "matrix-report: refusing to compare runs across {} rules_version regimes ({}) — runs are only comparable within one rules_version; filter to a single regime before comparing",
            regimes.len(),
            regimes.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    let harnesses: BTreeSet<String> = results
        .iter()
        .map(|r| BenchmarkRunResult::harness_of(r).to_string())
        .collect();
    if harnesses.len() > 1 {
        anyhow::bail!(
            "matrix-report: refusing to compare runs across harnesses ({}) — ranks never mix omp/opencode; filter to a single harness before comparing",
            harnesses.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    let mut per_model: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in results {
        per_model
            .entry(LeaderboardManager::short_model_name(&r.model).to_string())
            .or_default()
            .insert(r.effort.clone().unwrap_or_else(|| "auto".to_string()));
    }
    let sets: BTreeSet<Vec<String>> = per_model
        .values()
        .map(|s| s.iter().cloned().collect::<Vec<_>>())
        .collect();
    if sets.len() > 1 {
        let detail: Vec<String> = per_model
            .iter()
            .map(|(m, s)| {
                format!(
                    "{}: [{}]",
                    m,
                    s.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            })
            .collect();
        anyhow::bail!(
            "matrix-report: refusing to compare mismatched effort sets across models ({}) — one comparison wave requires uniform effort; plan single-effort waves (`matrix --efforts <one>`) and compare within one effort",
            detail.join("; ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave_run(
        id: &str,
        model: &str,
        effort: Option<&str>,
        rules: Option<&str>,
        timestamp: &str,
    ) -> BenchmarkRunResult {
        BenchmarkRunResult {
            id: id.to_string(),
            model: model.to_string(),
            task: "redis".to_string(),
            language: "Python".to_string(),
            effort: effort.map(|s| s.to_string()),
            harness: None,
            pass_rate: 100.0,
            passed_stages: 5,
            total_stages: 5,
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
            effective_cost_usd: Some(0.015),
            duration_seconds: None,
            turns: None,
            savings_percent: 0.0,
            efficiency_score: 0.0,
            throughput_score: None,
            timestamp: timestamp.to_string(),
            method_version: None,
            rules_version: rules.map(|s| s.to_string()),
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
        }
    }

    fn efforts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_plan_uniform_effort_ok() {
        assert!(validate_matrix_plan(&efforts(&["high"]), false).is_ok());
    }

    #[test]
    fn test_plan_mixed_effort_refused_without_flag() {
        let err = validate_matrix_plan(&efforts(&["minimal", "max"]), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("mixed-effort"), "unexpected message: {msg}");
        assert!(
            msg.contains("--per-effort-columns"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn test_plan_mixed_effort_ok_with_explicit_columns_mode() {
        assert!(validate_matrix_plan(&efforts(&["minimal", "max"]), true).is_ok());
    }

    #[test]
    fn test_plan_duplicate_efforts_dedupe_to_uniform() {
        assert!(validate_matrix_plan(&efforts(&["high", "high"]), false).is_ok());
    }

    #[test]
    fn test_plan_empty_efforts_refused() {
        assert!(validate_matrix_plan(&[], false).is_err());
        assert!(validate_matrix_plan(&efforts(&["", "   "]), false).is_err());
    }

    #[test]
    fn test_wave_uniform_ok() {
        let rows = vec![
            wave_run(
                "a1",
                "model-a",
                Some("high"),
                Some("2026-09-24.2+x"),
                "2026-09-25T00:00:00Z",
            ),
            wave_run(
                "b1",
                "model-b",
                Some("high"),
                Some("2026-09-24.2+x"),
                "2026-09-25T00:00:00Z",
            ),
        ];
        assert!(validate_comparison_wave(&rows).is_ok());
    }

    #[test]
    fn test_wave_identical_multi_effort_sets_ok() {
        // Explicit per-effort-columns shape: every model ran both efforts.
        let rows = vec![
            wave_run(
                "a1",
                "model-a",
                Some("minimal"),
                Some("r1"),
                "2026-09-25T00:00:00Z",
            ),
            wave_run(
                "a2",
                "model-a",
                Some("max"),
                Some("r1"),
                "2026-09-25T00:00:00Z",
            ),
            wave_run(
                "b1",
                "model-b",
                Some("minimal"),
                Some("r1"),
                "2026-09-25T00:00:00Z",
            ),
            wave_run(
                "b2",
                "model-b",
                Some("max"),
                Some("r1"),
                "2026-09-25T00:00:00Z",
            ),
        ];
        assert!(validate_comparison_wave(&rows).is_ok());
    }

    #[test]
    fn test_wave_mixed_regimes_refused() {
        let rows = vec![
            wave_run(
                "a1",
                "model-a",
                Some("high"),
                Some("regime-1"),
                "2026-09-25T00:00:00Z",
            ),
            wave_run(
                "b1",
                "model-b",
                Some("high"),
                Some("regime-2"),
                "2026-09-25T00:00:00Z",
            ),
        ];
        let err = validate_comparison_wave(&rows).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("rules_version"), "unexpected message: {msg}");
        assert!(
            msg.contains("regime-1") && msg.contains("regime-2"),
            "message: {msg}"
        );
    }

    #[test]
    fn test_wave_legacy_stamps_split_regimes() {
        // Unstamped runs fall back to the timestamp-era legacy stamp.
        let rows = vec![
            wave_run("a1", "model-a", Some("high"), None, "2026-09-10T00:00:00Z"),
            wave_run("b1", "model-b", Some("high"), None, "2026-09-25T00:00:00Z"),
        ];
        assert!(regime_of_version(&rows[0]) != regime_of_version(&rows[1]));
        assert!(validate_comparison_wave(&rows).is_err());
    }

    #[test]
    fn test_wave_mismatched_effort_sets_refused() {
        let rows = vec![
            wave_run(
                "a1",
                "model-a",
                Some("high"),
                Some("r1"),
                "2026-09-25T00:00:00Z",
            ),
            wave_run(
                "b1",
                "model-b",
                Some("low"),
                Some("r1"),
                "2026-09-25T00:00:00Z",
            ),
        ];
        let err = validate_comparison_wave(&rows).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("effort"), "unexpected message: {msg}");
        assert!(
            msg.contains("model-a") && msg.contains("model-b"),
            "message: {msg}"
        );
    }

    #[test]
    fn test_wave_mixed_harness_refused() {
        let mut a = wave_run(
            "a1",
            "model-a",
            Some("high"),
            Some("r1"),
            "2026-09-25T00:00:00Z",
        );
        let mut b = wave_run(
            "b1",
            "model-b",
            Some("high"),
            Some("r1"),
            "2026-09-25T00:00:00Z",
        );
        a.harness = Some("omp".to_string());
        b.harness = Some("opencode".to_string());
        let err = validate_comparison_wave(&[a, b]).unwrap_err();
        assert!(format!("{err:#}").contains("harness"));
    }

    #[test]
    fn test_wave_empty_ok() {
        assert!(validate_comparison_wave(&[]).is_ok());
    }
}
