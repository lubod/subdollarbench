use anyhow::Result;
use chrono::Utc;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, Color, Row, Table};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRunResult {
    pub id: String,
    pub model: String,
    pub task: String,
    pub language: String,
    pub effort: Option<String>,
    /// Effective effort (see RunManifest): unanimous trials agree or
    /// "mixed" — same rule as cost_source/graded_under.
    #[serde(default)]
    pub effort_effective: Option<String>,
    /// Trial/parent verdict status (`completed`, `failed_tests`,
    /// `disqualified_cheat`, ...). None = legacy rows predating the field.
    /// Boards exclude `disqualified_cheat` (a cheat is not a measurement);
    /// runs pages still show it with its badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Agent harness ("omp" | "opencode"; None = legacy OMP rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub pass_rate: f64,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub throughput_req_sec: Option<f64>,
    #[serde(default)]
    pub reference_throughput_req_sec: Option<f64>,
    #[serde(default)]
    pub baseline_source: Option<String>,
    #[serde(default)]
    pub ref_drift: Option<bool>,
    #[serde(default)]
    pub ref_image: Option<String>,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    /// Cost provenance (`"live"` / `"formula"` / `"formula_zero_delta"`)
    /// and the token-priced value, so the boards can tell a measured zero
    /// from a lagging one (a paid model at $0.00 must not read as free).
    #[serde(default)]
    pub cost_source: Option<String>,
    #[serde(default)]
    pub cost_formula_usd: Option<f64>,
    #[serde(default)]
    pub effective_cost_usd: Option<f64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub turns: Option<u32>,
    pub savings_percent: f64,
    pub efficiency_score: f64,
    #[serde(default)]
    pub throughput_score: Option<f64>,
    pub timestamp: String,
    pub method_version: Option<String>,
    /// Grader provenance vs score stamp (split 2026-10-03.3 review).
    /// `graded_under`: the rule-set whose prompts+verifiers actually
    /// graded this run (what the model faced). `rules_version`: the
    /// rule-set whose scoring maths produced the stored numbers
    /// (re-scored by backfill as maths evolve). Re-scored rows keep
    /// their original `graded_under` and carry the new `rules_version` —
    /// a backfill recomputes cost, it does not re-grade. Runs are only
    /// comparable when BOTH agree; the leaderboard groups by the pair.
    /// Legacy rows: `graded_under` backfilled from the recorded stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_version: Option<String>,
    /// Original grading rule-set; None only on rows predating the split
    /// (backfilled on next backfill pass from the pre-cutover stamp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graded_under: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subruns: Vec<BenchmarkRunResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_trials: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_pass_rate: Option<f64>,
    /// Wilson 95% score interval for the pass fraction (P@1 point estimate).
    /// Lower bound is the ranking key: stops n=3 1.00s outranking n=9 0.89s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_1_lo: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_at_1_hi: Option<f64>,
    /// Trial-stability verdict written by merge_subruns: stable | watch | unstable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stability: Option<String>,
    /// Frontier fraction: this parent's fair throughput / best fair
    /// throughput among same-task parents in the comparison set
    /// (post-pass recompute, same rules_version). Secondary denominator:
    /// the external reference stays primary for cross-wave stability.
    /// None when this row or the best row has no anchored throughput.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_fraction: Option<f64>,
    /// Evaluation protocol (e.g. "sequential_cap" or "fixed_n")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// True if any trial achieved 100% stage pass rate
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solved: Option<bool>,
    /// 1-indexed trial number that first achieved 100% pass (None if unsolved)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solved_at: Option<u32>,
    /// Highest stage pass rate achieved across trials
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_stage_score: Option<f64>,
    /// Whether this run succeeded via feedback repair (true) vs initial attempt (false).
    /// None = unsolved (no trial converted) or unknown. A repair that was
    /// attempted but never converted reads None here — see
    /// `repair_attempted` for the attempt signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repaired: Option<bool>,
    /// True when at least one repair round ran (trial level: this trial ran
    /// ≥1 round; parent level: any scored trial did). False = feedback was
    /// enabled but every scored trial passed or failed on the initial
    /// attempt with zero rounds used. None = feedback disabled or unknown
    /// (pre-field row). Unlike `repaired` (which only reports a converted
    /// repair), this answers "how much did repair run" from the board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_attempted: Option<bool>,
    /// Feedback repair rounds used: trial level = rounds this trial ran;
    /// parent level = max across scored trials. None = feedback disabled or
    /// unknown (pre-field row); Some(0) = enabled but unused. Named
    /// `feedback_rounds` for row compatibility; it has always stored used
    /// rounds, never the configured cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_rounds: Option<u32>,
}

pub fn format_duration(secs: f64) -> String {
    if secs <= 0.0 {
        "-".to_string()
    } else if secs < 60.0 {
        format!("{:.0}s", secs)
    } else {
        let mins = (secs / 60.0).floor() as u64;
        let rem_secs = (secs % 60.0).round() as u64;
        format!("{}m {}s", mins, rem_secs)
    }
}

/// Legacy rule-set stamps for runs recorded before fingerprinting.
/// `legacy-pre-cheat` (<=2026-09-17): no anti-cheat stage; looser grading on
/// pdf/pubsub/proxy. `legacy-cheat` (2026-09-18+): anti-cheat active.
pub const RULES_LEGACY_PRE_CHEAT: &str = "legacy-pre-cheat";
pub const RULES_LEGACY_CHEAT: &str = "legacy-cheat";

/// Stamp a pre-fingerprint timestamp with its legacy rule-set.
pub fn legacy_rules_version(timestamp: &str) -> &'static str {
    if timestamp < "2026-09-18T00:00:00Z" {
        RULES_LEGACY_PRE_CHEAT
    } else {
        RULES_LEGACY_CHEAT
    }
}

/// Tasks quarantined from cross-model ranking while their verifier is under
/// investigation. Ranked within-task only; excluded from the consolidated
/// global board until cleared here. (Empty since the dns task was removed
/// 2026-10-05; the gate stays so a future suspect task has a path.)
pub const QUARANTINED_TASKS: &[&str] = &[];

/// True when a task is quarantined from cross-model ranking.
pub fn is_quarantined(task: &str) -> bool {
    QUARANTINED_TASKS
        .iter()
        .any(|q| q.eq_ignore_ascii_case(task))
}

/// Trial-stability verdict for one consolidated run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stability {
    Stable,
    Watch,
    Unstable,
}

/// Assess whether a multi-trial result is reliable enough to publish.
/// Rules: any pass-rate spread across trials -> Unstable; throughput
/// CV > 25% -> Unstable (> 50% also flags a suspect measurement);
/// CV 10-25% -> Watch; otherwise Stable. Throughput-free tasks
/// are judged on pass spread alone.
/// Unanimous cost provenance across trials, else "mixed" — the same rule the
/// merged row applies to `graded_under`. A parent that cannot say how its
/// dollars were measured has no single story to tell.
fn merge_cost_source(subruns: &[BenchmarkRunResult]) -> Option<String> {
    let first = subruns.first()?.cost_source.clone()?;
    if subruns
        .iter()
        .all(|s| s.cost_source.as_deref() == Some(first.as_str()))
    {
        Some(first)
    } else {
        Some("mixed".to_string())
    }
}

/// Unanimous effective effort across trials, else "mixed" — same rule as
/// cost_source. A parent whose trials ran at different effective levels
/// (e.g. a variant fallback mid-cell) has no single effort story.
fn merge_effort_effective(subruns: &[BenchmarkRunResult]) -> Option<String> {
    let first = subruns.first()?.effort_effective.clone()?;
    if subruns
        .iter()
        .all(|s| s.effort_effective.as_deref() == Some(first.as_str()))
    {
        Some(first)
    } else {
        Some("mixed".to_string())
    }
}

/// Summed token-priced cost across trials (the formula side of the record),
/// present only when every trial carries it.
fn merge_cost_formula(subruns: &[BenchmarkRunResult]) -> Option<f64> {
    if subruns.iter().all(|s| s.cost_formula_usd.is_some()) {
        Some(
            subruns
                .iter()
                .map(|s| s.cost_formula_usd.unwrap_or(0.0))
                .sum(),
        )
    } else {
        None
    }
}

pub fn assess_stability(subruns: &[BenchmarkRunResult]) -> (Stability, String) {
    if subruns.len() < 2 {
        return (Stability::Stable, "single trial".to_string());
    }
    let pr: Vec<f64> = subruns.iter().map(|r| r.pass_rate).collect();
    let lo = pr.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = pr.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if hi - lo > 0.5 {
        return (
            Stability::Unstable,
            format!(
                "pass spread {lo:.0}-{hi:.0}% across {} trials",
                subruns.len()
            ),
        );
    }
    let tps: Vec<f64> = subruns
        .iter()
        .filter_map(|r| r.throughput_req_sec)
        .collect();
    if tps.len() > 1 {
        let mean = tps.iter().sum::<f64>() / tps.len() as f64;
        if mean > 0.0 {
            let var = tps.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / tps.len() as f64;
            let cv = var.sqrt() / mean * 100.0;
            if cv > 25.0 {
                let mut reason = format!("throughput CV {cv:.0}% across {} trials", tps.len());
                if cv > 50.0 {
                    reason.push_str("; suspect measurement, verify probe");
                }
                return (Stability::Unstable, reason);
            }
            if cv > 10.0 {
                return (
                    Stability::Watch,
                    format!("throughput CV {cv:.0}% across {} trials", tps.len()),
                );
            }
        }
    }
    (Stability::Stable, format!("{} trials agree", subruns.len()))
}

/// A trial that carries a real model measurement and enters pass
/// aggregates. `completed` / `failed_tests` are verdicts; None is a legacy
/// row predating the status field (real evidence, must keep ranking —
/// every archived row without a stamp is one of these).
/// Cheat (`disqualified_cheat`) and harness/agent faults (`agent_failed`,
/// `agent_killed`, `infra_error`, unknown futures) are NOT valid: cheats
/// are excluded by the cheat rule, faults are neutral pills by portal
/// doctrine (their stages were never meaningfully measured).
pub fn is_valid_trial(status: Option<&str>) -> bool {
    matches!(status, None | Some("completed") | Some("failed_tests"))
}

/// A parent row may rank only when it summarizes at least one measurement:
/// legacy (None) or an explicit model verdict. Cheat, fault, and future
/// statuses are visible in runs pages but never ranked.
pub fn is_rankable_status(status: Option<&str>) -> bool {
    matches!(status, None | Some("completed") | Some("failed_tests"))
}

impl BenchmarkRunResult {
    pub fn effective_cost(&self) -> f64 {
        self.effective_cost_usd.unwrap_or(self.total_cost_usd)
    }

    /// Cost per solved task: mean trial cost / Pass@1 fraction. Bounded,
    /// monotonic, and directly spendable ("$0.42 per solved redis task"),
    /// unlike pts/¢ which explodes toward infinity as cost → 0. None when
    /// Pass@1 is 0 (nothing solved: cost per solve is undefined, not zero).
    pub fn cost_per_solved(&self) -> Option<f64> {
        let p1 = self.pass_at_1.unwrap_or(self.pass_rate) / 100.0;
        if p1 > 0.0 {
            Some(self.effective_cost() / p1)
        } else {
            None
        }
    }

    /// First 12 hex chars of the digest (or trailing image tag when unpinned).
    fn short_digest(img: &str) -> String {
        let hex = img.rsplit('@').next().unwrap_or(img);
        let hex = hex.strip_prefix("sha256:").unwrap_or(hex);
        hex.chars().take(12).collect()
    }

    /// Harness of a row; None = legacy pre-harness rows, graded as OMP.
    /// Ranks never mix harnesses: frontier keys and the comparison-wave
    /// validator both key on this alongside task + rules_version.
    pub fn harness_of(r: &BenchmarkRunResult) -> &str {
        r.harness.as_deref().unwrap_or("omp")
    }

    /// Measurement regime of a run: short image digest, `+drift` when the
    /// post-batch bracket flagged host movement, `legacy` for runs from before
    /// digest pinning. Same task + same regime = comparable ratios.
    pub fn regime_of(r: &BenchmarkRunResult) -> String {
        match (&r.ref_image, r.ref_drift) {
            (Some(img), Some(true)) => format!("{}+drift", Self::short_digest(img)),
            (Some(img), _) => Self::short_digest(img),
            (None, _) => "legacy".to_string(),
        }
    }

    /// Comparability key: the FULL `{VERSION}+{hash}` score stamp, plus the
    /// grader stamp when it differs. Format: `scored` when grader == scorer
    /// (fresh runs), else `graded→scored`. The short label alone collides
    /// (8 content-distinct hashes sat under `2026-09-30.1`), and a re-score
    /// must not launder grader provenance: a row graded 09-27 under
    /// pre-echo-zero verifiers that was re-scored under `.3` maths groups
    /// apart from rows both graded AND scored under `.3`. `legacy` marks
    /// rows too old to carry a stamp.
    ///
    /// Distinct from `regime_of`, which reports the reference *image*.
    pub fn rules_of(r: &BenchmarkRunResult) -> String {
        let scored = r.rules_version.as_deref().unwrap_or("legacy");
        match r.graded_under.as_deref() {
            Some(g) if g != scored => format!("{g}→{scored}"),
            _ => scored.to_string(),
        }
    }

    /// Merges multiple subruns into a single consolidated BenchmarkRunResult
    /// with averaged metrics, while preserving the individual subrun records.
    /// Canonical best trial: highest pass rate, then highest score
    /// (throughput else efficiency). Single source of truth for the merge
    /// AND review-run's workspace selection — previously they tie-broke
    /// differently (pass_rate-only in review-run) and could read different
    /// trials on tied 100%s. Note: max_by returns the LAST max on ties;
    /// both callers iterate the same subruns order, so selections agree.
    pub fn canonical_best(subruns: &[BenchmarkRunResult]) -> Option<&BenchmarkRunResult> {
        subruns.iter().max_by(|a, b| {
            a.pass_rate
                .partial_cmp(&b.pass_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let s_a = a.throughput_score.unwrap_or(a.efficiency_score);
                    let s_b = b.throughput_score.unwrap_or(b.efficiency_score);
                    s_a.partial_cmp(&s_b).unwrap_or(std::cmp::Ordering::Equal)
                })
        })
    }

    pub fn merge_subruns(
        parent_id: &str,
        subruns: Vec<BenchmarkRunResult>,
        batch_drift: Option<bool>,
        graded_under: Option<String>,
    ) -> Option<BenchmarkRunResult> {
        if subruns.is_empty() {
            return None;
        }
        if subruns.len() == 1 {
            // Unified semantics (P@1/eff for ALL parents): the trial keeps
            // its own pass_rate as headline, but the SCORE is recomputed as
            // base_eff(100%) × P@1 — the same step 6 the n≥2 path runs. The
            // old path returned the trial's pass_rate-scaled score, so a
            // partial-credit n=1 (e.g. 66.7%) outscored a P@1=66.7% merge.
            let mut single = subruns.into_iter().next().unwrap();
            single.id = parent_id.to_string();
            let c = usize::from(single.pass_rate >= 100.0);
            let p1 = if c == 1 { 100.0 } else { 0.0 };
            // Single-trial Wilson: n=1 keeps the lower bound honest so a lone
            // 1/1 (lo ~20.7) ranks below a 9/9 (lo ~70.0) under rank_lo.
            let (w_lo, w_hi) = wilson_interval_95(1, c);
            single.total_trials = Some(1);
            single.pass_at_1 = Some(p1);
            single.pass_at_k = Some(p1);
            single.avg_pass_rate = Some(single.pass_rate);
            single.pass_at_1_lo = Some(w_lo);
            single.pass_at_1_hi = Some(w_hi);
            single.stability = Some("stable".to_string());
            single.solved = Some(c == 1);
            single.solved_at = if c == 1 { Some(1) } else { None };
            single.best_stage_score = Some(single.pass_rate);
            // Recompute the score under unified semantics: base at 100%,
            // scaled by P@1 (0 or 1 here). Keeps the trial's own cost,
            // duration, tokens — only the score formula unifies.
            let eff = single.effective_cost_usd.unwrap_or(single.total_cost_usd);
            single.efficiency_score =
                crate::cost::calculator::ModelPricing::calculate_efficiency_score(100.0, eff)
                    * (p1 / 100.0);
            single.throughput_score = single.throughput_score.map(|_| {
                crate::cost::calculator::ModelPricing::calculate_throughput_score_for_task(
                    100.0,
                    eff,
                    single.throughput_req_sec,
                    single.reference_throughput_req_sec,
                    Some(&single.task),
                )
                .unwrap_or(0.0)
            });
            if single.throughput_score == Some(0.0) {
                single.throughput_score = None;
            }
            return Some(single);
        }

        // Trial-level exclusion: cheats (`disqualified_cheat`) and
        // harness/agent non-results (`agent_failed`, `agent_killed`,
        // `infra_error`, unknown futures) never enter aggregates. A cheat
        // is not a measurement; a fault's stages were never meaningfully
        // measured, so scoring it 0% understates the model (measured live:
        // two archived 9-trial cells read 33.3%/66.7% instead of 50%/75%).
        // Excluded trials stay in `subruns` (visible, auditable) but every
        // mean, ratio, best-trial pick and the P@1 denominator below run
        // over `agg`. Legacy trials (no status) are real evidence and stay.
        // No valid trial (all-cheat / all-fault cells): fall back to the
        // old path over all trials for display continuity, stamped with a
        // non-ranked status so no board can read it as a score.
        let agg_owned: Vec<BenchmarkRunResult> = subruns
            .iter()
            .filter(|r| is_valid_trial(r.status.as_deref()))
            .cloned()
            .collect();
        let agg_empty = agg_owned.is_empty();
        let agg: &[BenchmarkRunResult] = if agg_empty { &subruns } else { &agg_owned };
        let n = agg.len();

        // 1. Conformance & Reliability Metrics
        let passing: Vec<&BenchmarkRunResult> =
            agg.iter().filter(|r| r.pass_rate >= 100.0).collect();
        let c = passing.len();
        let pass_at_1 = (c as f64 / n as f64) * 100.0;
        let pass_at_k = compute_pass_at_k(n, c, n) * 100.0;
        let avg_pass_rate = agg.iter().map(|r| r.pass_rate).sum::<f64>() / n as f64;

        // 2. Canonical best trial (shared helper: same trial review-run reads).
        let best = Self::canonical_best(agg).unwrap_or(&agg[0]);

        // 3. Fair Throughput: median among passing trials, but only when a
        // live/batch reference exists. Unanchored medians (wal/pdf/pubsub
        // without refs) are reported per-trial, never scored.
        let has_ref = best.reference_throughput_req_sec.unwrap_or(0.0) > 0.0;
        // A drifted or unverified bracket means the denominator these trials
        // were divided by is known-wrong, so the *ratio* is wrong. The
        // candidate's own throughput is still a valid measurement and is still
        // reported — but the batch is not anchored, so it is not scored and
        // not ranked, exactly like a task with no reference at all. Before
        // this gate `ref_drift` was a display string and changed no outcome.
        let ref_trusted = has_ref && batch_drift != Some(true);
        // None AND zero readings are no measurement (crashed candidate,
        // unmeasured trial): the median must skip both, not average in a
        // fabricated zero that drags the parent rate down.
        let fair_tp = if c > 0 && has_ref {
            let mut tps: Vec<f64> = passing
                .iter()
                .filter_map(|r| r.throughput_req_sec)
                .filter(|t| *t > 0.0)
                .collect();
            tps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            if tps.is_empty() {
                None
            } else if tps.len() % 2 == 1 {
                Some(tps[tps.len() / 2])
            } else {
                let mid = tps.len() / 2;
                Some((tps[mid - 1] + tps[mid]) / 2.0)
            }
        } else {
            None
        };

        // 4. Fair Cost: mean cost across ALL trials, pass or fail. The old
        // passing-only mean understated real spend (median 1.44x, up to
        // 6.7x): failed trials still burn API + compute. The score already
        // prices failure via the Pass@1 multiplier (step 6); hiding failed
        // spend in the cost column double-counted success.
        let sum_cost: f64 = agg.iter().map(|r| r.total_cost_usd).sum();
        let sum_eff: f64 = agg
            .iter()
            .map(|r| r.effective_cost_usd.unwrap_or(r.total_cost_usd))
            .sum();
        let (fair_cost_usd, fair_eff_cost_usd) = (sum_cost / n as f64, sum_eff / n as f64);

        // 5. Fair Duration, Turns, Tokens & Savings: mean over ALL trials,
        // same denominator as Fair Cost above. A failed trial's compute
        // seconds/tokens feed effective cost, so they must also appear in
        // the duration/token columns — otherwise the row reads
        // "cheap-seconds" for failed runs. No passing-only branch: the
        // score already prices failure via Pass@1 (step 6), and
        // headline passed_stages stays an all-trial mean (step ~397).
        let (fair_dur, fair_turns, fair_prompt, fair_cached, fair_comp, fair_tot_tok, fair_savings) = {
            let dur: f64 = agg.iter().filter_map(|r| r.duration_seconds).sum::<f64>() / n as f64;
            let turns: u32 =
                (agg.iter().filter_map(|r| r.turns).sum::<u32>() as f64 / n as f64).round() as u32;
            let p: u64 = agg.iter().map(|r| r.prompt_tokens).sum::<u64>() / n as u64;
            let ca: u64 = agg.iter().map(|r| r.cached_tokens).sum::<u64>() / n as u64;
            let co: u64 = agg.iter().map(|r| r.completion_tokens).sum::<u64>() / n as u64;
            let tot: u64 = agg.iter().map(|r| r.total_tokens).sum::<u64>() / n as u64;
            let sav: f64 = agg.iter().map(|r| r.savings_percent).sum::<f64>() / n as f64;
            (Some(dur), Some(turns), p, ca, co, tot, sav)
        };

        // 6. Fair Expected Score: Scaled by Pass@1 (Expected Utility).
        // Unanchored tasks (no live/batch ref) keep fair_tp_score None:
        // reported, never ranked. A drifted bracket is unanchored for scoring
        // purposes — `fair_tp` (the candidate's own reading) stays reported.
        // Denominator gate (Step 4): proxy/pdf/wal/pubsub/binproto have
        // no valid throughput denominator. Archived rows predate the scorer
        // gate and may still carry a reference + score; reconsolidation must
        // heal them, not re-publish them. Scrub first, then let the scorer
        // gate decide — belt and suspenders against a future caller that
        // passes a reference for an unscorable task.
        let trusted_reference =
            if crate::cost::calculator::ModelPricing::has_valid_throughput_denominator(Some(
                &best.task,
            )) {
                best.reference_throughput_req_sec
            } else {
                None
            };
        let (fair_eff_score, fair_tp_score): (f64, Option<f64>) = if c > 0 {
            let base_eff = crate::cost::calculator::ModelPricing::calculate_efficiency_score(
                100.0,
                fair_eff_cost_usd,
            );
            let base_tp = if ref_trusted {
                crate::cost::calculator::ModelPricing::calculate_throughput_score_for_task(
                    100.0,
                    fair_eff_cost_usd,
                    fair_tp,
                    trusted_reference,
                    Some(&best.task),
                )
            } else {
                None
            };
            let rel = pass_at_1 / 100.0;
            (base_eff * rel, base_tp.map(|b| b * rel))
        } else {
            (0.0, None)
        };

        let latest_timestamp = subruns
            .iter()
            .map(|r| &r.timestamp)
            .max()
            .cloned()
            .unwrap_or_else(|| Utc::now().to_rfc3339());
        // Honest headline: mean pass rate across trials. Best-of lives in
        // pass_at_k; the canonical workspace still comes from `best`.
        let headline_pass_rate = avg_pass_rate;
        let mean_passed = agg.iter().map(|r| r.passed_stages).sum::<u32>() as f64 / n as f64;
        let headline_passed_stages = mean_passed.round() as u32;
        let (w_lo, w_hi) = wilson_interval_95(n, c);

        let merged_rules_version: Option<String> = {
            let mut vs: Vec<String> = subruns
                .iter()
                .filter_map(|r| r.rules_version.clone())
                .collect();
            vs.sort();
            vs.dedup();
            match vs.len() {
                1 => Some(vs.into_iter().next().unwrap()),
                0 => None,
                _ => Some("mixed".to_string()),
            }
        };
        // Grader provenance: trials carry NO independent grader — a trial is
        // graded when its parent batch runs, so the parent's `graded_under`
        // is the only source of truth. Never derive the parent from trial
        // stamps: the .3 cutover wrote the fresh score stamp into trial
        // `graded_under` (420/420 wrong), and any merge reading them back
        // would launder that error onto the parent. `merge_subruns` takes
        // an explicit grader param; backfill/extend pass the stored parent
        // value through, and a parent without one stays None (honest
        // unknown), never a trial stamp, never the fresh score stamp.
        let merged_graded_under: Option<String> = graded_under;
        // Stability over measured trials: a harness fault is not a model
        // verdict, so it must not tip a cell to unstable. (`agg` falls back
        // to all trials only when nothing measured — display continuity.)
        let (stab, _) = assess_stability(agg);
        let stab_str = match stab {
            Stability::Stable => "stable",
            Stability::Watch => "watch",
            Stability::Unstable => "unstable",
        }
        .to_string();
        // solved_at is the TRIAL NUMBER that first passed, not the position
        // in the aggregate: exclusions shift positions (a fault at trial 1
        // must not make a trial-2 solve read @1). Dense trials from 1 parse
        // to the same value, so legacy cells are unaffected.
        let passing_pos = agg.iter().position(|r| r.pass_rate >= 100.0);
        let solved = passing_pos.is_some();
        let solved_at = passing_pos
            .map(|idx| crate::config::trial_no_of(&agg[idx].id).unwrap_or((idx + 1) as u32));
        let best_stage_score = agg.iter().map(|r| r.pass_rate).fold(0.0, f64::max);
        let any_repaired = agg.iter().any(|r| r.repaired == Some(true));
        // Attempt signal (defect 2): `repaired` only reports a CONVERTED
        // repair, so an attempted-but-failed loop read None — identical to
        // "no repair ran". Any scored trial with ≥1 round used (or a
        // converted repair) marks the cell attempted; all-Some(0) marks
        // enabled-but-unused; anything with unknown rounds stays None.
        let any_repair_attempted = agg
            .iter()
            .any(|r| r.repaired == Some(true) || r.feedback_rounds.unwrap_or(0) > 0);
        let repair_attempted = if any_repair_attempted {
            Some(true)
        } else if !agg.is_empty() && agg.iter().all(|r| r.feedback_rounds.is_some()) {
            Some(false)
        } else {
            None
        };
        let max_feedback_rounds = agg.iter().filter_map(|r| r.feedback_rounds).max();
        let merged_protocol: Option<String> = subruns.iter().find_map(|s| s.protocol.clone());
        Some(BenchmarkRunResult {
            id: parent_id.to_string(),
            model: best.model.clone(),
            task: best.task.clone(),
            language: best.language.clone(),
            effort: best.effort.clone(),
            harness: best.harness.clone(),
            pass_rate: headline_pass_rate,
            passed_stages: headline_passed_stages,
            total_stages: best.total_stages,
            throughput_req_sec: fair_tp,
            reference_throughput_req_sec: trusted_reference,
            baseline_source: if trusted_reference.is_some() {
                best.baseline_source.clone()
            } else {
                None
            },
            ref_drift: batch_drift,
            ref_image: best.ref_image.clone(),
            prompt_tokens: fair_prompt,
            cached_tokens: fair_cached,
            completion_tokens: fair_comp,
            total_tokens: fair_tot_tok,
            total_cost_usd: fair_cost_usd,
            // Cost provenance: unanimous trials agree on the source; a mix
            // says so rather than picking one verdict's story.
            cost_source: merge_cost_source(&subruns),
            effort_effective: merge_effort_effective(&subruns),
            cost_formula_usd: merge_cost_formula(&subruns),
            effective_cost_usd: Some(fair_eff_cost_usd),
            duration_seconds: fair_dur,
            turns: fair_turns,
            savings_percent: fair_savings,
            efficiency_score: fair_eff_score,
            throughput_score: fair_tp_score,
            timestamp: latest_timestamp,
            method_version: best.method_version.clone(),
            rules_version: merged_rules_version,
            graded_under: merged_graded_under,
            status: if agg_empty {
                // No measurable trial: visible, never ranked. All-cheat
                // keeps its verdict; all-fault (or cheat+fault mixes) carry
                // the first fault's status as a neutral pill.
                Some(
                    subruns
                        .iter()
                        .find(|r| {
                            !is_valid_trial(r.status.as_deref())
                                && r.status.as_deref() != Some("disqualified_cheat")
                        })
                        .and_then(|r| r.status.clone())
                        .or_else(|| subruns.iter().find_map(|r| r.status.clone()))
                        .unwrap_or_else(|| "infra_error".to_string()),
                )
            } else {
                None
            },
            subruns,
            total_trials: Some(n as u32),
            pass_at_1: Some(pass_at_1),
            pass_at_k: Some(pass_at_k),
            avg_pass_rate: Some(avg_pass_rate),
            pass_at_1_lo: Some(w_lo),
            pass_at_1_hi: Some(w_hi),
            stability: Some(stab_str),
            // Filled by recompute_frontier_fractions over the comparison
            // set (needs same-task peers); unknown at merge time.
            frontier_fraction: None,
            protocol: merged_protocol,
            solved: Some(solved),
            solved_at,
            best_stage_score: Some(best_stage_score),
            repaired: if any_repaired {
                Some(true)
            } else if solved {
                Some(false)
            } else {
                None
            },
            repair_attempted,
            feedback_rounds: max_feedback_rounds,
        })
    }

    /// Secondary denominator (post-pass over a comparison set): for each
    /// task, divide every parent's fair throughput by the best fair
    /// throughput among same-task, same-rules_version parents that have an
    /// anchored reference. Rows without anchored throughput (or tasks with
    /// no anchored row) keep None. External ref stays primary; this column
    /// answers "who is fastest" without moving the denominator across waves.
    pub fn recompute_frontier_fractions(results: &mut [BenchmarkRunResult]) {
        use std::collections::HashMap;
        let mut best: HashMap<(String, String, String), f64> = HashMap::new();
        for r in results.iter() {
            if r.subruns.is_empty() {
                continue;
            }
            let tp = match (r.throughput_req_sec, r.reference_throughput_req_sec) {
                (Some(t), Some(rt)) if t > 0.0 && rt > 0.0 => t,
                _ => continue,
            };
            let key = (
                r.task.clone(),
                r.rules_version.clone().unwrap_or_default(),
                Self::harness_of(r).to_string(),
            );
            best.entry(key).and_modify(|b| *b = b.max(tp)).or_insert(tp);
        }
        for r in results.iter_mut() {
            let tp = match (r.throughput_req_sec, r.reference_throughput_req_sec) {
                (Some(t), Some(rt)) if t > 0.0 && rt > 0.0 => t,
                _ => {
                    r.frontier_fraction = None;
                    continue;
                }
            };
            let key = (
                r.task.clone(),
                r.rules_version.clone().unwrap_or_default(),
                Self::harness_of(r).to_string(),
            );
            r.frontier_fraction = best.get(&key).map(|b| {
                if *b > 0.0 {
                    (tp / b).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            });
        }
    }
}

/// Wilson 95% score interval for c passes out of n trials, in percent.
/// Point estimate for P@1 is c/n; the interval makes thin cells (n=3)
/// honestly wide. z=1.96 hardcoded: this is a display statistic, not crypto.
pub fn wilson_interval_95(n: usize, c: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let z = 1.96_f64;
    let z2 = z * z;
    let n_f = n as f64;
    let p = c as f64 / n_f;
    let denom = 1.0 + z2 / n_f;
    let center = (p + z2 / (2.0 * n_f)) / denom;
    let half = z * ((p * (1.0 - p) / n_f + z2 / (4.0 * n_f * n_f)).sqrt()) / denom;
    (
        (center - half).max(0.0) * 100.0,
        (center + half).min(1.0) * 100.0,
    )
}

pub fn compute_pass_at_k(n: usize, c: usize, k: usize) -> f64 {
    if n == 0 || c == 0 || k == 0 || c > n || k > n {
        if n > 0 && c == n {
            return 1.0;
        }
        return 0.0;
    }
    if n - c < k {
        return 1.0;
    }
    let mut prod = 1.0;
    for i in 0..k {
        prod *= (n - c - i) as f64 / (n - i) as f64;
    }
    1.0 - prod
}

pub struct LeaderboardManager;

impl LeaderboardManager {
    /// Unit for a task's throughput number.
    ///
    /// Most tasks measure requests per second, but not all, and the units are
    /// not interchangeable: pubsub reports fan-out *deliveries* (200
    /// subscribers x 20 publishes = 4,000 copies from 20 requests), wal
    /// reports fsync-bound messages, binproto reports binary ops. Labelling any
    /// of those "req/s" invites a reader to compare them against a real
    /// request-rate task, which is not a comparison the numbers support.
    pub fn throughput_unit(task: &str) -> &'static str {
        match task.to_lowercase().as_str() {
            "pdf" => "pages/s",
            "wal" => "msgs/s",
            "pubsub" => "deliveries/s",
            "binproto" => "ops/s",
            _ => "req/s",
        }
    }
    /// Detects the candidate implementation language by scanning source file extensions in `workdir`.
    ///
    /// Uses a deterministic priority hierarchy: `Rust` > `Go` > `C/C++` > `Node.js` > `Python`.
    /// Priority ordering ensures deterministic categorization for polyglot workspaces
    /// (e.g. wrapper/build scripts), favoring compiled systems languages over scripting runtimes.
    pub fn detect_language(workdir: &Path) -> String {
        let mut has_c = false;
        let mut has_rust = false;
        let mut has_go = false;
        let mut has_py = false;
        let mut has_js = false;

        Self::scan_extensions(
            workdir,
            &mut has_c,
            &mut has_rust,
            &mut has_go,
            &mut has_py,
            &mut has_js,
        );

        if has_rust {
            "Rust".to_string()
        } else if has_go {
            "Go".to_string()
        } else if has_c {
            "C/C++".to_string()
        } else if has_js {
            "Node.js".to_string()
        } else if has_py {
            "Python".to_string()
        } else {
            "Unknown".to_string()
        }
    }

    fn scan_extensions(
        dir: &Path,
        has_c: &mut bool,
        has_rust: &mut bool,
        has_go: &mut bool,
        has_py: &mut bool,
        has_js: &mut bool,
    ) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') && name != "target" && name != "node_modules" {
                        Self::scan_extensions(&path, has_c, has_rust, has_go, has_py, has_js);
                    }
                } else if path.is_file() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name == "Cargo.toml" || name.ends_with(".rs") {
                        *has_rust = true;
                    } else if name == "go.mod" || name.ends_with(".go") {
                        *has_go = true;
                    } else if name.ends_with(".c")
                        || name.ends_with(".cpp")
                        || name.ends_with(".cc")
                    {
                        *has_c = true;
                    } else if name.ends_with(".py") || name == "requirements.txt" {
                        *has_py = true;
                    } else if name.ends_with(".js")
                        || name.ends_with(".ts")
                        || name == "package.json"
                    {
                        *has_js = true;
                    }
                }
            }
        }
    }

    /// Consolidates subruns of a multi-trial run into a single canonical result,
    /// writing a properly labeled merged manifest.json, preserving the best trial's
    /// workspace and review disclosure, and removing loose trial files.
    pub fn consolidate_multi_trial_run(
        parent_run_id: &str,
        subrun_results: &[BenchmarkRunResult],
        save_results: bool,
        batch_drift: Option<bool>,
    ) -> Option<BenchmarkRunResult> {
        if subrun_results.is_empty() {
            return None;
        }

        // Status sync: trial manifests are authoritative for cheating (old
        // result rows predate the status field). Patch row statuses BEFORE
        // merging so merge_subruns' exclusion sees one consistent verdict;
        // no post-merge override (single mechanism). Missing/unreadable
        // manifest counts as NOT cheat — unknown never nukes a parent.
        let mut synced: Vec<BenchmarkRunResult> = subrun_results.to_vec();
        for sub in synced.iter_mut() {
            let p = crate::config::get_repo_root()
                .join("runs")
                .join(&sub.id)
                .join("manifest.json");
            let manifest_cheat = std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| {
                    v.get("status")
                        .and_then(|x| x.as_str())
                        .map(|x| x.to_string())
                })
                == Some("disqualified_cheat".to_string());
            if manifest_cheat {
                sub.status = Some("disqualified_cheat".to_string());
            }
        }
        let merged = BenchmarkRunResult::merge_subruns(
            parent_run_id,
            synced,
            batch_drift,
            Some(crate::report::rules::fingerprint()),
        )?;

        if save_results {
            let repo_root = crate::config::get_repo_root();
            let results_dir = repo_root.join("results");
            // DB-first (fail-closed): save_result upserts bench.db before
            // touching the export copy. A DB failure aborts here — the
            // export copy and trial cleanup below are skipped — so the
            // merged parent can never exist on FS without its DB row.
            if let Err(e) = Self::save_result(results_dir.to_str().unwrap_or("./results"), &merged)
            {
                eprintln!("[LEADERBOARD] consolidate {parent_run_id}: DB-first save failed: {e:#}");
                return None;
            }

            // Clean up individual trial JSON files from results/ so only the merged parent appears
            for sub in subrun_results {
                let _ = fs::remove_file(results_dir.join(format!("{}.json", sub.id)));
            }
            // Drop matching rows from the primary store so DB reads match the export copies.
            if let Some(store) = Self::open_store_if_exists() {
                for sub in subrun_results {
                    let _ = store.delete_parent(&sub.id);
                }
            }
            // Mark trial subruns hidden so run lists show the consolidated parent
            let trial_runs_dir = crate::report::RunArchiver::resolve_runs_dir();
            for sub in subrun_results {
                let _ = crate::report::portal::set_hidden(&trial_runs_dir, &sub.id, true);
            }

            // Select the best scored trial (highest pass rate, then highest
            // score) — the same canonical_best the merge uses, over the same
            // valid pool. A fault at 0% must never win a tie over a scored
            // 0%: its workspace has no runnable code. Falls back to all
            // trials only when nothing measured (excluded parent anyway).
            let valid_pool: Vec<BenchmarkRunResult> = subrun_results
                .iter()
                .filter(|r| is_valid_trial(r.status.as_deref()))
                .cloned()
                .collect();
            let pool: &[BenchmarkRunResult] = if valid_pool.is_empty() {
                subrun_results
            } else {
                &valid_pool
            };
            let best_sub = BenchmarkRunResult::canonical_best(pool).unwrap_or(&subrun_results[0]);

            let runs_dir = crate::report::RunArchiver::resolve_runs_dir();
            let parent_dir = runs_dir.join(parent_run_id);
            let _ = fs::create_dir_all(&parent_dir);

            let best_sub_dir = runs_dir.join(&best_sub.id);

            // Parent verdict: the merge stamped the reason when nothing
            // measured (all-cheat stays disqualified_cheat, all-fault
            // carries the first fault) — the manifest carries it so runs
            // pages pill it honestly instead of failed_tests. Measured
            // cells take completed/failed_tests from the valid trials; the
            // excluded trials stay listed in subruns with their stamps
            // (visible, excluded from score).
            let n_valid = merged
                .subruns
                .iter()
                .filter(|s| is_valid_trial(s.status.as_deref()))
                .count();

            // 1. Build genuine merged parent manifest (not a trial-1 clone)
            if let Ok(content) = fs::read_to_string(best_sub_dir.join("manifest.json")) {
                if let Ok(mut manifest) =
                    serde_json::from_str::<crate::report::RunManifest>(&content)
                {
                    manifest.run_id = parent_run_id.to_string();
                    manifest.status = match merged.status.as_deref() {
                        // No measurable trial: carry the merge's stamp
                        // (cheat / first fault), never failed_tests — a
                        // harness fault is not a model verdict.
                        Some(s) if !is_rankable_status(Some(s)) => s.to_string(),
                        _ => {
                            if n_valid == 0 {
                                // Belt: merge stamps every all-excluded cell,
                                // so this is unreachable — but an unstamped
                                // no-evidence parent must never read
                                // failed_tests either.
                                merged
                                    .status
                                    .clone()
                                    .unwrap_or_else(|| "infra_error".to_string())
                            } else if merged.avg_pass_rate.unwrap_or(merged.pass_rate) >= 100.0 {
                                "completed".to_string()
                            } else {
                                "failed_tests".to_string()
                            }
                        }
                    };
                    manifest.pass_rate = merged.avg_pass_rate.unwrap_or(merged.pass_rate);
                    manifest.total_trials = merged.total_trials;
                    manifest.pass_at_1 = merged.pass_at_1;
                    manifest.pass_at_1_lo = merged.pass_at_1_lo;
                    manifest.pass_at_1_hi = merged.pass_at_1_hi;
                    manifest.stability = merged.stability.clone();
                    manifest.trial_pass_rates =
                        merged.subruns.iter().map(|s| s.pass_rate).collect();
                    manifest.protocol = merged.protocol.clone();
                    manifest.solved = merged.solved;
                    manifest.solved_at = merged.solved_at;
                    manifest.best_stage_score = merged.best_stage_score;
                    manifest.repaired = merged.repaired;
                    manifest.repair_attempted = merged.repair_attempted;
                    manifest.feedback_rounds = merged.feedback_rounds;
                    manifest.passed_stages = merged.passed_stages;
                    manifest.total_stages = merged.total_stages;
                    manifest.duration_seconds = merged.duration_seconds.unwrap_or(0.0);
                    manifest.turns = merged.turns;
                    manifest.throughput_req_sec = merged.throughput_req_sec;
                    manifest.reference_throughput_req_sec = merged.reference_throughput_req_sec;
                    manifest.baseline_source = merged.baseline_source.clone();
                    manifest.ref_drift = merged.ref_drift;
                    manifest.ref_image = merged.ref_image.clone();
                    manifest.cost_usd = merged.total_cost_usd;
                    manifest.effective_cost_usd = merged.effective_cost_usd;
                    manifest.savings_percent = merged.savings_percent;
                    manifest.efficiency_score = merged.efficiency_score;
                    manifest.throughput_score = merged.throughput_score;
                    manifest.tokens = crate::report::archive::RunTokenUsage {
                        prompt_tokens: merged.prompt_tokens,
                        cached_tokens: merged.cached_tokens,
                        completion_tokens: merged.completion_tokens,
                        total_tokens: merged.total_tokens,
                    };
                    manifest.completed_at = merged.timestamp.clone();
                    if let Ok(m_json) = serde_json::to_string_pretty(&manifest) {
                        let _ = fs::write(parent_dir.join("manifest.json"), m_json);
                    }
                    if let Some(store) = Self::open_store_if_exists() {
                        let _ = store.upsert_run_manifest(&manifest);
                    }
                }
            }

            // 2. Copy env.json
            if let Ok(content) = fs::read_to_string(best_sub_dir.join("env.json")) {
                let _ = fs::write(parent_dir.join("env.json"), content);
            }

            // 3. Review with multi-trial context disclosure
            if let Ok(content) = fs::read_to_string(best_sub_dir.join("review.json")) {
                if let Ok(mut rev_val) = serde_json::from_str::<serde_json::Value>(&content) {
                    let p1 = merged.pass_at_1.unwrap_or(100.0);
                    let pk = merged.pass_at_k.unwrap_or(100.0);
                    let n_all = subrun_results.len();
                    // Honest denominator: scored trials only. A fault trial
                    // at 0% is not a failed measurement, so it leaves both
                    // the passing count and the denominator (it stays named
                    // in the exclusion note, never silently dropped).
                    let n = merged.total_trials.unwrap_or(n_all as u32) as usize;
                    let passing = merged
                        .subruns
                        .iter()
                        .filter(|r| is_valid_trial(r.status.as_deref()) && r.pass_rate >= 100.0)
                        .count();
                    let excl = if n_all > n {
                        format!(", {} excluded non-measurement", n_all - n)
                    } else {
                        String::new()
                    };
                    rev_val["multi_trial_disclosure"] = serde_json::Value::String(format!(
                        "Evaluated best workspace of {} scored subruns ({}/{} passed, Pass@1: {:.0}%, Pass@{}: {:.0}%{}). Best subrun: {}",
                        n, passing, n, p1, n, pk, excl, best_sub.id
                    ));
                    let _ = fs::write(
                        parent_dir.join("review.json"),
                        serde_json::to_string_pretty(&rev_val).unwrap_or(content),
                    );
                } else {
                    let _ = fs::write(parent_dir.join("review.json"), content);
                }
            }

            // 4. Copy workspace from the best trial
            let _ = crate::report::RunArchiver::copy_dir_recursive(
                &best_sub_dir.join("workspace"),
                &parent_dir.join("workspace"),
            );

            // 5. Create multi-trial header in parent console.log
            let total_spent: f64 = subrun_results
                .iter()
                .map(|r| r.effective_cost_usd.unwrap_or(r.total_cost_usd))
                .sum();
            let mut parent_log = format!(
                "=== Multi-Trial Benchmark Execution (Fair Expected Scoring) ===\n                 Model: {}\n                 Task: {}\n                 Subruns: {}\n                 Pass@1: {:.1}%\n                 Pass@{}: {:.1}%\n                 Expected Throughput (Median): {:.0} req/s\n                 Expected Cost (Passing Mean): ${:.4}\n                 Total Multi-Trial Spend: ${:.4}\n                 Canonical Best Workspace: {}\n                 ===============================================================\n\n",
                merged.model,
                merged.task,
                subrun_results.len(),
                merged.pass_at_1.unwrap_or(100.0),
                subrun_results.len(),
                merged.pass_at_k.unwrap_or(100.0),
                merged.throughput_req_sec.unwrap_or(0.0),
                merged.effective_cost_usd.unwrap_or(merged.total_cost_usd),
                total_spent,
                best_sub.id
            );
            if let Ok(best_log) = fs::read_to_string(best_sub_dir.join("console.log")) {
                parent_log.push_str(&best_log);
            }
            let _ = fs::write(parent_dir.join("console.log"), parent_log);

            // 6. Write parent run.json
            let merged_data = serde_json::to_string_pretty(&merged).unwrap_or_default();
            let _ = fs::write(parent_dir.join("run.json"), merged_data);
        }

        Some(merged)
    }

    // Not collapsible: the let-chain form (`if cond && let Some(x) = ...`)
    // requires edition 2024, and this crate is edition 2021. `open_store_if_exists`
    // is side-effect free, so the nesting is purely a language-version limit.
    #[allow(clippy::collapsible_if)]
    pub fn reconsolidate_saved_results(results_dir: &str) -> Result<usize> {
        // DB primary (canonical dir only): iterate stored parents when bench.db exists.
        if Self::use_db(results_dir) {
            if let Some(store) = Self::open_store_if_exists() {
                let mut count = 0;
                for run in store.load_all_parents() {
                    if !run.subruns.is_empty() {
                        if Self::consolidate_multi_trial_run(&run.id, &run.subruns, true, None)
                            .is_some()
                        {
                            count += 1;
                        }
                    }
                }
                return Ok(count);
            }
        }
        let path = Path::new(results_dir);
        let mut count = 0;
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let file_path = entry.path();
                if file_path.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Ok(content) = fs::read_to_string(&file_path) {
                        if let Ok(run) = serde_json::from_str::<BenchmarkRunResult>(&content) {
                            if !run.subruns.is_empty() {
                                if let Some(_merged) = Self::consolidate_multi_trial_run(
                                    &run.id,
                                    &run.subruns,
                                    true,
                                    None,
                                ) {
                                    count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(count)
    }

    pub fn save_result(results_dir: &str, result: &BenchmarkRunResult) -> Result<()> {
        // Publish hygiene: test/CI probe runs (ids starting with `test_`) must
        // never reach the primary store or the export copies. Refuse loudly
        // (warn on stderr, Ok to the caller) so probes stay invisible to the
        // leaderboard, portal, and rollup.
        if result.id.starts_with("test_") {
            eprintln!(
                "[LEADERBOARD] save_result: refusing to publish test run `{}`; skipped",
                result.id
            );
            return Ok(());
        }
        // Fail-closed ordering: the DB upsert runs BEFORE the JSON export
        // copy. On DB error the write aborts before touching the filesystem
        // (loud anyhow context, not a silent skip), so the export copy can
        // never claim a row the primary store rejected. Temp-dir callers
        // (tests, CLI previews) skip the DB so they never touch the shared
        // repo-root store; only the canonical results dir (or an explicit
        // `$SDB_BENCH_DB` override) writes through.
        // `consolidate_multi_trial_run` funnels through here, so it inherits
        // the same DB-first ordering.
        if Self::use_db(results_dir) {
            let db_path = Self::db_path();
            let store = crate::report::Store::open(&db_path).map_err(|e| {
                anyhow::anyhow!(
                    "DB upsert {} failed; export copy not written: {e:#}",
                    result.id
                )
            })?;
            store.upsert_parent(result).map_err(|e| {
                anyhow::anyhow!(
                    "DB upsert {} failed; export copy not written: {e:#}",
                    result.id
                )
            })?;
        }

        let path = Path::new(results_dir);
        fs::create_dir_all(path)?;

        let filename = format!("{}.json", result.id);
        let filepath = path.join(filename);

        let data = serde_json::to_string_pretty(result)?;
        fs::write(filepath, data)?;
        Ok(())
    }

    pub fn load_all(results_dir: &str) -> Vec<BenchmarkRunResult> {
        // DB primary (canonical dir only): one SELECT over parents.
        // Missing/empty DB falls back to the JSON export scan until
        // migration is verified; temp dirs always scan the exports.
        let mut list = if Self::use_db(results_dir) {
            Self::load_all_from_db()
        } else {
            Vec::new()
        };
        if list.is_empty() {
            list = Self::load_all_from_dir(results_dir);
        }
        Self::finalize_results(list)
    }

    /// Primary-store path: shared helper on Store so every `Store::open`
    /// callsite resolves `$SDB_BENCH_DB` override vs repo-root identically.
    fn db_path() -> std::path::PathBuf {
        crate::report::Store::resolve_db_path()
    }

    /// True when `dir` is the canonical repo `results/` dir (absolute, or
    /// relative to a repo-root cwd such as `./results`).
    fn is_canonical_results_dir(dir: &str) -> bool {
        let canonical = crate::config::get_repo_root().join("results");
        let p = Path::new(dir);
        if p == canonical {
            return true;
        }
        if let Ok(cwd) = std::env::current_dir() {
            if cwd.join(p) == canonical {
                return true;
            }
        }
        false
    }

    /// The DB is authoritative only for the canonical results dir (or when
    /// `$SDB_BENCH_DB` is set); temp-dir callers stay on the JSON export
    /// copies so tests never touch the shared repo-root bench.db.
    fn use_db(results_dir: &str) -> bool {
        if Self::is_canonical_results_dir(results_dir) {
            return true;
        }
        matches!(std::env::var("SDB_BENCH_DB"), Ok(p) if !p.trim().is_empty())
    }

    /// Read-only open of the primary store; never creates bench.db.
    pub fn open_store_if_exists() -> Option<crate::report::Store> {
        let db_path = Self::db_path();
        if !db_path.exists() {
            return None;
        }
        crate::report::Store::open(&db_path).ok()
    }

    fn load_all_from_db() -> Vec<BenchmarkRunResult> {
        Self::open_store_if_exists()
            .map(|store| store.load_all_parents())
            .unwrap_or_default()
    }

    fn load_all_from_dir(results_dir: &str) -> Vec<BenchmarkRunResult> {
        let path = Path::new(results_dir);
        let mut list = Vec::new();

        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(data) = fs::read_to_string(p) {
                        if let Ok(res) = serde_json::from_str::<BenchmarkRunResult>(&data) {
                            list.push(res);
                        }
                    }
                }
            }
        }
        list
    }

    /// Shared post-processing for both readers: junk filter, token fallback,
    /// child-subrun filter, loose-trial regroup, ranking sort.
    fn finalize_results(mut list: Vec<BenchmarkRunResult>) -> Vec<BenchmarkRunResult> {
        // Defense in depth: refuse rows that must never publish, even if they
        // reached the store before the write-time guards (killed-run debris,
        // double-spawn orphans, CI probes). Mirrors save_result's refusals.
        let pre = list.len();
        list.retain(|r| {
            if r.id.starts_with("test_") {
                return false;
            }
            // 0-trial junk: loose *trial* rows with no evidence (no subruns,
            // no trial count). Legacy single-trial rows share the shape but
            // carry bare ids (no `_trialN`); they pass through — only `_trial`
            // rows are debris candidates (killed-run/double-spawn leftovers).
            let n = r.total_trials.unwrap_or(r.subruns.len() as u32);
            if n == 0 && r.subruns.is_empty() && r.id.contains("_trial") {
                return false;
            }
            // Legacy single-shot rows (no subruns, no trial count): unscored
            // evidence, never a cell. Zero the score so no consumer can render
            // a pass% for them; the canonical most-trials rule then keeps any
            // real parent ahead, and a phantom-only group draws as empty.
            if r.subruns.is_empty() && r.total_trials.is_none() {
                return false;
            }
            true
        });
        let cut = pre - list.len();
        if cut > 0 {
            eprintln!("[LEADERBOARD] finalize: filtered {cut} junk row(s) (test_/0-trial)");
        }
        // Only measurements rank: cheat verdicts and harness/agent
        // non-results never do (runs pages and the inspector still show
        // them with badges). Genuine cheats stay out; false-positive cures
        // re-enter via repair. Legacy rows (no status) and model verdicts
        // still rank.
        let pre_dq = list.len();
        list.retain(|r| is_rankable_status(r.status.as_deref()));
        let cut_dq = pre_dq - list.len();
        if cut_dq > 0 {
            eprintln!(
                "[LEADERBOARD] finalize: excluded {cut_dq} non-ranked row(s) (cheat/fault verdicts) from ranking"
            );
        }
        for res in &mut list {
            if res.total_tokens == 0 {
                res.total_tokens = res.prompt_tokens + res.cached_tokens + res.completion_tokens;
            }
        }

        // Filter out any child subrun whose ID is already embedded in a parent run
        let mut child_subrun_ids = HashSet::new();
        for r in &list {
            for sub in &r.subruns {
                child_subrun_ids.insert(sub.id.clone());
            }
        }
        list.retain(|r| !child_subrun_ids.contains(&r.id));

        // Group loose trial runs (e.g. ..._trial1_...) into consolidated merged rows
        let mut groups: HashMap<String, Vec<BenchmarkRunResult>> = HashMap::new();
        let mut consolidated = Vec::new();
        let trial_re = regex::Regex::new(r"_trial\d+").unwrap();

        for r in list {
            if r.subruns.is_empty() && r.id.contains("_trial") {
                let group_key = trial_re.replace(&r.id, "").to_string();
                groups.entry(group_key).or_default().push(r);
            } else {
                consolidated.push(r);
            }
        }

        for (parent_id, mut subruns) in groups {
            if subruns.len() > 1 {
                subruns.sort_by(|a, b| a.id.cmp(&b.id));
                // Read-path only: this merged row lives in the in-memory
                // view, nothing writes it back — so graded_under=None here
                // cannot overwrite a stored witness grader. Stored parents
                // (with embedded subruns) never reach this branch: the
                // child-subrun filter above drops their trials first. Do
                // NOT "fix" by persisting this row.
                if let Some(merged) =
                    BenchmarkRunResult::merge_subruns(&parent_id, subruns, None, None)
                {
                    consolidated.push(merged);
                }
            } else {
                consolidated.extend(subruns);
            }
        }

        list = consolidated;
        // Read-path merges above mint fault/cheat-status parents for
        // all-excluded loose groups; ranked boards never show them — same
        // predicate as the stored-row filter above (belt, the pre-regroup
        // filter normally starves such groups first).
        list.retain(|r| is_rankable_status(r.status.as_deref()));
        // Secondary denominator: per-task frontier fractions over this
        // comparison set (same rules_version grouping inside). Runs before
        // the rank sort; unanchored rows keep None.
        BenchmarkRunResult::recompute_frontier_fractions(&mut list);
        // Rank by P@1 lower bound (honest under thin trials), then P@1
        // point, then score. Legacy rows without Wilson fields rank by
        // headline pass rate so old results still order sensibly.
        fn rank_lo(r: &BenchmarkRunResult) -> f64 {
            r.pass_at_1_lo.unwrap_or(r.pass_at_1.unwrap_or(r.pass_rate))
        }
        fn rank_p1(r: &BenchmarkRunResult) -> f64 {
            r.pass_at_1.unwrap_or(r.pass_rate)
        }
        list.sort_by(|a, b| {
            rank_lo(b)
                .partial_cmp(&rank_lo(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    rank_p1(b)
                        .partial_cmp(&rank_p1(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let b_score = b.throughput_score.unwrap_or(b.efficiency_score);
                    let a_score = a.throughput_score.unwrap_or(a.efficiency_score);
                    b_score
                        .partial_cmp(&a_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let a_cost = a.effective_cost();
                    let b_cost = b.effective_cost();
                    a_cost
                        .partial_cmp(&b_cost)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let a_tp = a.throughput_req_sec.unwrap_or(0.0);
                    let b_tp = b.throughput_req_sec.unwrap_or(0.0);
                    b_tp.partial_cmp(&a_tp).unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        list
    }

    /// Pricing-tier badge: `[free]` when the id is a known zero-price model,
    /// `[contrib]` for contributor-discount tiers. Display-only: never touches
    /// scores. The $0 API column measures host patience (compute floor), not
    /// model efficiency — the badge says so at a glance.
    pub fn pricing_badge(model: &str) -> &'static str {
        let m = model.to_lowercase();
        if m.contains("contributor") {
            " [contrib]"
        } else if m.contains("ling-3.1-flash") || m.contains("space-bunny") || m.contains(":free") {
            " [free]"
        } else {
            ""
        }
    }

    pub fn short_model_name(model: &str) -> &str {
        model.rsplit('/').next().unwrap_or(model)
    }

    pub fn get_model_cli_meta(model: &str) -> (&'static str, Color) {
        let m = model.to_lowercase();
        if m.contains("claude") || m.contains("anthropic") {
            ("✦", Color::Red)
        } else if m.contains("gpt")
            || m.contains("openai")
            || m.contains("o1")
            || m.contains("o3")
            || m.contains("o4")
        {
            ("✳️", Color::Green)
        } else if m.contains("gemini") || m.contains("google") {
            ("✨", Color::Blue)
        } else if m.contains("deepseek") {
            ("🐋", Color::Cyan)
        } else if m.contains("llama") || m.contains("meta") {
            ("🦙", Color::Magenta)
        } else if m.contains("qwen") {
            ("🔮", Color::DarkMagenta)
        } else if m.contains("mistral") || m.contains("codestral") || m.contains("pixtral") {
            ("🌪️", Color::DarkYellow)
        } else if m.contains("minimax") {
            ("⚡", Color::DarkRed)
        } else if m.contains("nova") || m.contains("amazon") {
            ("☀️", Color::Yellow)
        } else if m.contains("cohere") || m.contains("command") {
            ("🌿", Color::DarkCyan)
        } else if m.contains("local")
            || m.contains("ollama")
            || m.contains("llama.cpp")
            || m.contains("vllm")
        {
            ("💻", Color::White)
        } else {
            ("🤖", Color::Cyan)
        }
    }

    pub fn print_table(results: &[BenchmarkRunResult]) {
        Self::print_table_with_title("Benchmark Leaderboard", results, false);
    }

    /// `archived`: the rows shown belong to a task pulled from scoring.
    /// The rows are still printed - the archive keeps them - but they lose
    /// their ordinals, because a table that medals a task every model passes
    /// is asserting a ranking that does not exist.
    pub fn print_table_with_title(title: &str, results: &[BenchmarkRunResult], archived: bool) {
        if results.is_empty() {
            println!("\n=== {} ===\nNo results to display.\n", title);
            return;
        }

        if archived {
            println!("\n  ⛔ {} — WITHDRAWN FROM SCORING", title);
            println!("     No rank is shown: this task does not separate models.");
            println!("     Rows below are the archived record, retained for provenance.");
        }

        println!("\n=== {} ===", title);
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                "Rank",
                "Model",
                "Effort",
                "Task",
                "Lang",
                "Pass Rate",
                "Duration",
                "Turns",
                "Throughput",
                "Frontier",
                "Regime",
                "Rules",
                "API cost ($)",
                "Effective cost ($)",
                "$/solved",
                "Cache Savings",
                "Score / ¢",
            ]);

        for (idx, r) in results.iter().enumerate() {
            let rank_str = match archived {
                true => "--".to_string(),
                false => match idx {
                    0 => "🥇 1".to_string(),
                    1 => "🥈 2".to_string(),
                    2 => "🥉 3".to_string(),
                    _ => format!("{}", idx + 1),
                },
            };

            // Throughput is language-relative: the reference is one
            // implementation, so same-language ratios compare and
            // cross-language ones mislead. The Lang column disambiguates.
            let tp = match (r.throughput_req_sec, r.reference_throughput_req_sec) {
                (Some(cand_tp), Some(ref_tp)) if ref_tp > 0.0 => {
                    let ratio = (cand_tp / ref_tp) * 100.0;
                    format!(
                        "{:.0} {} ({:.0}% ref, {})",
                        cand_tp,
                        Self::throughput_unit(&r.task),
                        ratio,
                        r.language
                    )
                }
                // Unanchored timing: reported, never ranked.
                (Some(cand_tp), _) => format!(
                    "{:.0} {} (unscored, {})",
                    cand_tp,
                    Self::throughput_unit(&r.task),
                    r.language
                ),
                _ => "N/A".to_string(),
            };

            // Rank key first: the table sorts by P@1-lo, so the cell
            // leads with the interval, avg second. Matches matrix-report.
            let pass_str = if !r.subruns.is_empty() {
                let avg = r.avg_pass_rate.unwrap_or(r.pass_rate);
                match (r.pass_at_1, r.pass_at_1_lo, r.pass_at_1_hi, r.pass_at_k) {
                    (Some(p1), Some(lo), Some(hi), Some(pk)) => {
                        format!(
                            "P@1 {:.0}% ({:.0}-{:.0}) · avg {:.0}% · P@{} {:.0}%",
                            p1,
                            lo,
                            hi,
                            avg,
                            r.subruns.len(),
                            pk
                        )
                    }
                    (Some(p1), _, _, Some(pk)) => {
                        format!(
                            "P@1 {:.0}% · avg {:.0}% · P@{} {:.0}%",
                            p1,
                            avg,
                            r.subruns.len(),
                            pk
                        )
                    }
                    _ => format!("avg {:.0}% ({} trials)", avg, r.subruns.len()),
                }
            } else {
                format!(
                    "{:.0}% ({}/{})",
                    r.pass_rate, r.passed_stages, r.total_stages
                )
            };

            let (icon, model_color) = Self::get_model_cli_meta(&r.model);
            let short_model = Self::short_model_name(&r.model);
            let badge = Self::pricing_badge(&r.model);
            let model_display = if !r.subruns.is_empty() {
                format!(
                    "{} {}{} [{} trials · Fair E[Score]]",
                    icon,
                    short_model,
                    badge,
                    r.subruns.len()
                )
            } else {
                format!("{} {}{}", icon, short_model, badge)
            };
            let savings_str = format!("{:.0}%", r.savings_percent);
            let eff_str = r.effort.clone().unwrap_or_else(|| "auto".to_string());
            let dur_str = r
                .duration_seconds
                .map(format_duration)
                .unwrap_or_else(|| "-".to_string());
            let turns_str = r
                .turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());

            table.add_row(Row::from(vec![
                Cell::new(rank_str).fg(Color::Yellow),
                Cell::new(&model_display).fg(model_color),
                Cell::new(eff_str).fg(Color::Yellow),
                Cell::new(&r.task),
                Cell::new(&r.language).fg(Color::Green),
                Cell::new(pass_str).fg(if r.avg_pass_rate.unwrap_or(r.pass_rate) >= 100.0 {
                    Color::Green
                } else {
                    Color::Yellow
                }),
                Cell::new(dur_str).fg(Color::White),
                Cell::new(turns_str).fg(Color::Cyan),
                Cell::new(tp),
                Cell::new(match r.frontier_fraction {
                    Some(f) => format!("{:.0}%", f * 100.0),
                    None => "--".to_string(),
                }),
                Cell::new(BenchmarkRunResult::regime_of(r)),
                Cell::new(BenchmarkRunResult::rules_of(r)),
                Cell::new(format!("${:.4}", r.total_cost_usd)).fg(Color::Magenta),
                Cell::new(format!("${:.4}", r.effective_cost())).fg(Color::Magenta),
                Cell::new(match r.cost_per_solved() {
                    Some(c) => format!("${:.4}", c),
                    None => "--".to_string(),
                })
                .fg(Color::Magenta),
                Cell::new(savings_str).fg(Color::Green),
                Cell::new(match r.throughput_score {
                    Some(tp) => format!("{tp:.1}"),
                    // Unanchored (no live ref): efficiency only, never a tp rank.
                    None => format!("-- ({:.1} eff)", r.efficiency_score),
                })
                .fg(Color::Cyan),
            ]));
        }

        // A rank is only meaningful inside one scoring regime. Say so when the
        // table is not, rather than leaving the reader to notice a column they
        // have to know to look at.
        let regimes: std::collections::BTreeSet<String> =
            results.iter().map(BenchmarkRunResult::rules_of).collect();
        if regimes.len() > 1 && !archived {
            let list = regimes.iter().cloned().collect::<Vec<_>>().join(", ");
            println!(
                "\n  ⚠️  This table ranks {} scoring regimes together ({list}).",
                regimes.len()
            );
            println!(
                "     Ranks are not comparable across them; only rows sharing a Rules value are."
            );
        }

        println!("\n{}", table);
    }

    /// Two-way P@1-lower-bound table: model rows x effort columns, per task.
    /// Answers whether the model ranking survives within-effort.
    pub fn print_effort_matrix(results: &[BenchmarkRunResult]) {
        use std::collections::BTreeSet;
        let mut tasks: BTreeSet<String> = BTreeSet::new();
        for r in results {
            tasks.insert(r.task.clone());
        }
        for task in tasks {
            let rows: Vec<&BenchmarkRunResult> =
                results.iter().filter(|r| r.task == task).collect();
            if rows.is_empty() {
                continue;
            }
            let mut models: BTreeSet<String> = BTreeSet::new();
            let mut efforts: BTreeSet<String> = BTreeSet::new();
            for r in &rows {
                models.insert(Self::short_model_name(&r.model).to_string());
                efforts.insert(r.effort.clone().unwrap_or_else(|| "auto".to_string()));
            }
            println!("\n=== Effort x Model: {task} (P@1 lower bound / trials) ===");
            let mut header = format!("{:>22}", "model");
            for e in &efforts {
                header.push_str(&format!(" | {:>16}", e));
            }
            println!("{header}");
            for m in &models {
                let mut line = format!("{:>22}", m);
                for e in &efforts {
                    let cell = rows
                        .iter()
                        .find(|r| {
                            Self::short_model_name(&r.model) == m
                                && r.effort.as_deref().unwrap_or("auto") == e
                        })
                        .map(|r| {
                            let lo = r.pass_at_1_lo.unwrap_or(r.pass_at_1.unwrap_or(r.pass_rate));
                            let n = r.total_trials.unwrap_or(r.subruns.len() as u32);
                            match r.frontier_fraction {
                                Some(f) => format!("{lo:.0} (n={n}, F{:.0}%)", f * 100.0),
                                None => format!("{lo:.0} (n={n})"),
                            }
                        })
                        .unwrap_or_else(|| "-".to_string());
                    line.push_str(&format!(" | {:>16}", cell));
                }
                println!("{line}");
            }
        }
    }

    pub fn print_leaderboard_views(results: &[BenchmarkRunResult], task_filter: &str) {
        let trimmed = task_filter.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
            // Group distinct tasks preserving order
            let mut tasks: Vec<String> = Vec::new();
            for r in results {
                if !tasks.contains(&r.task) {
                    tasks.push(r.task.clone());
                }
            }

            // Print per-task leaderboards first
            for task in &tasks {
                let task_results: Vec<BenchmarkRunResult> = results
                    .iter()
                    .filter(|r| r.task.eq_ignore_ascii_case(task))
                    .cloned()
                    .collect();

                let title = match task.to_lowercase().as_str() {
                    "redis" => "⚡ Task: In-Memory Redis Server Leaderboard",
                    "http" => "🌐 Task: HTTP/1.1 Web Server Leaderboard",
                    _ => &format!("Task: {} Leaderboard", task),
                };
                Self::print_table_with_title(title, &task_results, false);
            }

            // Quarantined tasks rank within-task only, never on the global board.
            let global: Vec<BenchmarkRunResult> = results
                .iter()
                .filter(|r| !is_quarantined(&r.task))
                .cloned()
                .collect();
            let skipped: Vec<String> = {
                let mut t: Vec<String> = results
                    .iter()
                    .filter(|r| is_quarantined(&r.task))
                    .map(|r| r.task.clone())
                    .collect();
                t.sort();
                t.dedup();
                t
            };
            if !skipped.is_empty() {
                println!(
                    "\n(Quarantined from global ranking pending verifier investigation: {} — see per-task board above.)\n",
                    skipped.join(", ")
                );
            }
            Self::print_table_with_title(
                "🏆 Consolidated Global Leaderboard (All Tasks)",
                &global,
                false,
            );
        } else {
            let task_results: Vec<BenchmarkRunResult> = results
                .iter()
                .filter(|r| r.task.eq_ignore_ascii_case(trimmed))
                .cloned()
                .collect();

            if task_results.is_empty() {
                println!("\nNo benchmark results found for task '{}'.\n", trimmed);
            } else {
                let title = match trimmed.to_lowercase().as_str() {
                    "redis" => "⚡ Task: In-Memory Redis Server Leaderboard",
                    "http" => "🌐 Task: HTTP/1.1 Web Server Leaderboard",
                    _ => &format!("Task: {} Leaderboard", trimmed),
                };
                Self::print_table_with_title(title, &task_results, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::docker::{
        BENCH_REDIS_CLIENT_IMAGE, BENCH_WRK_IMAGE, REF_NGINX_IMAGE, REF_REDIS_IMAGE,
    };

    #[test]
    fn test_ref_images_pinned_by_digest() {
        for img in [
            REF_REDIS_IMAGE,
            REF_NGINX_IMAGE,
            BENCH_REDIS_CLIENT_IMAGE,
            BENCH_WRK_IMAGE,
        ] {
            assert!(img.contains("@sha256:"), "unpinned image: {}", img);
        }
    }

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
        pinned.ref_image = Some(REF_REDIS_IMAGE.to_string());
        assert_eq!(
            BenchmarkRunResult::regime_of(&pinned).as_str(),
            &REF_REDIS_IMAGE["docker.io/library/redis@sha256:".len()..][..12]
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
        let kept = BenchmarkRunResult::merge_subruns(
            "p",
            stale_trials,
            None,
            Some("grader-a".to_string()),
        )
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
        let count =
            LeaderboardManager::reconsolidate_saved_results(temp.to_str().unwrap()).unwrap();
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
        let one =
            BenchmarkRunResult::merge_subruns("one", vec![mk("a", 100.0)], None, None).unwrap();
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
        let zero =
            BenchmarkRunResult::merge_subruns("zero", vec![mk("z", 0.0)], None, None).unwrap();
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
}
