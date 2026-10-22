//! Graded scoring and money rules for the campaign ladder.
//!
//! This file is hashed by `report::rules::fingerprint()`. Keep the numbers,
//! point weights and pool maths here; GUI, logging and orchestration edits
//! belong in `campaign.rs` and must not churn the rules stamp.

use std::collections::HashMap;

use crate::campaign::{LadderOutcome, StageSpec};

/// Feedback repair rounds inside one ticket attempt. Campaign work is
/// iterative by nature; one surgical repair round is the default, and a
/// ticket can override it.
pub fn default_feedback_rounds() -> u32 {
    1
}

pub const DEFAULT_TIER: u32 = 1;

pub fn default_tier() -> u32 {
    DEFAULT_TIER
}

pub const DEFAULT_LITE_WEIGHT: f64 = 0.4;

pub fn default_lite_weight() -> f64 {
    DEFAULT_LITE_WEIGHT
}

/// Q1: the phase start may retry this many earlier tickets. The P2 spec
/// said two; the round-19 review found that the common 4 -> 5 -> 6 chain
/// (4 fails, 5/6 skipped) needs three to rescue a career.
pub const CATCH_UP_SLOTS: usize = 3;

/// A catch-up scores 0.4 x tier; it unblocks but never outranks a first pass.
pub const CATCH_UP_WEIGHT: f64 = 0.4;

/// The budget floor (round-10 D3): a stage is not entered when the cap's
/// remaining headroom is below this — the ladder stops as "budget cap"
/// rather than overshooting the cap by one attempt's cost.
pub const BUDGET_FLOOR_USD: f64 = 0.02;

/// The campaign cap keeps the largest single call seen so far in reserve, so
/// a ticket never starts with less than one call's worth of headroom
/// (round-17 P0). The floor covers a cheap attempt when no call has been
/// observed yet.
pub const CAP_RESERVE_USD: f64 = 0.02;

/// Reserve for the next ticket: the largest call so far, or the floor.
pub fn cap_reserve(max_call_usd: f64) -> f64 {
    max_call_usd.max(CAP_RESERVE_USD)
}

/// API headroom after the reserve. `None` when the cell is uncapped.
pub fn cap_headroom(cap_usd: f64, spent_usd: f64, max_call_usd: f64) -> Option<f64> {
    (cap_usd > 0.0).then(|| (cap_usd - spent_usd - cap_reserve(max_call_usd)).max(0.0))
}

/// A ticket starts only when the cap can fund the reserve plus the floor.
pub fn cap_allows(cap_usd: f64, spent_usd: f64, max_call_usd: f64) -> bool {
    cap_usd <= 0.0 || (cap_usd - spent_usd) >= cap_reserve(max_call_usd) + BUDGET_FLOOR_USD
}

/// R23 D2: split the remaining API dollars by weight across the phase's
/// still-undecided tickets. This replaces the old fixed stage budget; the
/// field stays in the schema for archived specs but is not an allocation.
pub fn pool_budget(remaining_usd: f64, weight: f64, weight_sum: f64) -> f64 {
    if remaining_usd <= 0.0 || weight <= 0.0 || weight_sum <= 0.0 {
        0.0
    } else {
        remaining_usd * weight / weight_sum
    }
}

/// R23-1: a ticket whose lite has not run yet weighs the full share plus
/// the lite share, so the full attempt cannot consume the lite's part.
pub fn stage_weight(stage: &StageSpec) -> f64 {
    stage.tier as f64 * (1.0 + stage.lite.as_ref().map(|l| l.weight).unwrap_or(0.0))
}

/// R23-1: the remaining lite share of a ticket after its full attempt.
pub fn stage_lite_weight(stage: &StageSpec) -> f64 {
    stage.tier as f64 * stage.lite.as_ref().map(|l| l.weight).unwrap_or(0.0)
}

/// R23 D2: the weight of the phase's tickets that have no outcome yet and
/// whose needs are not already failed/skipped. R23-1 also excludes a
/// blocked ticket here, so a failed need does not dilute the runnable ones.
pub fn runnable_weight_sum(
    stage_list: &[&StageSpec],
    outcomes: &HashMap<u32, LadderOutcome>,
) -> f64 {
    stage_list
        .iter()
        .filter(|s| !outcomes.contains_key(&s.id))
        .filter(|s| {
            !s.needs.iter().any(|n| {
                matches!(
                    outcomes.get(n),
                    Some(LadderOutcome::Failed) | Some(LadderOutcome::Skipped)
                )
            })
        })
        .map(|s| stage_weight(s))
        .sum()
}

/// Budget and API headroom for a repair round. The round gets only the
/// attempt's remaining API budget and never starts below the floor of
/// either the budget or the cap (round-17 P0; R23 D3 counts API only).
pub fn repair_round_spend(
    budget: f64,
    api_spent: f64,
    cap_left: Option<f64>,
) -> Result<(Option<f64>, Option<f64>), String> {
    if budget.is_finite() {
        let left = budget - api_spent;
        if left < BUDGET_FLOOR_USD {
            return Err(format!(
                "budget floor: ${left:.4} left of ${budget:.4} (floor ${BUDGET_FLOOR_USD:.2})"
            ));
        }
    }
    if let Some(cap) = cap_left {
        if cap < BUDGET_FLOOR_USD {
            return Err(format!(
                "cap floor: ${cap:.4} left (floor ${BUDGET_FLOOR_USD:.2})"
            ));
        }
    }
    let budget_left = if budget.is_finite() {
        Some(budget - api_spent)
    } else {
        None
    };
    Ok((budget_left, cap_left))
}

/// The official sdb-sql cell cap (M5, decided 2026-10-20): the highest
/// budget that keeps every cell under $1.
pub const CELL_BUDGET_USD: f64 = 0.99;
/// R23 D1: turns are only a runaway guard; dollars are the session limit.
pub const TURN_GUARD: u32 = 200;
/// R23 D4: wall time is a guard; the expected stop is dollars.
pub const CELL_WALL_MIN: u64 = 480;
/// R23 D4: one ticket's per-attempt wall guard.
pub const TICKET_WALL_GUARD_MIN: u64 = 120;

/// Points for one stage: tier x (1.0 full | lite.weight lite | 0 else).
pub fn stage_points(stage: &StageSpec, outcome: LadderOutcome) -> f64 {
    let tier = stage.tier as f64;
    match outcome {
        LadderOutcome::Full => tier,
        LadderOutcome::Lite => {
            tier * stage
                .lite
                .as_ref()
                .map(|l| l.weight)
                .unwrap_or(DEFAULT_LITE_WEIGHT)
        }
        LadderOutcome::Failed | LadderOutcome::Skipped => 0.0,
    }
}

/// Maximum achievable points for a spec (every stage full).
pub fn max_points(stages: &[StageSpec]) -> f64 {
    stages.iter().map(|s| s.tier as f64).sum()
}

/// A scored ladder: points, ceiling, and the per-stage rows a rollup
/// reports (profile, tier, points).
pub struct LadderScore {
    pub points: f64,
    pub max_points: f64,
    pub rows: Vec<(u32, u32, LadderOutcome, f64)>,
}

/// Score a ladder from per-stage outcomes (missing ids count as Skipped).
pub fn score_ladder(stages: &[StageSpec], outcomes: &[(u32, LadderOutcome)]) -> LadderScore {
    let by_id: HashMap<u32, LadderOutcome> = outcomes.iter().copied().collect();
    let rows: Vec<(u32, u32, LadderOutcome, f64)> = stages
        .iter()
        .map(|s| {
            let outcome = by_id.get(&s.id).copied().unwrap_or(LadderOutcome::Skipped);
            (s.id, s.tier, outcome, stage_points(s, outcome))
        })
        .collect();
    LadderScore {
        points: rows.iter().map(|r| r.3).sum(),
        max_points: max_points(stages),
        rows,
    }
}
