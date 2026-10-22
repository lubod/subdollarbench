//! Campaign orchestration: ordered tickets on one living workspace.
//!
//! A campaign replaces "one shot, one task" with a sequence of stages run
//! against the same repository: stage i is attempted in the workspace the
//! model already built, and its acceptance is the union of the verifier
//! stages of every ticket up to i. A check that was green at an earlier
//! stage and fails now is a REGRESSION — the cardinal sin of developer
//! work and the reason campaigns exist.
//!
//! This module owns the pure machinery: spec loading/validation, the
//! regression/verdict math, the rollup, and the host-side git-per-stage
//! helper. The stage executor (agent run + verification) is injected by
//! the caller so the orchestration is unit-testable without Docker or
//! spend; `main_campaign.rs` wires the real `BenchmarkPipeline` in.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Feedback repair rounds inside one ticket attempt. Campaign work is
/// iterative by nature; one surgical repair round is the default, and a
/// ticket can override it.
pub fn default_feedback_rounds() -> u32 {
    1
}

/// Verifier stage ids a ticket must pass (redis: 1 handshake, 2 key-value,
/// 3 expiry, 4 counters, 5 pub/sub, 6 AOF). The union over stages 0..i is
/// the cumulative suite at ticket i.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Acceptance {
    #[serde(default)]
    pub redis_stages: Vec<u32>,
    /// sdb-edge (reverse-proxy) acceptance. A campaign gates exactly one
    /// task, so one of the two lists stays empty.
    #[serde(default)]
    pub proxy_stages: Vec<u32>,
    /// sdb-queue (job broker) acceptance.
    #[serde(default)]
    pub queue_stages: Vec<u32>,
    /// sdb-http (file server) acceptance.
    #[serde(default)]
    pub http_stages: Vec<u32>,
    /// sdb-sql (differential SQL database) acceptance.
    #[serde(default)]
    pub sql_stages: Vec<u32>,
}

impl Acceptance {
    /// The stage ids this acceptance gates for `task` (`redis` default).
    /// The suite run at stage `current`: the declared acceptance list
    /// intersected with the stages passed so far, plus `current` itself.
    /// A failed or skipped stage contributes nothing — otherwise one
    /// failure would sink every later ticket (docs/PROPOSAL-sdb-sql.md,
    /// "the ladder continues").
    pub fn accepted_for(declared: &[u32], passed: &[u32], current: u32) -> Vec<u32> {
        declared
            .iter()
            .copied()
            .filter(|s| *s == current || passed.contains(s))
            .collect()
    }

    pub fn stages_for(&self, task: &str) -> &[u32] {
        match task {
            "proxy" => &self.proxy_stages,
            "queue" => &self.queue_stages,
            "http" => &self.http_stages,
            "sql" => &self.sql_stages,
            _ => &self.redis_stages,
        }
    }

    /// Task ids with a campaign stage adapter.
    pub fn supported_task(task: &str) -> bool {
        matches!(task, "redis" | "proxy" | "queue" | "http" | "sql")
    }

    /// Highest acceptance id a campaign of `task` may reference.
    pub fn max_stage(task: &str) -> u32 {
        match task {
            // Proxy stages 6-9 (quick failover, health surface, caching,
            // limits) extend the single-shot 5. The queue suite is 9 stages
            // (the campaign is its primary home).
            "proxy" => 9,
            "queue" => 9,
            // http stages 6-10 (ranges, conditional GET, compression,
            // upload limits, hardening) extend the single-shot 5.
            "http" => 10,
            // sdb-sql: 21 stages (tiers 1-7).
            "sql" => 21,
            _ => 16,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageSpec {
    pub id: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    /// Path to the ticket prompt, relative to the campaign directory.
    pub prompt: String,
    pub acceptance: Acceptance,
    pub budget_usd: f64,
    pub max_turns: u32,
    #[serde(default = "default_feedback_rounds")]
    pub feedback_rounds: u32,
    /// Difficulty tier (sdb-sql: 1..=7). Stage points = tier x weight.
    #[serde(default = "default_tier")]
    pub tier: u32,
    /// Prerequisite stage ids. A stage whose needs include a failed stage is
    /// skipped (and costs nothing). Empty = always runnable.
    #[serde(default)]
    pub needs: Vec<u32>,
    /// Optional lite variant attempted after a failed full attempt (the
    /// workspace rolls back to the last green commit first).
    #[serde(default)]
    pub lite: Option<LiteSpec>,
}

fn default_tier() -> u32 {
    1
}

/// A weaker-but-real variant of a ticket, attempted only after the full
/// attempt fails. Passing it satisfies `needs` but scores `weight` of the
/// stage's points (0.4 by default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiteSpec {
    /// Path to the lite ticket prompt, relative to the campaign directory.
    pub prompt: String,
    /// Feature flags this lite variant's acceptance/profile covers (the
    /// verifier's generator profile; opaque to the orchestrator).
    #[serde(default)]
    pub profile: Vec<String>,
    pub budget_usd: f64,
    pub max_turns: u32,
    #[serde(default = "default_lite_weight")]
    pub weight: f64,
}

fn default_lite_weight() -> f64 {
    0.4
}

/// How the ladder treats a failed ticket. `pass_stop` is the historical
/// single-ladder behavior (the four retired app campaigns); `continue` is
/// sdb-sql's semantics: lite variants, `needs` skipping, and later tickets
/// still run after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LadderMode {
    #[default]
    PassStop,
    Continue,
}

/// One career phase: a contiguous slice of the ladder that runs as its own
/// cell (round-17 P1). Phase ids are 1-based and consecutive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseSpec {
    pub id: u32,
    pub title: String,
    pub stages: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignSpec {
    pub name: String,
    pub version: u32,
    pub task: String,
    /// Ladder semantics (default: pass_stop, the archived app campaigns).
    #[serde(default)]
    pub ladder: LadderMode,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub stages: Vec<StageSpec>,
    /// Optional career split: phases partition the stage ids. Empty for
    /// single-cell campaigns (the archived app suites and pre-P1 specs).
    #[serde(default)]
    pub phases: Vec<PhaseSpec>,
}

#[derive(Debug, Clone)]
pub struct LoadedCampaign {
    pub spec: CampaignSpec,
    pub root: PathBuf,
}

impl LoadedCampaign {
    pub fn prompt_path(&self, stage: &StageSpec) -> PathBuf {
        self.root.join(&stage.prompt)
    }

    /// The harness-owned `SPEC.md` for an attempt at `stage`
    /// (docs/PROPOSAL-sdb-sql.md: "holds the sections unlocked so far, so a
    /// fresh session can always read the rules"). Each session sees only
    /// its own ticket, and tickets after genesis do not restate the launch
    /// contract, protocol or constraints, so the spec is every earlier
    /// ticket in ladder order followed by this attempt's own ticket
    /// (`current_prompt`: the full or the lite text).
    ///
    /// Round-10 D2: a ticket that failed or was skipped is *not* contract.
    /// Its section collapses to a marker line so a later ticket never reads
    /// a failed predecessor as something it must already satisfy. The
    /// `outcomes` list is the ladder's state before this attempt.
    /// Q4: a catch-up's SPEC.md includes every passed ticket, regardless of
    /// its id, with the target's section last.
    pub fn render_spec_md_catch_up(
        &self,
        stage: &StageSpec,
        current_prompt: &Path,
        outcomes: &[(u32, LadderOutcome)],
        passed: &[u32],
    ) -> Result<String> {
        let mut out = format!(
            "# SPEC.md — {} — catch-up of ticket {}\n\n\
             Harness-owned: this catch-up must keep every ticket the career \
             already passed working. All passed contracts appear below, oldest \
             first; the catch-up target is last.\n",
            self.spec.title, stage.id
        );
        fn push(out: &mut String, title: &str, path: &Path) -> Result<()> {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("SPEC.md: read {}", path.display()))?;
            out.push_str(&format!(
                "\n---\n\n<!-- {title} -->\n\n{}\n",
                text.trim_end()
            ));
            Ok(())
        }
        for s in &self.spec.stages {
            if s.id == stage.id || !passed.contains(&s.id) {
                continue;
            }
            let outcome = outcomes.iter().find(|(id, _)| *id == s.id).map(|(_, o)| *o);
            // R3: `passed` is the filter; the label comes from `outcomes`.
            let label = match outcome {
                Some(LadderOutcome::Full) => "full",
                Some(LadderOutcome::Lite) => "lite",
                _ => "passed",
            };
            push(
                &mut out,
                &format!("Ticket {}: {} ({label})", s.id, s.title),
                &self.root.join(&s.prompt),
            )?;
        }
        push(
            &mut out,
            &format!("Ticket {} (catch-up target): {}", stage.id, stage.title),
            current_prompt,
        )?;
        Ok(out)
    }

    pub fn render_spec_md(
        &self,
        stage: &StageSpec,
        current_prompt: &Path,
        outcomes: &[(u32, LadderOutcome)],
    ) -> Result<String> {
        let mut out = format!(
            "# SPEC.md — {}\n\n\
             Harness-owned: rewritten before every ticket, so edits here are \
             discarded. Tickets that passed appear in full, oldest first; a \
             later ticket extends the earlier ones and wins where they \
             differ. A ticket marked FAILED or SKIPPED was not passed: its \
             section is collapsed, it is **not graded and not required**, \
             and no later contract rests on it. The last section is the \
             ticket you are working on now.\n",
            self.spec.title
        );
        fn push(out: &mut String, title: &str, path: &Path) -> Result<()> {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("SPEC.md: read {}", path.display()))?;
            out.push_str(&format!(
                "\n---\n\n<!-- {title} -->\n\n{}\n",
                text.trim_end()
            ));
            Ok(())
        }
        let outcome_of = |id: u32| outcomes.iter().find(|(s, _)| *s == id).map(|(_, o)| *o);
        for s in self.spec.stages.iter().filter(|s| s.id < stage.id) {
            match outcome_of(s.id) {
                Some(LadderOutcome::Full) => push(
                    &mut out,
                    &format!("ticket {}: {} (passed)", s.id, s.title),
                    &self.prompt_path(s),
                )?,
                Some(LadderOutcome::Lite) => push(
                    &mut out,
                    &format!("ticket {}: {} (passed on the lite variant)", s.id, s.title),
                    &self.prompt_path(s),
                )?,
                Some(LadderOutcome::Failed) => out.push_str(&format!(
                    "\n---\n\n<!-- ticket {}: {} - FAILED, rolled back; not graded, not required -->\n\n> Ticket {} ({}) **failed and was rolled back**: it is not graded and not required by this ticket, and the contract below does not include it.\n",
                    s.id, s.title, s.id, s.title
                )),
                Some(LadderOutcome::Skipped) => out.push_str(&format!(
                    "\n---\n\n<!-- ticket {}: {} - SKIPPED, not attempted; not graded, not required -->\n\n> Ticket {} ({}) was **skipped** (a prerequisite ticket did not pass): it is not graded and not required by this ticket.\n",
                    s.id, s.title, s.id, s.title
                )),
                None => out.push_str(&format!(
                    "\n---\n\n<!-- ticket {}: {} - not attempted; not graded, not required -->\n\n> Ticket {} ({}) was **not attempted**: it is not graded and not required by this ticket.\n",
                    s.id, s.title, s.id, s.title
                )),
            }
        }
        push(
            &mut out,
            &format!("ticket {} (current): {}", stage.id, stage.title),
            current_prompt,
        )?;
        Ok(out)
    }
}

/// Format stage ids for prose: `0, 1, 3-7 and 10`.
pub fn fmt_stage_ids(ids: &[u32]) -> String {
    let mut ids: Vec<u32> = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let start = ids[i];
        let mut end = start;
        while i + 1 < ids.len() && ids[i + 1] == end + 1 {
            i += 1;
            end = ids[i];
        }
        parts.push(if end > start {
            format!("{start}-{end}")
        } else {
            format!("{start}")
        });
        i += 1;
    }
    match parts.len() {
        0 => String::new(),
        1 => parts.pop().unwrap_or_default(),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{} and {}", parts.join(", "), last)
        }
    }
}

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

/// Effective headroom after the reserve. `None` when the cell is uncapped.
pub fn cap_headroom(cap_usd: f64, spent_usd: f64, max_call_usd: f64) -> Option<f64> {
    (cap_usd > 0.0).then(|| (cap_usd - spent_usd - cap_reserve(max_call_usd)).max(0.0))
}

/// A ticket starts only when the cap can fund the reserve plus the floor.
pub fn cap_allows(cap_usd: f64, spent_usd: f64, max_call_usd: f64) -> bool {
    cap_usd <= 0.0 || (cap_usd - spent_usd) >= cap_reserve(max_call_usd) + BUDGET_FLOOR_USD
}

/// R23 D2: split the remaining API dollars by tier across the phase's
/// still-undecided tickets. This replaces the old fixed stage budget; the
/// field stays in the schema for archived specs but is not an allocation.
pub fn pool_budget(remaining_usd: f64, tier: u32, weight_sum: u32) -> f64 {
    if remaining_usd <= 0.0 || tier == 0 || weight_sum == 0 {
        0.0
    } else {
        remaining_usd * tier as f64 / weight_sum as f64
    }
}

/// R23 D2: the tier weight of the phase's tickets that have no outcome yet
/// (the current ticket and the later ones not skipped/passed/failed).
pub fn runnable_tier_sum(stage_list: &[&StageSpec], outcomes: &HashMap<u32, LadderOutcome>) -> u32 {
    stage_list
        .iter()
        .filter(|s| !outcomes.contains_key(&s.id))
        .map(|s| s.tier)
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

/// The harness note prepended to an attempt's prompt (round-10 D2, item
/// 11): what earlier tickets failed or were skipped, that the workspace was
/// rolled back, and the suite that actually grades this ticket. Generated
/// from the same `accepted_for` list the verifier runs, so the two cannot
/// drift. `None` for a clean ladder: a ticket after only passes needs no
/// note.
pub fn attempt_preamble(
    stages: &[StageSpec],
    stage: &StageSpec,
    variant: StageVariant,
    rollback_first: bool,
    outcomes: &[(u32, LadderOutcome)],
    accepted: &[u32],
) -> Option<String> {
    let title_of = |id: u32| {
        stages
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.title.as_str())
            .unwrap_or("")
    };
    let broken = |want: LadderOutcome| -> Vec<u32> {
        let mut v: Vec<u32> = outcomes
            .iter()
            .filter(|(id, o)| *id < stage.id && *o == want)
            .map(|(id, _)| *id)
            .collect();
        v.sort_unstable();
        v
    };
    let failed = broken(LadderOutcome::Failed);
    let skipped = broken(LadderOutcome::Skipped);
    let lite_retry = variant == StageVariant::Lite;
    if failed.is_empty() && skipped.is_empty() && !lite_retry {
        return None;
    }
    let named = |ids: &[u32]| -> String {
        let parts: Vec<String> = ids
            .iter()
            .map(|id| format!("{id} ({})", title_of(*id)))
            .collect();
        match parts.len() {
            0 => String::new(),
            1 => parts[0].clone(),
            _ => {
                let last = parts.last().cloned().unwrap_or_default();
                format!("{} and {last}", parts[..parts.len() - 1].join(", "))
            }
        }
    };
    // Round-13 R1: the note is prepended to a *positional* prompt; a
    // leading `-` made both harnesses' parsers read it as an unknown flag
    // (0-turn agents). The note opens with a plain line, never a dash.
    let mut out = String::from("Harness note (added by the benchmark, not the ticket):\n");
    if lite_retry {
        out.push_str(&format!(
            "Your full attempt at ticket {} ({}) failed; its work was rolled back, and this lite variant's shorter contract below is what is graded.\n",
            stage.id, stage.title
        ));
    }
    if !failed.is_empty() {
        out.push_str(&format!(
            "Ticket{} {} failed and {} rolled back; {} not graded and not required by this ticket, and SPEC.md marks it.\n",
            if failed.len() == 1 { "" } else { "s" },
            named(&failed),
            if failed.len() == 1 { "was" } else { "were" },
            if failed.len() == 1 { "it is" } else { "they are" },
        ));
    }
    if !skipped.is_empty() {
        out.push_str(&format!(
            "Ticket{} {} {} skipped (a prerequisite ticket did not pass); {} not graded and not required.\n",
            if skipped.len() == 1 { "" } else { "s" },
            named(&skipped),
            if skipped.len() == 1 { "was" } else { "were" },
            if skipped.len() == 1 { "it is" } else { "they are" },
        ));
    }
    if rollback_first {
        let anchor = outcomes
            .iter()
            .filter(|(id, o)| {
                *id < stage.id && matches!(o, LadderOutcome::Full | LadderOutcome::Lite)
            })
            .map(|(id, _)| *id)
            .max();
        out.push_str(&match anchor {
            Some(id) => format!(
                "The workspace was rolled back to ticket {id}'s commit (the last ticket that passed) before this attempt.\n"
            ),
            None => "The workspace was rolled back to the starting state (no ticket has passed yet) before this attempt.\n".to_string(),
        });
    }
    out.push_str(&format!(
        "This ticket's acceptance suite is tickets {}.\n",
        fmt_stage_ids(accepted)
    ));
    out.push_str("End of harness note.");
    Some(out)
}

/// Stage config handed to `BenchmarkPipeline`: prompt override + workspace
/// continuity + the acceptance subset that decides this stage's verdict.
#[derive(Debug, Clone)]
pub struct CampaignStageConfig {
    pub campaign: String,
    /// Effective cap headroom for this attempt, after the cell's total spend
    /// and the largest-call reserve. None when the campaign is uncapped.
    pub cap_headroom_usd: Option<f64>,
    pub stage: u32,
    pub prompt_path: PathBuf,
    /// true when the workspace already exists and must be preserved
    /// (every stage after genesis).
    pub continuity: bool,
    pub accepted_stages: Vec<u32>,
    /// Feature flags of the lite variant this attempt grades (empty for a
    /// full attempt). Opaque strings; the task's verifier interprets them
    /// (sdb-sql: `crash-acked-only` narrows stage 5's crash ladder).
    pub lite_profile: Vec<String>,
    /// Lite profile of every earlier stage this ladder passed on a lite
    /// variant, by stage (empty for a full run). See [`StageAttempt`].
    pub passed_lite: Vec<(u32, Vec<String>)>,
    /// Every stage passed so far; `accepted_stages` is intersected with it.
    pub passed_stages: Vec<u32>,
    /// sdb-sql: the harness-owned `SPEC.md`, written into the workspace
    /// after the pipeline prepares it (a genesis workspace is wiped first,
    /// so the campaign runner cannot write it beforehand).
    pub spec_md: Option<String>,
    /// Round-10 D2: the harness note for this attempt (failed/skipped
    /// predecessors, the rollback, the effective acceptance suite). The
    /// pipeline prepends it to the ticket prompt.
    pub preamble: Option<String>,
}

/// The campaigns the console and the public board present. The pre-sdb-sql
/// tasks are retired (2026-10-17): only the differential SQL ladder runs,
/// so only its campaign history is listed.
pub const ACTIVE_CAMPAIGNS: &[&str] = &["sdb-sql"];

pub fn campaigns_root() -> PathBuf {
    crate::config::get_repo_root().join("campaigns")
}

pub fn list_campaigns() -> Vec<String> {
    list_campaigns_in(&campaigns_root())
}

pub fn list_campaigns_in(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.path().join("campaign.json").is_file() {
                out.push(e.file_name().to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    out
}

pub fn load_campaign(name: &str) -> Result<LoadedCampaign> {
    load_campaign_from(&campaigns_root(), name)
}

pub fn load_campaign_from(root: &Path, name: &str) -> Result<LoadedCampaign> {
    let dir = root.join(name);
    let path = dir.join("campaign.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let spec: CampaignSpec =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    validate_spec(&spec, &dir)?;
    Ok(LoadedCampaign { spec, root: dir })
}

/// Fail-closed validation: a campaign with a broken spec must never run
/// (the runner would otherwise skip prompts or score the wrong stages).
pub fn validate_spec(spec: &CampaignSpec, root: &Path) -> Result<()> {
    if spec.name.trim().is_empty() {
        bail!("campaign name is empty");
    }
    if spec.stages.is_empty() {
        bail!("campaign '{}' has no stages", spec.name);
    }
    if !Acceptance::supported_task(&spec.task) {
        bail!(
            "campaign '{}': task '{}' has no stage adapter yet (supported: redis, proxy, queue, http, sql)",
            spec.name,
            spec.task
        );
    }
    let max_stage = Acceptance::max_stage(&spec.task);
    if spec.stages[0].id != 0 {
        bail!(
            "campaign '{}': first stage id must be 0 (got {})",
            spec.name,
            spec.stages[0].id
        );
    }
    let mut prev_id: Option<u32> = None;
    for (pos, s) in spec.stages.iter().enumerate() {
        if let Some(p) = prev_id {
            if s.id <= p {
                bail!(
                    "campaign '{}': stage ids must strictly increase (found {} after {} at position {pos})",
                    spec.name,
                    s.id,
                    p
                );
            }
        }
        prev_id = Some(s.id);
        if s.title.trim().is_empty() {
            bail!(
                "campaign '{}': stage {} has an empty title",
                spec.name,
                s.id
            );
        }
        if s.budget_usd <= 0.0 {
            bail!(
                "campaign '{}': stage {} budget must be > 0",
                spec.name,
                s.id
            );
        }
        if s.max_turns == 0 {
            bail!(
                "campaign '{}': stage {} max_turns must be > 0",
                spec.name,
                s.id
            );
        }
        let a = s.acceptance.stages_for(&spec.task);
        if a.is_empty() {
            bail!(
                "campaign '{}': stage {} has empty acceptance (task '{}' list)",
                spec.name,
                s.id,
                spec.task
            );
        }
        let mut seen = HashSet::new();
        // The sdb-sql ladder is 0-based (ticket 0 is genesis and the
        // differential stages carry the same ids); every other suite
        // numbers its stages from 1. Both span `max_stage` stages, so the
        // 0-based ladder (21 tickets) ends at 20.
        let (min_stage, max_stage) = if spec.task == "sql" {
            (0, max_stage.saturating_sub(1))
        } else {
            (1, max_stage)
        };
        for id in a {
            if !(min_stage..=max_stage).contains(id) {
                bail!(
                    "campaign '{}': stage {} acceptance id {} out of range {}..={} (task '{}')",
                    spec.name,
                    s.id,
                    id,
                    min_stage,
                    max_stage,
                    spec.task
                );
            }
            if !seen.insert(*id) {
                bail!(
                    "campaign '{}': stage {} repeats acceptance id {}",
                    spec.name,
                    s.id,
                    id
                );
            }
        }
        // Tiers and needs (sdb-sql ladder semantics): a need must name an
        // earlier, defined stage; the tier weights the stage's points.
        if s.tier == 0 {
            bail!("campaign '{}': stage {} tier must be >= 1", spec.name, s.id);
        }
        let defined: HashSet<u32> = spec.stages.iter().map(|st| st.id).collect();
        for need in &s.needs {
            if !defined.contains(need) {
                bail!(
                    "campaign '{}': stage {} needs unknown stage {}",
                    spec.name,
                    s.id,
                    need
                );
            }
            if *need >= s.id {
                bail!(
                    "campaign '{}': stage {} needs {} which is not an earlier stage",
                    spec.name,
                    s.id,
                    need
                );
            }
        }
        if let Some(lite) = &s.lite {
            if lite.budget_usd <= 0.0 {
                bail!(
                    "campaign '{}': stage {} lite budget must be > 0",
                    spec.name,
                    s.id
                );
            }
            if lite.max_turns == 0 {
                bail!(
                    "campaign '{}': stage {} lite max_turns must be > 0",
                    spec.name,
                    s.id
                );
            }
            if !(lite.weight > 0.0 && lite.weight < 1.0) {
                bail!(
                    "campaign '{}': stage {} lite weight must be in (0,1), got {}",
                    spec.name,
                    s.id,
                    lite.weight
                );
            }
            let lp = root.join(&lite.prompt);
            if !lp.is_file() {
                bail!(
                    "campaign '{}': stage {} lite prompt missing: {}",
                    spec.name,
                    s.id,
                    lp.display()
                );
            }
        }
        let pp = root.join(&s.prompt);
        if !pp.is_file() {
            bail!(
                "campaign '{}': stage {} prompt missing: {}",
                spec.name,
                s.id,
                pp.display()
            );
        }
    }
    // Career phases (P1): a partition of the ladder, contiguous and in
    // order; no stage may need a ticket from a later phase.
    if !spec.phases.is_empty() {
        let defined: HashSet<u32> = spec.stages.iter().map(|s| s.id).collect();
        let mut seen: HashSet<u32> = HashSet::new();
        let mut last_phase_stage: Option<u32> = None;
        for (pos, phase) in spec.phases.iter().enumerate() {
            let want_id = (pos + 1) as u32;
            if phase.id != want_id {
                bail!(
                    "campaign '{}': phases must be numbered 1..=n in order (found id {} at position {pos}, wanted {want_id})",
                    spec.name,
                    phase.id
                );
            }
            if phase.title.trim().is_empty() {
                bail!(
                    "campaign '{}': phase {} has an empty title",
                    spec.name,
                    phase.id
                );
            }
            if phase.stages.is_empty() {
                bail!("campaign '{}': phase {} has no stages", spec.name, phase.id);
            }
            for id in &phase.stages {
                if !defined.contains(id) {
                    bail!(
                        "campaign '{}': phase {} names unknown stage {}",
                        spec.name,
                        phase.id,
                        id
                    );
                }
                if !seen.insert(*id) {
                    bail!(
                        "campaign '{}': stage {} appears in more than one phase",
                        spec.name,
                        id
                    );
                }
                if let Some(prev) = last_phase_stage {
                    if *id <= prev {
                        bail!(
                            "campaign '{}': phase stages must be contiguous and in order (stage {} after {})",
                            spec.name,
                            id,
                            prev
                        );
                    }
                }
                last_phase_stage = Some(*id);
            }
            for st in spec.stages.iter().filter(|s| phase.stages.contains(&s.id)) {
                for need in &st.needs {
                    let need_phase = spec
                        .phases
                        .iter()
                        .find(|p| p.stages.contains(need))
                        .map(|p| p.id)
                        .unwrap_or(0);
                    if need_phase > phase.id {
                        bail!(
                            "campaign '{}': stage {} (phase {}) needs stage {} from later phase {}",
                            spec.name,
                            st.id,
                            phase.id,
                            need,
                            need_phase
                        );
                    }
                }
            }
        }
        if seen.len() != defined.len() {
            bail!(
                "campaign '{}': phases do not partition the stages ({} of {} covered)",
                spec.name,
                seen.len(),
                defined.len()
            );
        }
    }
    Ok(())
}

/// Q4: the acceptance of a catch-up is the target's declared list plus
/// every ticket the career already passed. Sorted and deduplicated.
pub fn catch_up_accepted(mut declared: Vec<u32>, passed: &[u32]) -> Vec<u32> {
    declared.extend(passed.iter().copied());
    declared.sort_unstable();
    declared.dedup();
    declared
}

/// Points ceiling for one phase: the sum of its stages' tiers.
pub fn phase_max_points(phase: &PhaseSpec, stages: &[StageSpec]) -> f64 {
    phase
        .stages
        .iter()
        .filter_map(|id| stages.iter().find(|s| s.id == *id))
        .map(|s| s.tier as f64)
        .sum()
}

/// The phase that owns a stage id, if the spec has phases.
pub fn phase_of(spec: &CampaignSpec, stage: u32) -> Option<&PhaseSpec> {
    spec.phases.iter().find(|p| p.stages.contains(&stage))
}

/// R1: any cell that names `chain_id` in `chain_from` uses up the chain.
/// This counts running, cancelled, crashed and unfinished cells; only a
/// harness-faulted cell (a non-result) is ignored. It reads the rollup when
/// present and the status-level marker (written at cell creation) when a
/// cell died before its first stage.
pub fn chain_already_used(runs_dir: &std::path::Path, chain_id: &str) -> Option<String> {
    let rd = std::fs::read_dir(runs_dir).ok()?;
    // S2: a chain cell named by any non-faulted cell's refork.of is used
    // too (the old branch was superseded), and the successor chosen for the
    // error/refork record is the newest by started_at, not the first
    // read_dir hit.
    let mut candidates: Vec<(String, String)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name == chain_id || !name.starts_with("campaign_") || !e.path().is_dir() {
            continue;
        }
        let read = |f: &str| {
            std::fs::read_to_string(e.path().join(f))
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        };
        let campaign = read("campaign.json");
        let status = read("status.json");
        let rollup = campaign.or_else(|| {
            status
                .as_ref()
                .and_then(|s| s.get("rollup").cloned().filter(|r| !r.is_null()))
        });
        let str_at = |v: Option<&serde_json::Value>| -> Option<String> {
            v.and_then(|x| x.as_str()).map(str::to_string)
        };
        let chain_from = str_at(rollup.as_ref().and_then(|r| r.get("chain_from")))
            .or_else(|| str_at(status.as_ref().and_then(|s| s.get("chain_from"))))
            .or_else(|| str_at(status.as_ref().and_then(|s| s.pointer("/live/chain_from"))));
        let refork_of = str_at(rollup.as_ref().and_then(|r| r.pointer("/refork/of")))
            .or_else(|| str_at(status.as_ref().and_then(|s| s.pointer("/refork/of"))))
            .or_else(|| str_at(status.as_ref().and_then(|s| s.pointer("/live/refork/of"))));
        if chain_from.as_deref() != Some(chain_id) && refork_of.as_deref() != Some(chain_id) {
            continue;
        }
        let faulted = rollup
            .as_ref()
            .and_then(|r| r.get("harness_fault"))
            .filter(|v| !v.is_null())
            .is_some()
            || status
                .as_ref()
                .and_then(|s| s.get("harness_fault"))
                .filter(|v| !v.is_null())
                .is_some()
            || status
                .as_ref()
                .and_then(|s| s.pointer("/rollup/harness_fault"))
                .filter(|v| !v.is_null())
                .is_some();
        if faulted {
            continue;
        }
        let started_at = str_at(rollup.as_ref().and_then(|r| r.get("started_at")))
            .or_else(|| str_at(status.as_ref().and_then(|s| s.get("started_at"))))
            .or_else(|| str_at(status.as_ref().and_then(|s| s.pointer("/live/started_at"))))
            .unwrap_or_default();
        candidates.push((started_at, name));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    candidates.into_iter().next().map(|(_, name)| name)
}

/// R1 (D2): decide whether this run may proceed given the chain's existing
/// successor. A successor without `refork` is refused with the existing id;
/// `refork` without `--chain` or with an empty reason is refused; a refork
/// on an unused chain is refused.
pub fn resolve_refork(
    successor: Option<&str>,
    chain: Option<&str>,
    refork: Option<&str>,
) -> Result<Option<ReforkInfo>, String> {
    if refork.is_some() && chain.is_none() {
        return Err("--refork requires --chain".to_string());
    }
    let reason = refork.unwrap_or("").trim();
    if refork.is_some() && reason.is_empty() {
        return Err("--refork needs a non-empty reason".to_string());
    }
    match (successor, refork) {
        (Some(other), None) => Err(format!(
            "chain cell {} was already continued by {other}; rerun with --refork \"<reason>\" only for a deliberate fork",
            chain.unwrap_or("?")
        )),
        (Some(other), Some(_)) => Ok(Some(ReforkInfo {
            of: other.to_string(),
            reason: reason.to_string(),
        })),
        (None, Some(_)) => Err(format!(
            "chain cell {} is not used by any existing cell; drop --refork",
            chain.unwrap_or("?")
        )),
        (None, None) => Ok(None),
    }
}

/// Validate a career chain and return its ladder state (P1). The previous
/// cell must be the immediately preceding phase, the same model/harness/
/// effort/rules, finished, and carry a ladder state.
pub fn validate_chain(
    prev: &CampaignRollup,
    state: Option<&LadderState>,
    phase: u32,
    model: &str,
    harness: &str,
    effort: &str,
    rules_version: &str,
) -> Result<LadderState> {
    if phase <= 1 {
        bail!("phase 1 has no chain");
    }
    if prev.phase != Some(phase - 1) {
        bail!(
            "chain cell {} is phase {:?}, expected phase {}",
            prev.campaign_id,
            prev.phase,
            phase - 1
        );
    }
    if prev.model != model {
        bail!(
            "chain cell {} used model {}, not {}",
            prev.campaign_id,
            prev.model,
            model
        );
    }
    if prev.harness != harness || prev.effort != effort {
        bail!(
            "chain cell {} used {} / {}, not {} / {}",
            prev.campaign_id,
            prev.harness,
            prev.effort,
            harness,
            effort
        );
    }
    if prev.rules_version != rules_version {
        bail!(
            "chain cell {} ran rules {}, not {}",
            prev.campaign_id,
            prev.rules_version,
            rules_version
        );
    }
    if prev.finished_at.trim().is_empty() {
        bail!("chain cell {} did not finish", prev.campaign_id);
    }
    if prev.harness_fault.is_some() {
        bail!(
            "chain cell {} is a non-result (harness fault) and cannot be continued",
            prev.campaign_id
        );
    }
    state
        .or(prev.ladder_state.as_ref())
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "chain cell {} has no ladder state (run it with P1 or newer)",
                prev.campaign_id
            )
        })
}

/// Career totals carried into the next phase; older cells with no career
/// block start from their own phase totals.
pub fn career_from(prev: &CampaignRollup) -> CareerRollup {
    prev.career.clone().unwrap_or_else(|| CareerRollup {
        points: prev.points,
        total_cost_usd: prev.total_cost_usd,
        cells: vec![prev.campaign_id.clone()],
    })
}

/// Severity weight for a verifier stage id: protocol/keyspace/durability
/// regressions (handshake, key-value, counters, AOF) outweigh feature ones.
pub fn regression_severity(stage_id: u32) -> u32 {
    match stage_id {
        1 | 2 | 4 | 6 => 3,
        3 | 5 => 2,
        _ => 1,
    }
}

/// Regressions in `results` against the best-known state (`best`: stage id
/// -> ever passed). Returned ids are sorted and unique.
pub fn detect_regressions(
    best: &HashMap<u32, bool>,
    results: &[crate::verifier::StageResult],
) -> (Vec<u32>, u32) {
    let mut ids: Vec<u32> = results
        .iter()
        // A scenario that never ran because an earlier stage failed is not a
        // regression of that scenario: the failing earlier stage is already
        // in the report (round-15 L1).
        .filter(|r| {
            !r.passed
                && !r.error.as_deref().is_some_and(|e| {
                    e.starts_with("not run:")
                        || e.starts_with("skipped due to failure in earlier stages")
                })
                && best.get(&r.stage).copied().unwrap_or(false)
        })
        .map(|r| r.stage)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let severity = ids
        .iter()
        .map(|id| regression_severity(*id))
        .max()
        .unwrap_or(0);
    (ids, severity)
}

/// Which variant of a ticket is being attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageVariant {
    Full,
    Lite,
}

/// One attempt the orchestrator asks the executor to perform. The executor
/// owns prompt paths, budgets and the workspace; the orchestrator owns the
/// ladder decisions (including when a rollback is required first).
#[derive(Debug, Clone)]
pub struct StageAttempt<'a> {
    pub stage: &'a StageSpec,
    pub variant: StageVariant,
    /// Prompt file for this variant (lite prompts differ).
    pub prompt_path: std::path::PathBuf,
    pub budget_usd: f64,
    pub max_turns: u32,
    /// Feature flags of the lite variant (`LiteSpec.profile`), empty for a
    /// full attempt: the verifier uses them to grade the lite suite.
    pub lite_profile: Vec<String>,
    /// Lite profile of every *earlier* stage this ladder passed on a lite
    /// variant, by stage: a later ticket's suite must keep grading those
    /// stages at the variant the model actually passed, or it would demand
    /// work that ticket-5-lite never asked for.
    pub passed_lite: Vec<(u32, Vec<String>)>,
    /// Every stage passed so far (full or lite). The suite at stage k is
    /// the *passed* stages' suites plus k itself: a stage the model failed
    /// or skipped contributes nothing, or one failure would sink every
    /// later ticket (docs/PROPOSAL-sdb-sql.md, "the ladder continues").
    pub passed: Vec<u32>,
    /// Ladder state before this attempt, by stage id (round-10 D2): drives
    /// the collapsed SPEC.md sections and the rollback note.
    pub outcomes: Vec<(u32, LadderOutcome)>,
    /// Score weight of a pass (1.0 full, the lite spec's weight otherwise).
    pub weight: f64,
    /// R23 D3: API-bill headroom after the reserve. None when uncapped.
    /// The compute floor is reported only, never a stopping condition.
    pub cap_headroom_usd: Option<f64>,
    /// P2: the harness hint block for the phase's attempts (recorded
    /// failures and blocked tickets); None outside a chained phase.
    pub hints: Option<String>,
    /// P2/Q4: a catch-up grades the declared list plus every passed ticket
    /// (the target may be far behind the current phase).
    pub catch_up: bool,
    /// True when the workspace/data must be rolled back to the last green
    /// state before this attempt (a predecessor failed; lite attempts after
    /// a failed full always roll back).
    pub rollback_first: bool,
}

/// One stage's raw outcome, mapped from `PipelineOutput` by the executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageOutcome {
    pub run_id: String,
    pub passed: bool,
    pub pass_rate: f64,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub stage_results: Vec<crate::verifier::StageResult>,
    pub effective_cost_usd: f64,
    /// Provider-billed API spend at list price. R23 D3: the campaign cap
    /// and the ticket pool count this number only.
    pub api_cost_usd: f64,
    pub turns: Option<u32>,
    pub duration_seconds: Option<f64>,
    /// The largest single provider call this attempt made; the campaign cap
    /// reserves it before the next ticket (round-17 P0).
    #[serde(default)]
    pub max_call_usd: f64,
    /// R23 D1/D5: why the agent session ended: dollars | turns | wall |
    /// ready | crash. None on eval-only/short paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub agent_failed: bool,
    pub infra_error: bool,
    pub disqualified: bool,
    pub git_commit: Option<String>,
    pub git_stat: Option<String>,
    /// Every verification round of this attempt (initial + repairs).
    pub rounds: Vec<RoundRecord>,
    /// The classified cause of the final round when the attempt failed.
    pub failure: Option<FailureSummary>,
    /// Round-14 N2: the attempt measured nothing because of the harness or
    /// the provider (the agent never started, a billing/auth trip). The
    /// ladder stops here and the cell is a non-result.
    #[serde(default)]
    pub harness_fault: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRecord {
    pub id: u32,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub passed: bool,
    pub pass_rate: f64,
    pub passed_stages: u32,
    pub total_stages: u32,
    /// Verifier stage ids that were green earlier and failed now.
    pub regressed: Vec<u32>,
    pub regression_severity: u32,
    pub effective_cost_usd: f64,
    pub turns: Option<u32>,
    pub duration_seconds: Option<f64>,
    /// passed | failed | fault (harness/provider fault, not a model verdict)
    /// | skipped (a `needs` prerequisite failed)
    pub status: String,
    /// R23 D1/D5: why the agent session ended: dollars | turns | wall |
    /// ready | crash. None on eval-only/short paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// full | lite (which variant produced this record).
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub tier: u32,
    /// Weighted points this stage scored (tier x weight when passed).
    #[serde(default)]
    pub points: f64,
    pub run_id: String,
    pub git_commit: Option<String>,
    pub git_stat: Option<String>,
    /// Every verification round of this attempt (initial + repairs), in
    /// order. Empty on records written before the field existed; the GUI
    /// then derives one round from the summary.
    #[serde(default)]
    pub rounds: Vec<RoundRecord>,
    /// Classified one-line cause when the attempt failed (None when passed).
    #[serde(default)]
    pub failure: Option<FailureSummary>,
}

/// One feedback round of a campaign attempt (gui-spec.md §5.1): the initial
/// try (round 0) and every repair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundRecord {
    pub round: u32,
    pub passed_stages: u32,
    pub total_stages: u32,
    pub turns: Option<u32>,
    /// Equivalent compute cost of this round's own tokens; the attempt's
    /// `effective_cost_usd` stays the authoritative number.
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub git_commit: Option<String>,
    #[serde(default)]
    pub failure: Option<FailureSummary>,
    /// True when the round was reconstructed on read (an old cell whose
    /// manifests predate recorded rounds), never recorded live.
    #[serde(default)]
    pub derived: bool,
}

/// The classified one-line cause of a failed ticket or round
/// (gui-spec.md §5.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureSummary {
    /// fault | no_start | regression | late_check | own_stage | agent |
    /// agent_crash (max-output truncation after grading)
    pub kind: String,
    /// The verifier stage that failed first, when the round has one.
    #[serde(default)]
    pub stage: Option<u32>,
    /// One line, <= 160 chars, no newlines.
    pub line: String,
    /// The full verifier error (reproduction), when available.
    #[serde(default)]
    pub detail: String,
    /// Candidate stderr tail when the server never started.
    #[serde(default)]
    pub stderr_tail: Option<String>,
}

/// `"5 of 6 stages passed (83.3%)"` -> `(5, 6)`.
fn parse_of_rest(rest: &str) -> Option<(u32, u32)> {
    let (p, t) = rest.split_once(" of ")?;
    let passed = p.trim().parse::<u32>().ok()?;
    let total = t
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse::<u32>()
        .ok()?;
    Some((passed, total))
}

/// One line, at most `n` chars (round-10 D5): the GUI's failure line and
/// the live block's `first_fail` both use this, so the Live card can never
/// hold a multi-line verifier repro.
pub fn truncate_line(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim().to_string();
    if line.chars().count() <= n {
        line
    } else {
        let cut: String = line.chars().take(n.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// The first stderr-looking error line of a candidate log tail, with its
/// `File "...", line N` context when present (`IndentationError server.py:101`).
fn first_error_line(tail: &str) -> Option<String> {
    let strip = |l: &str| -> String {
        // Console lines carry a `[HH:MM:SS] ` prefix and `| ` continuation.
        let l = l.trim();
        let l = match l.strip_prefix('[') {
            Some(rest) => rest.split_once("] ").map(|(_, r)| r).unwrap_or(l),
            None => l,
        };
        l.trim().trim_start_matches('|').trim().to_string()
    };
    let lines: Vec<String> = tail.lines().map(strip).collect();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("Error:") {
            continue;
        }
        // `File "/app/./server.py", line 101` on the line just above.
        let place = lines[..i].iter().rev().find_map(|l| {
            let l = l.as_str();
            let rest = l.strip_prefix("File \"")?;
            let file = rest.split('"').next()?;
            let line_no = rest.split("line ").nth(1)?.trim();
            let base = std::path::Path::new(file)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| file.to_string());
            Some(format!("{base}:{line_no}"))
        });
        let msg = line.as_str();
        return Some(match place {
            Some(p) => format!("{p}: {msg}"),
            None => msg.to_string(),
        });
    }
    None
}

/// Classify why an attempt round failed (gui-spec.md §5.2): the first
/// matching rule wins. `was_green` is the set of stages that had passed
/// before the attempt; `stage_results` are that round's verifier results.
pub fn classify_failure(
    stage_id: u32,
    was_green: &[u32],
    stage_results: &[crate::verifier::StageResult],
    agent_failed: bool,
    infra_error: bool,
    candidate_logs_tail: Option<&str>,
) -> Option<FailureSummary> {
    let failed: Vec<&crate::verifier::StageResult> =
        stage_results.iter().filter(|s| !s.passed).collect();
    // Results the verifier skipped ("skipped due to failure in earlier
    // stages") and late checks the pipeline did not run ("not run: ...")
    // only follow the real break; they are counted, not blamed (round-7
    // finding 3; round-18 N6: since `.28` the late-checked stages 5 and 6
    // read "not run" whenever a later stage fails, and were blamed).
    let is_skipped = |s: &crate::verifier::StageResult| {
        s.error
            .as_deref()
            .is_some_and(|e| e.starts_with("skipped due to failure") || e.starts_with("not run:"))
    };
    let first = failed
        .iter()
        .copied()
        .find(|s| !is_skipped(s))
        .or_else(|| failed.first().copied());
    if failed.is_empty() && !agent_failed && !infra_error {
        return None;
    }
    let detail = first.and_then(|s| s.error.clone()).unwrap_or_default();
    let stage = first.map(|s| s.stage);
    let tail = candidate_logs_tail.map(str::to_string);
    let mk = |kind: &str, stage: Option<u32>, line: String| FailureSummary {
        kind: kind.to_string(),
        stage,
        line: truncate_line(&line, 160),
        detail: detail.clone(),
        stderr_tail: tail.clone(),
    };
    // 1. fault: a harness or provider fault, never a model verdict.
    if infra_error {
        return Some(mk("fault", stage, "harness or provider fault".to_string()));
    }
    // 2. no_start: stage 0 could not be reached.
    if let Some(s0) = stage_results.iter().find(|s| s.stage == 0) {
        if !s0.passed
            && s0
                .error
                .as_deref()
                .map(|e| e.starts_with("server unreachable"))
                .unwrap_or(false)
        {
            let line = tail
                .as_deref()
                .and_then(first_error_line)
                .map(|e| format!("server didn't start: {e}"))
                .unwrap_or_else(|| "server didn't start".to_string());
            return Some(mk("no_start", Some(0), line));
        }
    }
    // 3. regression: an earlier green ticket fails now; skipped results
    // are not blamed (round-7 finding 3: ticket 6 broke stage 0, not
    // stages 0,1,3,4,5).
    let real: Vec<&crate::verifier::StageResult> =
        failed.iter().copied().filter(|s| !is_skipped(s)).collect();
    let regressed: Vec<u32> = real
        .iter()
        .filter(|s| s.stage < stage_id && was_green.contains(&s.stage))
        .map(|s| s.stage)
        .collect();
    if !regressed.is_empty() {
        let list = regressed
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let not_run: Vec<u32> = failed
            .iter()
            .copied()
            .filter(|s| is_skipped(s) && s.stage < stage_id && was_green.contains(&s.stage))
            .map(|s| s.stage)
            .collect();
        let what = real
            .first()
            .copied()
            .or(first)
            .and_then(|s| s.error.as_deref())
            .map(|e| e.lines().next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
        let ticket = if regressed.len() == 1 {
            "ticket"
        } else {
            "tickets"
        };
        let line = match (what.is_empty(), not_run.is_empty()) {
            (false, false) => format!(
                "broke {ticket} {list} ({} not run): {what}",
                not_run
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            (false, true) => format!("broke {ticket} {list}: {what}"),
            (true, false) => format!(
                "broke {ticket} {list} ({} not run)",
                not_run
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            (true, true) => format!("broke {ticket} {list}"),
        };
        return Some(mk("regression", stage, line));
    }
    // 4. late_check: a pipeline late failure carries its tag.
    if let Some(tagged) = failed.iter().find_map(|s| {
        s.error.as_deref().filter(|e| {
            e.starts_with("[crash]") || e.starts_with("[perf]") || e.starts_with("[upgrade]")
        })
    }) {
        return Some(mk("late_check", stage, tagged.to_string()));
    }
    // 5. own_stage: only this ticket's stage failed.
    if !real.is_empty() && real.iter().all(|s| s.stage == stage_id) {
        let what = first
            .and_then(|s| s.error.as_deref())
            .map(|e| e.lines().next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
        let line = if what.is_empty() {
            "own stage failed".to_string()
        } else {
            format!("own stage: {what}")
        };
        return Some(mk("own_stage", stage, line));
    }
    // 6. agent: the agent died or never signalled READY.
    if agent_failed {
        return Some(mk(
            "agent",
            stage,
            "agent stopped without READY".to_string(),
        ));
    }
    // A mixed shape no rule names: report the ticket's own stage when it
    // failed, else the first failing stage, honestly.
    let own = failed.iter().find(|s| s.stage == stage_id).copied();
    let what = own
        .or(first)
        .and_then(|s| s.error.as_deref())
        .map(|e| e.lines().next().unwrap_or("").trim().to_string())
        .unwrap_or_else(|| "failed".to_string());
    Some(mk("own_stage", own.map(|s| s.stage).or(stage), what))
}

/// F2: annotate a classified agent failure with the max-output truncation
/// when the host console shows it. Returns whether the record changed.
pub fn mark_agent_crash(failure: &mut FailureSummary, console: &str) -> bool {
    let truncation = console.contains("output: 65536")
        || console.contains("output limit")
        || console.contains("not valid JSON");
    if !truncation {
        return false;
    }
    failure.kind = "agent_crash".to_string();
    failure.line = "agent hit the 65,536-token output limit and crashed".to_string();
    true
}

/// The campaign lock (round-7 finding 1): one campaign at a time, whether
/// started from the CLI (`scripts/sql-cell.sh`) or the console. A small
/// JSON file holding `{pid, campaign_id}`; a lock whose holder is gone is
/// stale and may be replaced. Every campaign path takes it, so the GUI,
/// the queue and the CLI all agree on who is running.
pub fn campaign_lock_path() -> std::path::PathBuf {
    crate::report::archive::RunArchiver::resolve_runs_dir().join(".campaign.lock")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub campaign_id: String,
    /// The holder's process start time from /proc/<pid>/stat (field 22).
    /// A reused pid gets a different start time, so the lock goes stale
    /// instead of pinning an unrelated process (round-8 finding 5). Zero
    /// for locks written before the field existed.
    #[serde(default)]
    pub start_ticks: u64,
}

/// `/proc/<pid>/stat` field 22 (starttime in clock ticks), parsed after the
/// last ')' because the comm field may contain spaces.
fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// A lock holder is live when its pid still has the *same* start time it
/// had at acquisition. An old lock without the stamp falls back to a
/// cmdline check: anything but a campaign process does not own it.
fn holder_alive(info: &LockInfo) -> bool {
    let Some(ticks) = process_start_ticks(info.pid) else {
        return false;
    };
    if info.start_ticks == 0 {
        return std::fs::read_to_string(format!("/proc/{}/cmdline", info.pid))
            .map(|c| c.contains("campaign"))
            .unwrap_or(false);
    }
    ticks == info.start_ticks
}

/// The live campaign-lock holder, if any. A dead holder's lock is ignored
/// (and will be replaced by the next `acquire`).
pub fn live_lock_holder() -> Option<LockInfo> {
    live_lock_holder_at(&campaign_lock_path())
}

/// The same check against an explicit path (tests use a temp file).
pub fn live_lock_holder_at(path: &std::path::Path) -> Option<LockInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let info: LockInfo = serde_json::from_str(&text).ok()?;
    holder_alive(&info).then_some(info)
}

/// The lock guard: held for the whole campaign run; removed on drop.
#[derive(Debug)]
pub struct CampaignLock {
    path: std::path::PathBuf,
    held: bool,
}

impl CampaignLock {
    /// Take the lock for this process, or name the live holder.
    pub fn acquire(campaign_id: &str) -> Result<CampaignLock, String> {
        Self::try_acquire_at(&campaign_lock_path(), campaign_id)
    }

    /// The same acquisition against an explicit path (tests use a temp
    /// file so parallel tests never touch the real lock).
    pub fn try_acquire_at(
        path: &std::path::Path,
        campaign_id: &str,
    ) -> Result<CampaignLock, String> {
        let path = path.to_path_buf();
        for _ in 0..3 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write as _;
                    let info = LockInfo {
                        pid: std::process::id(),
                        campaign_id: campaign_id.to_string(),
                        start_ticks: process_start_ticks(std::process::id()).unwrap_or(0),
                    };
                    let _ =
                        f.write_all(serde_json::to_string(&info).unwrap_or_default().as_bytes());
                    return Ok(CampaignLock { path, held: true });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(holder) = live_lock_holder_at(&path) {
                        return Err(format!(
                            "campaign {} (pid {}) is already running — if that process is gone, delete {}",
                            holder.campaign_id,
                            holder.pid,
                            path.display()
                        ));
                    }
                    // Stale lock: replace it.
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => return Err(format!("cannot take the campaign lock: {e}")),
            }
        }
        Err("cannot take the campaign lock (contended)".to_string())
    }
}

impl Drop for CampaignLock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The state a cell is shown as (round-7 finding 6): a `running` status
/// whose writer is gone (no live lock) and which has not been updated for
/// 30 minutes is `stalled`, never running forever.
pub fn effective_state(campaign_id: &str, state: &str, updated_at: Option<&str>) -> String {
    // Only *this* cell's lock keeps it running: another campaign holding
    // the lock says nothing about a cell that died yesterday (round-8
    // finding 4).
    let lock_live = live_lock_holder()
        .map(|h| h.campaign_id == campaign_id)
        .unwrap_or(false);
    effective_state_with(state, updated_at, lock_live)
}

/// The same derivation with the lock fact passed in, so a test is not at
/// the mercy of a real lock file.
pub fn effective_state_with(state: &str, updated_at: Option<&str>, lock_live: bool) -> String {
    if state != "running" {
        return state.to_string();
    }
    let fresh = updated_at
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_minutes() < 30)
        .unwrap_or(false);
    if fresh || lock_live {
        "running".to_string()
    } else {
        "stalled".to_string()
    }
}

/// Fill `rounds`/`failure` on records written before they were recorded
/// (gui-spec.md §6.1): read each attempt's manifest from the campaign dir
/// and mark the derived rounds. Cells with live rounds are untouched.
pub fn backfill_rounds(rollup: &mut CampaignRollup, campaign_dir: &std::path::Path) {
    for i in 0..rollup.stages.len() {
        if !rollup.stages[i].rounds.is_empty() {
            continue;
        }
        // Stages green before this record: the passed records earlier in
        // the rollup, which is in attempt order.
        let was_green: Vec<u32> = rollup.stages[..i]
            .iter()
            .filter(|r| r.passed)
            .map(|r| r.id)
            .collect();
        let (id, variant) = (rollup.stages[i].id, rollup.stages[i].variant.clone());
        // Lenient: old manifests may predate fields RunManifest now
        // requires, and the backfill must still read what is there.
        let manifest: Option<serde_json::Value> = std::fs::read_to_string(
            campaign_dir
                .join(format!("s{id}_{variant}"))
                .join("manifest.json"),
        )
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
        let agent_failed = manifest
            .as_ref()
            .map(|m| m["status"].as_str() == Some("agent_failed"))
            .unwrap_or(rollup.stages[i].status == "fault");
        // F2: the F2-era campaign records `failed` for a graded agent crash.
        // Old rows wrote `fault`; correct them on read when the cell has no
        // real harness fault. A real fault (billing, never-started) keeps
        // the cell-level harness_fault and the fault label.
        if agent_failed && rollup.harness_fault.is_none() && rollup.stages[i].status == "fault" {
            rollup.stages[i].status = "failed".to_string();
        }
        let stages_of = |m: &serde_json::Value| -> Vec<crate::verifier::StageResult> {
            m.get("stages")
                .and_then(|s| serde_json::from_value(s.clone()).ok())
                .unwrap_or_default()
        };
        let recorded_rounds: Vec<crate::report::archive::CampaignRound> = manifest
            .as_ref()
            .and_then(|m| m.get("rounds"))
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default();
        let mut rounds: Vec<RoundRecord> = Vec::new();
        if !recorded_rounds.is_empty() {
            for r in &recorded_rounds {
                let failure = classify_failure(
                    id,
                    &was_green,
                    &r.stage_results,
                    agent_failed,
                    false,
                    r.candidate_logs_tail.as_deref(),
                );
                rounds.push(RoundRecord {
                    round: r.round,
                    passed_stages: r.passed_stages,
                    total_stages: r.total_stages,
                    turns: r.turns,
                    cost_usd: r.cost_usd,
                    duration_seconds: r.duration_seconds,
                    git_commit: None,
                    failure,
                    derived: true,
                });
            }
        } else if let Some(m) = &manifest {
            // A manifest from before rounds existed. The stage console.log
            // still carries each round's score ("Initial attempt passed 5
            // of 6", "Round 1 test result: 2 of 6"); recover the rounds
            // from it, then classify the final round from the manifest's
            // stage results.
            let stages = stages_of(m);
            let console = std::fs::read_to_string(
                campaign_dir
                    .join(format!("s{id}_{variant}"))
                    .join("console.log"),
            )
            .unwrap_or_default();
            // The console tail is the best stderr evidence an old cell has
            // (the manifest predates candidate_logs_tail).
            let console_tail: String = console
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            let final_failure = classify_failure(
                id,
                &was_green,
                &stages,
                agent_failed,
                false,
                (!console_tail.trim().is_empty()).then_some(console_tail.as_str()),
            );
            let mut scores: Vec<(u32, u32, u32)> = Vec::new();
            for line in console.lines() {
                let Some(at) = line.find("[FEEDBACK] ") else {
                    continue;
                };
                let rest = &line[at + "[FEEDBACK] ".len()..];
                if let Some(rest) = rest.strip_prefix("Initial attempt passed ") {
                    if let Some((p, t)) = parse_of_rest(rest) {
                        scores.push((0, p, t));
                    }
                } else if let Some(rest) = rest.strip_prefix("Round ") {
                    if let Some((round, tail)) = rest.split_once(" test result: ") {
                        if let (Ok(r), Some((p, t))) =
                            (round.trim().parse::<u32>(), parse_of_rest(tail))
                        {
                            scores.push((r, p, t));
                        }
                    }
                }
            }
            if !scores.is_empty() {
                let last = scores.len() - 1;
                for (n, (round, passed, total)) in scores.iter().enumerate() {
                    rounds.push(RoundRecord {
                        round: *round,
                        passed_stages: *passed,
                        total_stages: *total,
                        turns: None,
                        cost_usd: 0.0,
                        duration_seconds: None,
                        git_commit: None,
                        failure: (n == last).then(|| final_failure.clone()).flatten(),
                        derived: true,
                    });
                }
            } else {
                // No console either: one derived round with the final score.
                let rec = &rollup.stages[i];
                rounds.push(RoundRecord {
                    round: 0,
                    passed_stages: m["passed_stages"]
                        .as_u64()
                        .map(|v| v as u32)
                        .unwrap_or(rec.passed_stages),
                    total_stages: m["total_stages"]
                        .as_u64()
                        .map(|v| v as u32)
                        .unwrap_or(rec.total_stages),
                    turns: m["turns"].as_u64().map(|v| v as u32).or(rec.turns),
                    cost_usd: 0.0,
                    duration_seconds: m["duration_seconds"].as_f64().or(rec.duration_seconds),
                    git_commit: None,
                    failure: final_failure,
                    derived: true,
                });
            }
        }
        if rounds.is_empty() {
            // No manifest on disk: the record's own summary is all we have.
            let rec = &rollup.stages[i];
            rounds.push(RoundRecord {
                round: 0,
                passed_stages: rec.passed_stages,
                total_stages: rec.total_stages,
                turns: rec.turns,
                cost_usd: rec.effective_cost_usd,
                duration_seconds: rec.duration_seconds,
                git_commit: rec.git_commit.clone(),
                failure: None,
                derived: true,
            });
        }
        let failure = rounds.last().and_then(|r| r.failure.clone());
        rollup.stages[i].rounds = rounds;
        if rollup.stages[i].failure.is_none() {
            rollup.stages[i].failure = failure;
        }
    }
}

/// One ticket on the ladder, attempted or not (gui-spec.md §4.1, §5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadderEntry {
    pub id: u32,
    pub title: String,
    /// pass | lite | fail | fault | skipped | waiting | not_reached
    pub status: String,
    /// full | lite: the variant of the record that decided the status.
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub points: f64,
    /// R23 D5: why the agent ended (dollars/turns/wall/ready/crash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// One-line cause for the Run view; None when the ticket passed.
    #[serde(default)]
    pub cause: Option<String>,
}

/// A rollup-shaped stub for a running cell that has no rollup yet (its
/// first ticket is still in flight; round-8 finding 8). The live block the
/// runner writes carries the model, harness, effort and budgets.
pub fn live_rollup_stub(campaign_id: &str, v: &serde_json::Value) -> CampaignRollup {
    let live = &v["live"];
    let s = |k: &str, fb: &str| live[k].as_str().unwrap_or(fb).to_string();
    CampaignRollup {
        campaign_id: campaign_id.to_string(),
        name: s("campaign", "sdb-sql"),
        model: s("model", "?"),
        harness: s("harness", "omp"),
        effort: s("effort", "high"),
        task: "sql".to_string(),
        started_at: v["started_at"].as_str().unwrap_or("").to_string(),
        finished_at: String::new(),
        stages: Vec::new(),
        reached: None,
        regression_free_tickets: 0,
        total_cost_usd: live["campaign_spent"].as_f64().unwrap_or(0.0),
        over_budget: false,
        cap_reached: false,
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        budget_usd: live["campaign_budget"].as_f64().unwrap_or(0.0),
        wall_min: v["wall_cap_min"].as_u64().unwrap_or(0),
        points: 0.0,
        max_points: 0.0,
        // S3: `live.phase` is the activity ("agent", "verifying"); the
        // career phase is `career_phase` (a numeric `phase` is the
        // pre-S3 creation write).
        phase: v["live"]["career_phase"]
            .as_u64()
            .or_else(|| v["career_phase"].as_u64())
            .or_else(|| v["live"]["phase"].as_u64())
            .map(|p| p as u32),
        chain_from: v["live"]["chain_from"]
            .as_str()
            .or_else(|| v["chain_from"].as_str())
            .map(str::to_string),
        career: None,
        refork: if v["live"]["refork"].is_null() {
            None
        } else {
            serde_json::from_value(v["live"]["refork"].clone()).ok()
        },
        ladder_state: None,
        harness_fault: None,
    }
}

/// The official sdb-sql cell cap (M5, decided 2026-10-20): the highest
/// budget that keeps every cell under $1. The `.29` calibration cell hit
/// the old $0.75 cap at ticket 19 after 71 of 120 min; a full ladder costs
/// about $1.03 at its rate, so reaching the top stays part of the score.
pub const CELL_BUDGET_USD: f64 = 0.99;
/// R23 D1: turns are only a runaway guard; dollars are the session limit.
pub const TURN_GUARD: u32 = 200;
/// R23 D4: wall time is a guard, not the budget. Eight hours per phase;
/// the expected stop is dollars. The late checks in later stages still fit.
pub const CELL_WALL_MIN: u64 = 480;
/// R23 D4: one ticket's per-attempt wall guard. It must never bind before
/// the ticket's dollar budget in normal use.
pub const TICKET_WALL_GUARD_MIN: u64 = 120;

/// R23 D3: a paid campaign launch must have a real list price. The live
/// OpenRouter catalog is tried first, then the offline table; eval-only
/// runs do not spend and are exempt. `_at` is the offline-testable form.
pub async fn require_model_list_price_at(
    model: &str,
    url: &str,
    eval_only: bool,
) -> Result<(), String> {
    if eval_only {
        return Ok(());
    }
    if crate::cost::calculator::ModelPricing::list_price_for_model_async_at(model, url)
        .await
        .is_some()
    {
        Ok(())
    } else {
        Err(format!(
            "model {model} has no list price in the OpenRouter catalog or offline table; refusing a paid campaign launch"
        ))
    }
}

pub async fn require_model_list_price(model: &str, eval_only: bool) -> Result<(), String> {
    require_model_list_price_at(model, "https://openrouter.ai/api/v1/models", eval_only).await
}

/// The argv `POST /api/campaigns` spawns: one place, and the CLI parse
/// test pins that the real clap definition accepts it (round-8 finding 2).
pub fn launcher_args(model: &str, effort: &str, budget_usd: f64, wall_min: u64) -> Vec<String> {
    launcher_args_with_phase(model, effort, budget_usd, wall_min, None, None, None)
}

/// Launcher argv with the optional career phase/chain (P1). Kept separate
/// so the existing launcher callers and their CLI-parse test stay stable.
pub fn launcher_args_with_phase(
    model: &str,
    effort: &str,
    budget_usd: f64,
    wall_min: u64,
    phase: Option<u32>,
    chain: Option<&str>,
    refork: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "campaign",
        "run",
        "--name",
        "sdb-sql",
        "--model",
        model,
        "--effort",
        effort,
        "--budget-usd",
        &format!("{budget_usd}"),
        "--wall-min",
        &format!("{wall_min}"),
        "--timeout-min",
        &TICKET_WALL_GUARD_MIN.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(p) = phase {
        args.push("--phase".into());
        args.push(p.to_string());
    }
    if let Some(c) = chain.filter(|c| !c.is_empty()) {
        args.push("--chain".into());
        args.push(c.to_string());
    }
    if let Some(r) = refork.filter(|r| !r.trim().is_empty()) {
        args.push("--refork".into());
        args.push(r.to_string());
    }
    args
}

/// The launcher's resolved binary: the repo's release build when present
/// (a rebuilt binary's `/proc/self/exe` reads `… (deleted)` and spawning it
/// fails), else the running executable with that suffix stripped.
pub fn launcher_binary() -> Option<std::path::PathBuf> {
    let release = crate::config::get_repo_root().join("target/release/subdollarbench");
    if release.is_file() {
        return Some(release);
    }
    std::env::current_exe().ok().map(|p| {
        let s = p.to_string_lossy();
        match s.strip_suffix(" (deleted)") {
            Some(base) if std::path::Path::new(base).is_file() => std::path::PathBuf::from(base),
            _ => p,
        }
    })
}

/// The board's ranking key (gui-spec.md §4.1): current rule sets first,
/// then difficulty-weighted points, then points per dollar, then cheaper.
/// ONE implementation for the console, the API and the public board —
/// there must be no second ranking.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct BoardSortKey {
    /// A running cell with no rollup yet: pinned first, in its own group,
    /// so the comparator stays a total order (round-8 finding 9).
    pub live: bool,
    pub current_rules: bool,
    pub points: f64,
    pub per_dollar: Option<f64>,
    pub cost: f64,
}

impl BoardSortKey {
    pub fn of(rollup: &CampaignRollup) -> Self {
        let current = is_current_rules(&rollup.rules_version);
        // Q5: a phase cell is ranked as its career (the leaf's cumulative
        // points and spend), never as one phase in isolation.
        let (points, cost) = rollup
            .career
            .as_ref()
            .map(|c| (c.points, c.total_cost_usd))
            .unwrap_or((rollup.points, rollup.total_cost_usd));
        Self {
            live: false,
            current_rules: current,
            points,
            per_dollar: (cost > 0.0).then(|| points / cost),
            cost,
        }
    }
}

/// The one "current rules" predicate (round-13 B2): the console, the API and
/// the portal must agree on what is comparable. An unknown (empty) version is
/// *not* current.
pub fn is_current_rules(rules: &str) -> bool {
    rules == crate::report::rules::RULES_VERSION
}

/// The board order: current rules first, then points desc, then points per
/// dollar desc, then cheaper.
pub fn board_order(a: &BoardSortKey, b: &BoardSortKey) -> std::cmp::Ordering {
    b.live
        .cmp(&a.live)
        .then_with(|| b.current_rules.cmp(&a.current_rules))
        .then_with(|| {
            b.points
                .partial_cmp(&a.points)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            b.per_dollar
                .unwrap_or(0.0)
                .partial_cmp(&a.per_dollar.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            a.cost
                .partial_cmp(&b.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// The stop reason for a finished campaign cell (gui-spec.md §4.1).
pub fn stop_reason(rollup: &CampaignRollup) -> &'static str {
    if rollup.harness_fault.is_some() {
        "harness fault"
    } else if rollup.over_budget || rollup.cap_reached {
        "budget cap"
    } else if rollup.over_wall {
        "wall cap"
    } else {
        "ladder end"
    }
}

/// Derive the full ladder from a rollup and the campaign spec: one
/// implementation for the console, the API and the public board
/// (gui-spec.md §5.3). `needs` unmet -> `skipped`; a runnable but
/// unattempted ticket -> `not reached` (with the stop reason).
pub fn ladder(
    spec: &CampaignSpec,
    rollup: &CampaignRollup,
    current: Option<u32>,
) -> Vec<LadderEntry> {
    ladder_inner(spec, rollup, current, &[])
}

/// `ladder` with tickets passed in earlier phases counted towards `needs`.
fn ladder_inner(
    spec: &CampaignSpec,
    rollup: &CampaignRollup,
    current: Option<u32>,
    carried_passed: &[u32],
) -> Vec<LadderEntry> {
    let passed: Vec<u32> = rollup
        .stages
        .iter()
        .filter(|r| r.passed)
        .map(|r| r.id)
        .chain(carried_passed.iter().copied())
        .collect();
    let mut out = Vec::new();
    for stage in &spec.stages {
        let records: Vec<&StageRecord> =
            rollup.stages.iter().filter(|r| r.id == stage.id).collect();
        let passed_full = records.iter().find(|r| r.passed && r.variant != "lite");
        let passed_any = records.iter().rev().find(|r| r.passed);
        let entry = if let Some(rec) = passed_any {
            LadderEntry {
                id: stage.id,
                title: stage.title.clone(),
                status: if passed_full.is_some() {
                    "pass"
                } else {
                    "lite"
                }
                .to_string(),
                variant: rec.variant.clone(),
                points: passed_full.map(|r| r.points).unwrap_or(rec.points),
                stop_reason: rec.stop_reason.clone(),
                cause: None,
            }
        } else if let Some(rec) = records.last() {
            let status = match rec.status.as_str() {
                "fault" => "fault",
                "skipped" => "skipped",
                _ => "fail",
            };
            LadderEntry {
                id: stage.id,
                title: stage.title.clone(),
                status: status.to_string(),
                variant: rec.variant.clone(),
                points: 0.0,
                stop_reason: rec.stop_reason.clone(),
                cause: rec
                    .failure
                    .as_ref()
                    .map(|f| f.line.clone())
                    .or_else(|| rec.git_stat.is_some().then(|| "failed".to_string())),
            }
        } else if current == Some(stage.id) {
            // Attempted right now, no record yet: the Run view and the
            // board show it as running (round-8 finding 8).
            LadderEntry {
                id: stage.id,
                title: stage.title.clone(),
                status: "running".to_string(),
                variant: String::new(),
                points: 0.0,
                stop_reason: None,
                cause: None,
            }
        } else {
            let unmet: Vec<u32> = stage
                .needs
                .iter()
                .copied()
                .filter(|n| !passed.contains(n))
                .collect();
            let (status, cause) = if !unmet.is_empty() {
                let list = unmet
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                // F3: while this cell is live, an unmet need that has no
                // deciding negative record is still pending. Show waiting
                // (cause "needs N"), not skipped; once the need fails or
                // the cell is over, the ordinary skipped label applies.
                let decided = unmet
                    .iter()
                    .any(|n| rollup.stages.iter().any(|r| r.id == *n && !r.passed));
                if decided || !rollup.finished_at.is_empty() {
                    ("skipped", format!("skipped: needs {list}"))
                } else {
                    ("waiting", format!("needs {list}"))
                }
            } else if rollup.harness_fault.is_some() {
                ("not_reached", "not reached: harness fault".to_string())
            } else if rollup.over_budget || rollup.cap_reached {
                ("not_reached", "not reached: budget cap".to_string())
            } else if rollup.over_wall {
                ("not_reached", "not reached: wall cap".to_string())
            } else {
                ("not_reached", "not reached".to_string())
            };
            LadderEntry {
                id: stage.id,
                title: stage.title.clone(),
                status: status.to_string(),
                variant: String::new(),
                points: 0.0,
                stop_reason: None,
                cause: Some(cause),
            }
        };
        out.push(entry);
    }
    out
}

/// S3: the ladder a phase cell shows. Every ticket is listed; tickets of
/// an earlier phase that this cell did not attempt are `carried` (variant =
/// the carried outcome), tickets of a later phase are `later_phase`, and
/// the cell's own tickets count the carried passes towards their `needs`.
/// A full-ladder cell gets the plain `ladder`.
pub fn phase_ladder(
    spec: &CampaignSpec,
    rollup: &CampaignRollup,
    current: Option<u32>,
    carried: Option<&LadderState>,
) -> Vec<LadderEntry> {
    let Some(cell_phase) = rollup.phase.filter(|_| !spec.phases.is_empty()) else {
        return ladder(spec, rollup, current);
    };
    let carried = carried.or(rollup.ladder_state.as_ref());
    let carried_passed = carried.map(|s| s.passed.clone()).unwrap_or_default();
    let mut out = ladder_inner(spec, rollup, current, &carried_passed);
    for e in &mut out {
        let Some(p) = phase_of(spec, e.id).map(|p| p.id) else {
            continue;
        };
        let attempted = current == Some(e.id) || rollup.stages.iter().any(|r| r.id == e.id);
        if p > cell_phase {
            e.status = "later_phase".to_string();
            e.variant.clear();
            e.points = 0.0;
            e.cause = Some(format!("phase {p}: not part of this cell"));
        } else if p < cell_phase && !attempted {
            let label = match carried.and_then(|s| s.outcomes.get(&e.id)) {
                Some(LadderOutcome::Full) => "full",
                Some(LadderOutcome::Lite) => "lite",
                Some(LadderOutcome::Failed) => "failed",
                Some(LadderOutcome::Skipped) => "skipped",
                None => "unknown",
            };
            e.status = "carried".to_string();
            e.variant = label.to_string();
            e.points = 0.0;
            e.cause = Some(format!("phase {p}, carried over: {label}"));
        }
    }
    out
}

/// S3: the phase table the console draws the strip groups from.
pub fn phases_json(spec: &CampaignSpec) -> serde_json::Value {
    serde_json::Value::Array(
        spec.phases
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "title": p.title,
                    "stages": p.stages,
                    "max_points": phase_max_points(p, &spec.stages),
                })
            })
            .collect(),
    )
}

/// The ladder state that survives a phase boundary (P1 career chaining).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LadderState {
    pub outcomes: BTreeMap<u32, LadderOutcome>,
    pub passed: Vec<u32>,
    pub liters: BTreeMap<u32, Vec<String>>,
    pub best: BTreeMap<u32, bool>,
    pub dirty: bool,
    pub last_green: Option<String>,
    /// P2 harness hint block for the next phase (recorded failures and the
    /// phase tickets they block).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<String>,
    /// Q7: each broken ticket's last FailureSummary, carried across phases.
    #[serde(default)]
    pub last_failures: BTreeMap<u32, FailureSummary>,
}

/// R1 (D2): an explicit fork of an already-used chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReforkInfo {
    /// The successor that already claimed the predecessor.
    pub of: String,
    pub reason: String,
}

/// Career totals across chained phase cells (P1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CareerRollup {
    pub points: f64,
    pub total_cost_usd: f64,
    pub cells: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignRollup {
    pub campaign_id: String,
    pub name: String,
    pub model: String,
    pub harness: String,
    pub effort: String,
    pub task: String,
    pub started_at: String,
    pub finished_at: String,
    pub stages: Vec<StageRecord>,
    /// Highest stage id whose ticket passed (full or lite); with the
    /// continue ladder this is the maximum passed id, not a contiguous
    /// point.
    pub reached: Option<u32>,
    /// Tickets passed without any regression in their acceptance suite.
    pub regression_free_tickets: u32,
    pub total_cost_usd: f64,
    /// Spent *at or above* the campaign cap (round-11 finding 2: this stays
    /// strictly "over budget"; the floor stop is `cap_reached`).
    pub over_budget: bool,
    /// The ladder stopped because the cap's remaining headroom fell below
    /// `BUDGET_FLOOR_USD` without actually crossing the cap (round-11
    /// finding 2). The stop reason is still "budget cap".
    #[serde(default)]
    pub cap_reached: bool,
    /// Optional wall-clock cap reached before the ladder finished.
    #[serde(default)]
    pub over_wall: bool,
    /// The grading rule set this cell was measured under (cells from
    /// different versions never rank on one board).
    #[serde(default)]
    pub rules_version: String,
    /// Campaign-wide budget cap in USD (0 = uncapped), for the Run view's
    /// spend bar.
    #[serde(default)]
    pub budget_usd: f64,
    /// Wall-clock cap in minutes (0 = uncapped).
    #[serde(default)]
    pub wall_min: u64,
    /// Difficulty-weighted points actually scored (tier x weight per pass).
    #[serde(default)]
    pub points: f64,
    /// Ceiling: every stage full (sum of tiers).
    #[serde(default)]
    pub max_points: f64,
    /// Career phase this cell ran, when the spec is split (P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<u32>,
    /// Previous cell this phase continued from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_from: Option<String>,
    /// Career totals across the chain (points, spend and cell ids).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub career: Option<CareerRollup>,
    /// R1: a deliberate fork of an already-used chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refork: Option<ReforkInfo>,
    /// The persisted ladder state for a chained next phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ladder_state: Option<LadderState>,
    /// Round-14 N2: the ladder stopped because an attempt measured nothing
    /// (the agent never started, or a billing/auth trip). The cell is a
    /// non-result: the runner poisons it and no board publishes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_fault: Option<String>,
}

fn outcomes_vec(m: &HashMap<u32, LadderOutcome>) -> Vec<(u32, LadderOutcome)> {
    let mut v: Vec<(u32, LadderOutcome)> = m.iter().map(|(k, o)| (*k, *o)).collect();
    v.sort_by_key(|(id, _)| *id);
    v
}

fn snapshot_state(
    outcomes: &HashMap<u32, LadderOutcome>,
    passed_all: &std::collections::BTreeSet<u32>,
    liters: &std::collections::BTreeMap<u32, Vec<String>>,
    best: &HashMap<u32, bool>,
    dirty: bool,
    last_green: Option<String>,
    hints: Option<String>,
) -> LadderState {
    LadderState {
        outcomes: outcomes.iter().map(|(k, v)| (*k, *v)).collect(),
        passed: passed_all.iter().copied().collect(),
        liters: liters.clone(),
        best: best.iter().map(|(k, v)| (*k, *v)).collect(),
        dirty,
        last_green,
        hints,
        last_failures: BTreeMap::new(),
    }
}

impl CampaignRollup {
    /// The headline: regression-free tickets per dollar.
    pub fn tickets_per_dollar(&self) -> Option<f64> {
        if self.total_cost_usd > 0.0 {
            Some(self.regression_free_tickets as f64 / self.total_cost_usd)
        } else {
            None
        }
    }

    /// Survival vector: P(stage >= i) as a 0/1 outcome per stage id
    /// (single campaigns are one sample; the grid averages these).
    pub fn survival(&self) -> HashMap<u32, f64> {
        let mut out = HashMap::new();
        for r in &self.stages {
            let mut s = 0.0;
            if r.id > 0 {
                let prev = self.stages.iter().find(|p| p.id == r.id - 1);
                let prev_ok = prev.map(|p| p.passed).unwrap_or(true);
                s = if prev_ok { 1.0 } else { 0.0 };
                if !r.passed {
                    s = 0.0;
                }
            } else if r.passed {
                s = 1.0;
            }
            out.insert(r.id, s);
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct CampaignRunOptions {
    pub campaign_id: String,
    pub model: String,
    pub harness: String,
    pub effort: String,
    /// R23 D3: campaign-wide cap in provider API dollars at list price.
    /// The ladder stops when cumulative API spend reaches it.
    pub budget_usd: f64,
    /// Optional wall-clock cap in minutes (sdb-sql runs are multi-hour);
    /// the ladder stops with `over_wall` when reached.
    pub wall_min: Option<u64>,
}

/// Run the ladder. `exec` performs one ticket (agent + verification) and
/// returns its outcome; orchestration (stops, regressions, rollup) is pure
/// and unit-tested with a mock executor. `on_stage` receives the
/// rollup-so-far after every ticket (and on budget stop) so callers can
/// publish live progress.
fn needs_closure_contains(spec: &CampaignSpec, stage: u32, target: u32) -> bool {
    let mut stack = vec![stage];
    let mut seen = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(st) = spec.stages.iter().find(|s| s.id == id) {
            for need in &st.needs {
                if *need == target {
                    return true;
                }
                stack.push(*need);
            }
        }
    }
    false
}

/// Q1: the phase start may retry this many earlier tickets. The P2 spec
/// said two; the round-19 review found that the common 4 -> 5 -> 6 chain
/// (4 fails, 5/6 skipped) needs three to rescue a career. This is the
/// operator-facing choice recorded in the rules history.
pub const CATCH_UP_SLOTS: usize = 3;

/// P2/Q1: Failed *or* Skipped earlier tickets whose own needs are already
/// passed and that a phase ticket needs (directly or through its needs
/// closure), ordered by the phase points they unblock, then id. The caller
/// recomputes after each slot so a newly runnable skipped ticket (5 after
/// 4) can be chosen next.
pub fn catch_up_targets(
    spec: &CampaignSpec,
    phase: &PhaseSpec,
    outcomes: &HashMap<u32, LadderOutcome>,
    passed: &std::collections::BTreeSet<u32>,
) -> Vec<(u32, f64)> {
    let mut out = Vec::new();
    for (id, outcome) in outcomes {
        if !matches!(outcome, LadderOutcome::Failed | LadderOutcome::Skipped) {
            continue;
        }
        let Some(stage) = spec.stages.iter().find(|s| s.id == *id) else {
            continue;
        };
        // Q2: a catch-up must obey its own `needs`; the main loop would
        // have skipped this ticket.
        if !stage.needs.iter().all(|n| passed.contains(n)) {
            continue;
        }
        let points: f64 = phase
            .stages
            .iter()
            .filter(|sid| needs_closure_contains(spec, **sid, *id))
            .filter_map(|sid| spec.stages.iter().find(|s| s.id == *sid))
            .map(|s| s.tier as f64)
            .sum();
        if points > 0.0 {
            out.push((*id, points));
        }
    }
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    out
}

/// P2: the phase-start hint block. It is built only from recorded verifier
/// output (the previous rollup's failure line/detail), never a reference
/// solution.
pub fn phase_hints(
    prev: &CampaignRollup,
    spec: &CampaignSpec,
    phase: &PhaseSpec,
    state: &LadderState,
) -> Option<String> {
    let mut broken: Vec<u32> = state
        .outcomes
        .iter()
        .filter(|(_, o)| matches!(o, LadderOutcome::Failed | LadderOutcome::Skipped))
        .map(|(id, _)| *id)
        .collect();
    broken.sort_unstable();
    let mut block = String::new();
    for id in broken {
        let blocked: Vec<(u32, String, f64)> = phase
            .stages
            .iter()
            .filter(|sid| needs_closure_contains(spec, **sid, id))
            .filter_map(|sid| spec.stages.iter().find(|s| s.id == *sid))
            .map(|s| (s.id, s.title.clone(), s.tier as f64))
            .collect();
        if blocked.is_empty() {
            continue;
        }
        let title = spec
            .stages
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.title.clone())
            .unwrap_or_else(|| format!("ticket {id}"));
        // Q7: the failure may be older than the immediate predecessor; the
        // state carries each broken ticket's last FailureSummary.
        let failure = prev
            .stages
            .iter()
            .rev()
            .find(|r| r.id == id)
            .and_then(|r| r.failure.as_ref())
            .or_else(|| state.last_failures.get(&id));
        let line = failure
            .map(|f| f.line.clone())
            .unwrap_or_else(|| "(no recorded failure line)".to_string());
        let detail = failure
            .map(|f| f.detail.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "(no reproduction recorded)".to_string());
        let blocked_txt = blocked
            .iter()
            .map(|(i, t, p)| format!("ticket {i} ({t}, {} pts)", crate::campaign::fmt_points(*p)))
            .collect::<Vec<_>>()
            .join(", ");
        block.push_str(&format!(
            "- Ticket {id} ({title}) is still broken. Last failure: {line}\n  Repro: {detail}\n  Blocks: {blocked_txt}\n  SPEC.md section: # Ticket {id} \u{2014} {title}\n"
        ));
    }
    if block.is_empty() {
        None
    } else {
        Some(format!(
            "## Career hints for phase {}\nThe previous phase recorded these blockers. They cost no points to read; up to {} catch-up slots may retry these before the phase.\n{}",
            phase.id, CATCH_UP_SLOTS, block
        ))
    }
}

/// F2: only a real harness/provider fault is a non-result. An agent that
/// exited non-zero after grading is a candidate failure; its lite attempt
/// must still run. `infra_error` without a `harness_fault` is still a
/// measured-nothing fault and stops the cell with this reason.
fn measured_nothing(out: &StageOutcome) -> Option<&str> {
    out.harness_fault
        .as_deref()
        .or_else(|| out.infra_error.then_some("infrastructure error"))
}

/// Format a points value without trailing zeroes, for the hint text.
fn fmt_points(p: f64) -> String {
    if (p.fract()).abs() < 1e-9 {
        format!("{}", p as u32)
    } else {
        format!("{p:.1}")
    }
}

pub async fn run_campaign<F, Fut>(
    lc: &LoadedCampaign,
    opts: &CampaignRunOptions,
    exec: F,
    on_stage: Option<&dyn Fn(&CampaignRollup)>,
) -> Result<CampaignRollup>
where
    F: FnMut(&StageAttempt<'_>) -> Fut,
    Fut: std::future::Future<Output = Result<StageOutcome>>,
{
    run_campaign_ctx(lc, opts, None, None, None, None, exec, on_stage).await
}

/// Career-phase run (P1): the ladder is filtered to `phase`, the state comes
/// from `chain` and the career totals from `career`.
#[allow(clippy::too_many_arguments)]
pub async fn run_campaign_phase<F, Fut>(
    lc: &LoadedCampaign,
    opts: &CampaignRunOptions,
    phase: Option<&PhaseSpec>,
    chain: Option<&LadderState>,
    chain_from: Option<&str>,
    career: Option<&CareerRollup>,
    exec: F,
    on_stage: Option<&dyn Fn(&CampaignRollup)>,
) -> Result<CampaignRollup>
where
    F: FnMut(&StageAttempt<'_>) -> Fut,
    Fut: std::future::Future<Output = Result<StageOutcome>>,
{
    run_campaign_ctx(lc, opts, phase, chain, chain_from, career, exec, on_stage).await
}

#[allow(clippy::too_many_arguments)]
async fn run_campaign_ctx<F, Fut>(
    lc: &LoadedCampaign,
    opts: &CampaignRunOptions,
    phase: Option<&PhaseSpec>,
    chain: Option<&LadderState>,
    chain_from: Option<&str>,
    career: Option<&CareerRollup>,
    mut exec: F,
    on_stage: Option<&dyn Fn(&CampaignRollup)>,
) -> Result<CampaignRollup>
where
    F: FnMut(&StageAttempt<'_>) -> Fut,
    Fut: std::future::Future<Output = Result<StageOutcome>>,
{
    let started_at = chrono::Utc::now().to_rfc3339();
    let wall_start = std::time::Instant::now();
    let mut best: HashMap<u32, bool> = chain
        .map(|c| c.best.iter().map(|(k, v)| (*k, *v)).collect())
        .unwrap_or_default();
    let mut reached: Option<u32> = chain.and_then(|c| c.passed.iter().copied().max());
    // R23 D3: the cap counts the provider API bill at list price. The
    // compute floor remains a reported number in each StageRecord, never a
    // stopping condition.
    let mut total_cost = 0.0f64;
    // Ladder bookkeeping (continue mode): per-stage outcome + whether the
    // last attempt left the workspace dirty (needs a rollback first).
    let mut outcomes: HashMap<u32, LadderOutcome> = chain
        .map(|c| c.outcomes.iter().map(|(k, v)| (*k, *v)).collect())
        .unwrap_or_default();
    // Which stages passed on a lite variant (stage -> profile).
    let mut liters: std::collections::BTreeMap<u32, Vec<String>> =
        chain.map(|c| c.liters.clone()).unwrap_or_default();
    // Every stage passed so far (full or lite): the declared acceptance
    // list is intersected with it, so a failed stage sinks nothing later.
    let mut passed_all: std::collections::BTreeSet<u32> = chain
        .map(|c| c.passed.iter().copied().collect())
        .unwrap_or_default();
    // Largest single call seen anywhere in this cell; the cap reserves it
    // before the next ticket (round-17 P0).
    let mut largest_call_usd = 0.0f64;
    let mut dirty = chain.map(|c| c.dirty).unwrap_or(false);
    let chain_last_green = chain.and_then(|c| c.last_green.clone());
    let chain_hints = chain.and_then(|c| c.hints.clone());
    let mut rollup = CampaignRollup {
        campaign_id: opts.campaign_id.clone(),
        name: lc.spec.name.clone(),
        model: opts.model.clone(),
        harness: opts.harness.clone(),
        effort: opts.effort.clone(),
        task: lc.spec.task.clone(),
        started_at: started_at.clone(),
        finished_at: String::new(),
        stages: Vec::new(),
        reached,
        regression_free_tickets: 0,
        total_cost_usd: 0.0,
        over_budget: false,
        cap_reached: false,
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        budget_usd: opts.budget_usd,
        wall_min: opts.wall_min.unwrap_or(0),
        points: 0.0,
        max_points: phase
            .map(|p| phase_max_points(p, &lc.spec.stages))
            .unwrap_or_else(|| max_points(&lc.spec.stages)),
        phase: phase.map(|p| p.id),
        chain_from: chain_from.map(str::to_string),
        career: None,
        ladder_state: None,
        harness_fault: None,
        refork: None,
    };

    let stage_list: Vec<&StageSpec> = match phase {
        Some(p) => lc
            .spec
            .stages
            .iter()
            .filter(|s| p.stages.contains(&s.id))
            .collect(),
        None => lc.spec.stages.iter().collect(),
    };
    // P2/Q1: career catch-up slots. Choose greedily, one at a time, from
    // Failed *or* Skipped earlier tickets whose own needs are already met; a
    // pass can make the next skipped ticket runnable (the common 4 -> 5 -> 6
    // chain). Up to CATCH_UP_SLOTS (3) are retried before the first ticket.
    let mut catch_up_fault = false;
    let mut attempted = HashSet::new();
    if let Some(phase) = phase {
        for _slot in 0..CATCH_UP_SLOTS {
            if !cap_allows(opts.budget_usd, total_cost, largest_call_usd) {
                break;
            }
            let targets = catch_up_targets(&lc.spec, phase, &outcomes, &passed_all);
            let Some((target, _blocked_points)) =
                targets.into_iter().find(|(id, _)| !attempted.contains(id))
            else {
                break;
            };
            attempted.insert(target);
            let Some(stage) = lc.spec.stages.iter().find(|s| s.id == target) else {
                continue;
            };
            let headroom = cap_headroom(opts.budget_usd, total_cost, largest_call_usd)
                .unwrap_or(f64::INFINITY);
            let (variant, prompt_path, max_turns, lite_profile) = match &stage.lite {
                Some(lite) => (
                    StageVariant::Lite,
                    lc.root.join(&lite.prompt),
                    lite.max_turns,
                    lite.profile.clone(),
                ),
                None => (
                    StageVariant::Full,
                    lc.prompt_path(stage),
                    stage.max_turns,
                    Vec::new(),
                ),
            };
            // R23 D2: the catch-up draws from the phase pool. The target's
            // tier joins the still-undecided phase tickets' weight.
            let catch_up_weight = runnable_tier_sum(&stage_list, &outcomes) + stage.tier;
            let attempt = StageAttempt {
                stage,
                variant,
                prompt_path,
                budget_usd: pool_budget(headroom, stage.tier, catch_up_weight),
                max_turns,
                lite_profile: lite_profile.clone(),
                passed_lite: liters.iter().map(|(k, v)| (*k, v.clone())).collect(),
                passed: passed_all.iter().copied().collect(),
                outcomes: outcomes_vec(&outcomes),
                weight: 0.4,
                cap_headroom_usd: (opts.budget_usd > 0.0).then_some(headroom),
                hints: chain_hints.clone(),
                catch_up: true,
                rollback_first: dirty,
            };
            // Q9: the shown phase ceiling includes the catch-up's possible
            // 0.4 x tier, so a full phase plus a catch-up cannot read >max.
            rollup.max_points += stage.tier as f64 * 0.4;
            let out = exec(&attempt)
                .await
                .with_context(|| format!("catch-up stage {} ({})", target, stage.title))?;
            largest_call_usd = largest_call_usd.max(out.max_call_usd);
            let (regressed, severity) = detect_regressions(&best, &out.stage_results);
            for r in &out.stage_results {
                let e = best.entry(r.stage).or_insert(false);
                *e = *e || r.passed;
            }
            total_cost += out.api_cost_usd;
            push_attempt_record(
                &mut rollup,
                stage,
                variant,
                &out,
                regressed,
                severity,
                "catch-up",
            );
            if out.passed {
                outcomes.insert(target, LadderOutcome::Lite);
                passed_all.insert(target);
                if variant == StageVariant::Lite {
                    liters.insert(target, lite_profile);
                }
                reached = Some(reached.map(|r| r.max(target)).unwrap_or(target));
                dirty = false;
            } else {
                outcomes.insert(target, LadderOutcome::Failed);
                dirty = true;
            }
            rollup.reached = reached;
            rollup.total_cost_usd = total_cost;
            rollup.points = rollup.stages.iter().map(|r| r.points).sum();
            rollup.regression_free_tickets = rollup
                .stages
                .iter()
                .filter(|r| r.passed && r.regressed.is_empty())
                .count() as u32;
            rollup.ladder_state = Some(snapshot_state(
                &outcomes,
                &passed_all,
                &liters,
                &best,
                dirty,
                chain_last_green.clone(),
                chain_hints.clone(),
            ));
            if let Some(cb) = on_stage {
                cb(&rollup);
            }
            if let Some(reason) = measured_nothing(&out) {
                // Q3: a catch-up fault is a cell fault, not a reason to run
                // the rest of the phase against a non-result.
                outcomes.insert(target, LadderOutcome::Failed);
                stop_on_harness_fault(&mut rollup, target, reason, reached, total_cost);
                rollup.ladder_state = Some(snapshot_state(
                    &outcomes,
                    &passed_all,
                    &liters,
                    &best,
                    dirty,
                    chain_last_green.clone(),
                    chain_hints.clone(),
                ));
                if let Some(cb) = on_stage {
                    cb(&rollup);
                }
                catch_up_fault = true;
                break;
            }
        }
    }

    for stage in stage_list.iter().copied() {
        if catch_up_fault {
            break;
        }
        // Caps are checked between tickets; both flags are set when both
        // were crossed at the same boundary (round-7 finding 5; the stop
        // reason stays budget-first). Round-10 D3: the cap also stops when
        // the remaining headroom cannot fund a useful attempt (below the
        // floor), instead of overshooting by one attempt's cost. Round-11
        // finding 2: crossing the cap (`over_budget`) and stopping on the
        // floor short of it (`cap_reached`) are separate flags.
        let over_budget_now = opts.budget_usd > 0.0 && total_cost >= opts.budget_usd;
        // The next ticket must fit the largest call seen so far plus the
        // floor; the reserve is subtracted from the headroom the pipeline
        // and the API-budget conversion see (round-17 P0).
        let floor_now = !cap_allows(opts.budget_usd, total_cost, largest_call_usd);
        let headroom =
            cap_headroom(opts.budget_usd, total_cost, largest_call_usd).unwrap_or(f64::INFINITY);
        // R23 D2: the phase pool is split by tier over the undecided
        // tickets; a cheap pass leaves more for the next ticket.
        let phase_weight = runnable_tier_sum(&stage_list, &outcomes);
        let outcomes_snapshot = |m: &HashMap<u32, LadderOutcome>| -> Vec<(u32, LadderOutcome)> {
            let mut v: Vec<(u32, LadderOutcome)> = m.iter().map(|(k, o)| (*k, *o)).collect();
            v.sort_by_key(|(id, _)| *id);
            v
        };
        let over_wall_now = opts
            .wall_min
            .map(|m| m > 0 && wall_start.elapsed() >= std::time::Duration::from_secs(m * 60))
            .unwrap_or(false);
        if over_budget_now || floor_now || over_wall_now {
            rollup.over_budget |= over_budget_now;
            rollup.cap_reached |= floor_now && !over_budget_now;
            rollup.over_wall |= over_wall_now;
            rollup.ladder_state = Some(snapshot_state(
                &outcomes,
                &passed_all,
                &liters,
                &best,
                dirty,
                chain_last_green.clone(),
                chain_hints.clone(),
            ));
            if let Some(cb) = on_stage {
                cb(&rollup);
            }
            break;
        }
        // `needs` gate (both modes; archived specs simply have no needs).
        // A need that failed, was skipped, or never recorded blocks the
        // stage: it is recorded as skipped and costs nothing.
        let needs_blocked = stage.needs.iter().any(|n| {
            !matches!(
                outcomes.get(n),
                Some(LadderOutcome::Full) | Some(LadderOutcome::Lite)
            )
        });
        if needs_blocked {
            outcomes.insert(stage.id, LadderOutcome::Skipped);
            rollup.stages.push(skipped_record(stage));
            rollup.reached = reached;
            rollup.total_cost_usd = total_cost;
            rollup.points = rollup.stages.iter().map(|r| r.points).sum();
            rollup.ladder_state = Some(snapshot_state(
                &outcomes,
                &passed_all,
                &liters,
                &best,
                dirty,
                chain_last_green.clone(),
                chain_hints.clone(),
            ));
            if let Some(cb) = on_stage {
                cb(&rollup);
            }
            continue;
        }

        // Full attempt.
        let attempt_full = StageAttempt {
            stage,
            variant: StageVariant::Full,
            prompt_path: lc.prompt_path(stage),
            // D3 + finding 1: the cap's effective headroom converts to the
            // API dollars OMP actually enforces.
            budget_usd: pool_budget(headroom, stage.tier, phase_weight),
            max_turns: stage.max_turns,
            lite_profile: Vec::new(),
            passed_lite: liters.iter().map(|(k, v)| (*k, v.clone())).collect(),
            passed: passed_all.iter().copied().collect(),
            outcomes: outcomes_snapshot(&outcomes),
            weight: 1.0,
            cap_headroom_usd: (opts.budget_usd > 0.0).then_some(headroom),
            hints: chain_hints.clone(),
            catch_up: false,
            rollback_first: dirty,
        };
        let out = exec(&attempt_full)
            .await
            .with_context(|| format!("stage {} ({})", stage.id, stage.title))?;
        largest_call_usd = largest_call_usd.max(out.max_call_usd);
        let (regressed, regression_severity) = detect_regressions(&best, &out.stage_results);
        for r in &out.stage_results {
            let e = best.entry(r.stage).or_insert(false);
            *e = *e || r.passed;
        }
        total_cost += out.api_cost_usd;
        // F2: a non-zero agent exit after the round was graded is a
        // candidate failure; only a real harness fault (or an infra fault)
        // measured nothing and keeps the `fault` label.
        let fault = out.infra_error || out.harness_fault.is_some();
        let status = if out.passed {
            "passed"
        } else if fault {
            "fault"
        } else {
            "failed"
        };
        push_attempt_record(
            &mut rollup,
            stage,
            StageVariant::Full,
            &out,
            regressed,
            regression_severity,
            status,
        );
        if let Some(reason) = measured_nothing(&out) {
            // Round-14 N2: a harness fault is not a ladder step. Stop the
            // cell instead of letting one crash block its dependants.
            outcomes.insert(stage.id, LadderOutcome::Failed);
            stop_on_harness_fault(&mut rollup, stage.id, reason, reached, total_cost);
            rollup.ladder_state = Some(snapshot_state(
                &outcomes,
                &passed_all,
                &liters,
                &best,
                dirty,
                chain_last_green.clone(),
                chain_hints.clone(),
            ));
            if let Some(cb) = on_stage {
                cb(&rollup);
            }
            break;
        }
        if out.passed {
            outcomes.insert(stage.id, LadderOutcome::Full);
            liters.remove(&stage.id);
            passed_all.insert(stage.id);
            reached = Some(stage.id);
            dirty = false;
        } else if fault {
            // A fault measured nothing: recorded, never a model failure,
            // but it cannot satisfy `needs`.
            outcomes.insert(stage.id, LadderOutcome::Failed);
            dirty = true;
        } else if let Some(lite) = &stage.lite {
            // Lite attempt: the workspace rolls back to the last green
            // commit (and the data dir) before the fresh session. The full
            // attempt just raised the largest-call reserve; the lite check
            // uses the raised value (round-17 P0).
            let headroom2 = cap_headroom(opts.budget_usd, total_cost, largest_call_usd)
                .unwrap_or(f64::INFINITY);
            if !cap_allows(opts.budget_usd, total_cost, largest_call_usd) {
                // D3: the full attempt already spent; no lite when the
                // remaining headroom is below the floor. The stage stays
                // failed and the next iteration stops on budget.
                outcomes.insert(stage.id, LadderOutcome::Failed);
                dirty = true;
            } else {
                // R23 D2: the lite is the same ticket, so it draws the same
                // pool share after the failed full attempt.
                let attempt_lite = StageAttempt {
                    stage,
                    variant: StageVariant::Lite,
                    prompt_path: lc.root.join(&lite.prompt),
                    budget_usd: pool_budget(
                        headroom2,
                        stage.tier,
                        runnable_tier_sum(&stage_list, &outcomes),
                    ),
                    max_turns: lite.max_turns,
                    lite_profile: lite.profile.clone(),
                    passed_lite: liters.iter().map(|(k, v)| (*k, v.clone())).collect(),
                    passed: passed_all.iter().copied().collect(),
                    outcomes: outcomes_snapshot(&outcomes),
                    weight: lite.weight,
                    cap_headroom_usd: (opts.budget_usd > 0.0).then_some(headroom2),
                    hints: chain_hints.clone(),
                    catch_up: false,
                    rollback_first: true,
                };
                let out2 = exec(&attempt_lite)
                    .await
                    .with_context(|| format!("stage {} lite", stage.id))?;
                largest_call_usd = largest_call_usd.max(out2.max_call_usd);
                let (regressed2, severity2) = detect_regressions(&best, &out2.stage_results);
                for r in &out2.stage_results {
                    let e = best.entry(r.stage).or_insert(false);
                    *e = *e || r.passed;
                }
                total_cost += out2.api_cost_usd;
                let fault2 = out2.infra_error || out2.harness_fault.is_some();
                let status2 = if out2.passed {
                    "passed"
                } else if fault2 {
                    "fault"
                } else {
                    "failed"
                };
                push_attempt_record(
                    &mut rollup,
                    stage,
                    StageVariant::Lite,
                    &out2,
                    regressed2,
                    severity2,
                    status2,
                );
                if let Some(reason) = measured_nothing(&out2) {
                    outcomes.insert(stage.id, LadderOutcome::Failed);
                    stop_on_harness_fault(&mut rollup, stage.id, reason, reached, total_cost);
                    rollup.ladder_state = Some(snapshot_state(
                        &outcomes,
                        &passed_all,
                        &liters,
                        &best,
                        dirty,
                        chain_last_green.clone(),
                        chain_hints.clone(),
                    ));
                    if let Some(cb) = on_stage {
                        cb(&rollup);
                    }
                    break;
                }
                if out2.passed {
                    outcomes.insert(stage.id, LadderOutcome::Lite);
                    liters.insert(stage.id, lite.profile.clone());
                    passed_all.insert(stage.id);
                    if reached.map(|r| stage.id > r).unwrap_or(true) {
                        reached = Some(stage.id);
                    }
                    dirty = false;
                } else {
                    outcomes.insert(stage.id, LadderOutcome::Failed);
                    dirty = true;
                }
            }
        } else {
            outcomes.insert(stage.id, LadderOutcome::Failed);
            dirty = true;
        }

        rollup.reached = reached;
        rollup.total_cost_usd = total_cost;
        rollup.points = rollup.stages.iter().map(|r| r.points).sum();
        rollup.regression_free_tickets = rollup
            .stages
            .iter()
            .filter(|r| r.passed && r.regressed.is_empty())
            .count() as u32;
        rollup.ladder_state = Some(snapshot_state(
            &outcomes,
            &passed_all,
            &liters,
            &best,
            dirty,
            chain_last_green.clone(),
            chain_hints.clone(),
        ));
        if let Some(cb) = on_stage {
            cb(&rollup);
        }
        // Historical pass-stop behavior for the archived app campaigns; the
        // continue ladder (sdb-sql) keeps going with rollbacks and lites.
        if lc.spec.ladder == LadderMode::PassStop
            && !matches!(
                outcomes.get(&stage.id),
                Some(LadderOutcome::Full) | Some(LadderOutcome::Lite)
            )
        {
            break;
        }
    }

    rollup.ladder_state = Some(snapshot_state(
        &outcomes,
        &passed_all,
        &liters,
        &best,
        dirty,
        chain_last_green.clone(),
        chain_hints.clone(),
    ));
    if phase.is_some() || career.is_some() {
        let prev = career.cloned().unwrap_or_default();
        let mut cells = prev.cells.clone();
        cells.push(opts.campaign_id.clone());
        rollup.career = Some(CareerRollup {
            points: prev.points + rollup.points,
            total_cost_usd: prev.total_cost_usd + total_cost,
            cells,
        });
    }
    rollup.finished_at = chrono::Utc::now().to_rfc3339();
    Ok(rollup)
}

/// Round-14 N2: record why the ladder stopped on a harness fault and settle
/// the rollup's totals (the caller breaks out of the ladder).
fn stop_on_harness_fault(
    rollup: &mut CampaignRollup,
    stage_id: u32,
    reason: &str,
    reached: Option<u32>,
    total_cost: f64,
) {
    rollup.harness_fault = Some(format!("ticket {stage_id}: {reason}"));
    rollup.reached = reached;
    rollup.total_cost_usd = total_cost;
    rollup.points = rollup.stages.iter().map(|r| r.points).sum();
    rollup.regression_free_tickets = rollup
        .stages
        .iter()
        .filter(|r| r.passed && r.regressed.is_empty())
        .count() as u32;
}

/// A stage recorded as skipped (`needs` blocked): no attempt, no cost.
fn skipped_record(stage: &StageSpec) -> StageRecord {
    StageRecord {
        id: stage.id,
        title: stage.title.clone(),
        kind: stage.kind.clone(),
        passed: false,
        pass_rate: 0.0,
        passed_stages: 0,
        total_stages: 0,
        regressed: Vec::new(),
        regression_severity: 0,
        effective_cost_usd: 0.0,
        turns: None,
        duration_seconds: None,
        status: "skipped".to_string(),
        stop_reason: None,
        variant: match stage.lite {
            Some(_) => "full".to_string(),
            None => "full".to_string(),
        },
        tier: stage.tier,
        points: 0.0,
        run_id: format!("{}_skipped", stage.id),
        git_commit: None,
        git_stat: None,
        rounds: Vec::new(),
        failure: None,
    }
}

/// Record one attempt (full or lite) into the rollup.
#[allow(clippy::too_many_arguments)]
fn push_attempt_record(
    rollup: &mut CampaignRollup,
    stage: &StageSpec,
    variant: StageVariant,
    out: &StageOutcome,
    regressed: Vec<u32>,
    regression_severity: u32,
    status: &str,
) {
    let points = if status == "catch-up" && out.passed {
        // P2: a catch-up unblocks `needs` but never outranks a first-time
        // pass; it always scores the lite weight (0.4 x tier).
        stage.tier as f64 * 0.4
    } else {
        match (status, variant) {
            ("passed", StageVariant::Full) => stage.tier as f64,
            ("passed", StageVariant::Lite) => {
                stage.tier as f64 * stage.lite.as_ref().map(|l| l.weight).unwrap_or(0.4)
            }
            _ => 0.0,
        }
    };
    rollup.stages.push(StageRecord {
        id: stage.id,
        title: stage.title.clone(),
        kind: stage.kind.clone(),
        passed: out.passed,
        pass_rate: out.pass_rate,
        passed_stages: out.passed_stages,
        total_stages: out.total_stages,
        regressed,
        regression_severity,
        effective_cost_usd: out.effective_cost_usd,
        turns: out.turns,
        duration_seconds: out.duration_seconds,
        status: status.to_string(),
        stop_reason: out.stop_reason.clone(),
        variant: match variant {
            StageVariant::Full => "full".to_string(),
            StageVariant::Lite => "lite".to_string(),
        },
        tier: stage.tier,
        points,
        run_id: out.run_id.clone(),
        git_commit: out.git_commit.clone(),
        git_stat: out.git_stat.clone(),
        rounds: out.rounds.clone(),
        failure: out.failure.clone(),
    });
}

/// Host-side git commit of the current workspace state as one stage.
/// The workspace is a plain directory and git runs from the host, so this
/// stays language-agnostic (the sandbox needs no git). Initializes the
/// repo and a small artifact .gitignore on first use; `--allow-empty` keeps
/// the timeline complete even for faulted tickets.
pub fn git_commit_stage(
    workdir: &Path,
    stage: u32,
    title: &str,
    verdict: &str,
) -> Result<(String, String)> {
    let git = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(workdir)
            .output()
            .with_context(|| format!("run git {:?} in {}", args, workdir.display()))?;
        if !out.status.success() {
            bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    if !workdir.join(".git").is_dir() {
        git(&["init", "-q"])?;
    }
    // Harness-owned artifacts are never source (round-10 D1): the per-round
    // FEEDBACK_REPORT.md is deleted around every attempt, and the ignore
    // entry keeps a legacy copy from being re-added or resurrected by a
    // rollback. Rewriting keeps workspaces created before this rule in step.
    let ignore = workdir.join(".gitignore");
    let mut text = std::fs::read_to_string(&ignore).unwrap_or_default();
    for line in [
        "target/",
        "node_modules/",
        "__pycache__/",
        "*.o",
        "*.tmp",
        // Runtime state (AOF/dumps) is evidence on disk, never source:
        // committing it bloated the first replay's stage diffs (123k lines).
        "appendonly.log",
        "*.aof",
        "*.rdb",
        // Per-round verifier feedback, rewritten by the harness (D1).
        "FEEDBACK_REPORT.md",
    ] {
        if !text.lines().any(|l| l.trim() == line) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(line);
            text.push('\n');
        }
    }
    std::fs::write(&ignore, &text)?;
    // A copy tracked before this rule leaves the index now.
    git(&[
        "rm",
        "--cached",
        "-q",
        "--ignore-unmatch",
        "FEEDBACK_REPORT.md",
    ])?;
    git(&["add", "-A"])?;
    let msg = format!("stage {stage}: {title} [{verdict}]");
    let commit = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=subdollarbench",
            "-c",
            "user.email=bench@local",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &msg,
        ])
        .current_dir(workdir)
        .output()
        .context("run git commit")?;
    if !commit.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    let hash = git(&["rev-parse", "--short", "HEAD"])?.trim().to_string();
    let stat = git(&["show", "--stat", "--format=%h %s", "HEAD"])?;
    Ok((hash, stat))
}

/// Recursive copy excluding `.git`: the stage history belongs to the
/// original workspace; scratch copies (tests sensitivity scenarios) start
/// clean. Used by the tests/CI stage.
pub fn copy_dir_excluding_git(src: &Path, dst: &Path) -> Result<()> {
    copy_dir_inner(src, dst, false)
}

/// Copy a workspace **including** `.git`, so a scratch copy can run
/// `git apply --3way` against the original object database (the ticket-15
/// sensitivity check reverses only the ticket's own diff when it still
/// applies; round-15 M3).
pub fn copy_dir_with_git(src: &Path, dst: &Path) -> Result<()> {
    copy_dir_inner(src, dst, true)
}

fn copy_dir_inner(src: &Path, dst: &Path, include_git: bool) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if !include_git && name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir_inner(&from, &to, include_git)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| anyhow::anyhow!("copy {} -> {}: {e}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Full patch of the latest stage commit (evidence for review).
pub fn git_head_patch(workdir: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["format-patch", "-1", "--stdout", "HEAD"])
        .current_dir(workdir)
        .output()
        .context("run git format-patch")?;
    if !out.status.success() {
        bail!(
            "git format-patch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sdb_campaign_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One stage's outcome under the continue-past-failures ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LadderOutcome {
    /// The full ticket's acceptance passed.
    Full,
    /// The lite variant passed (scores `lite.weight` of the tier).
    Lite,
    /// Full and lite both failed (or no lite existed).
    Failed,
    /// Skipped because a `needs` prerequisite failed.
    Skipped,
}

/// Points for one stage: tier x (1.0 full | lite.weight lite | 0 else).
pub fn stage_points(stage: &StageSpec, outcome: LadderOutcome) -> f64 {
    let tier = stage.tier as f64;
    match outcome {
        LadderOutcome::Full => tier,
        LadderOutcome::Lite => tier * stage.lite.as_ref().map(|l| l.weight).unwrap_or(0.4),
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

#[cfg(test)]
mod tests {
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
            over_budget: false,
            cap_reached: false,
            over_wall: false,
            rules_version: String::new(),
            budget_usd: 0.0,
            wall_min: 0,
            points: 0.0,
            max_points: 0.0,
            harness_fault: None,
            phase: None,
            chain_from: None,
            career: None,
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
            over_budget: true,
            cap_reached: false,
            over_wall: false,
            rules_version: crate::report::rules::RULES_VERSION.to_string(),
            budget_usd: 0.25,
            wall_min: 30,
            points: 1.0,
            max_points: 4.0,
            harness_fault: None,
            phase: None,
            chain_from: None,
            career: None,
            ladder_state: None,
            refork: None,
        };
        // 1's full attempt failed, its lite attempt passed.
        rollup.stages.push(record(1, false, "full", "failed"));
        rollup.stages.push(record(1, true, "lite", "passed"));
        // Ticket 2 was attempted and failed.
        rollup.stages.push(record(2, false, "full", "failed"));
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
            effective_cost_usd: 0.01,
            api_cost_usd: 0.01,
            turns: Some(10),
            duration_seconds: Some(60.0),
            max_call_usd: 0.0,
            stop_reason: None,
            agent_failed: false,
            infra_error: false,
            rounds: Vec::new(),
            failure: None,
            harness_fault: None,
            disqualified: false,
            git_commit: Some("abc1234".into()),
            git_stat: Some("1 file changed".into()),
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

    #[test]
    fn test_real_sdb_kv_spec_validates() {
        let lc = load_campaign("sdb-kv").expect("bundled campaign must load");
        assert_eq!(lc.spec.name, "sdb-kv");
        assert!(lc.spec.stages.len() >= 4);
        assert_eq!(lc.spec.stages[0].id, 0);
    }

    #[test]
    fn test_real_sdb_edge_spec_validates() {
        let lc = load_campaign("sdb-edge").expect("bundled campaign must load");
        assert_eq!(lc.spec.name, "sdb-edge");
        assert_eq!(lc.spec.task, "proxy");
        assert_eq!(lc.spec.stages.len(), 7);
        assert_eq!(lc.spec.stages[0].id, 0);
        for stage in &lc.spec.stages {
            assert!(
                !stage.acceptance.proxy_stages.is_empty(),
                "ticket {} has no proxy acceptance",
                stage.id
            );
            assert!(
                stage
                    .acceptance
                    .proxy_stages
                    .iter()
                    .all(|id| (1..=9).contains(id)),
                "ticket {} acceptance out of the proxy suite",
                stage.id
            );
        }
        // Acceptance scopes grow with the ladder.
        assert_eq!(lc.spec.stages[0].acceptance.proxy_stages, vec![1]);
        assert_eq!(
            lc.spec.stages[6].acceptance.proxy_stages,
            vec![1, 2, 3, 4, 6, 7, 8, 9]
        );
    }

    #[test]
    fn test_real_sdb_queue_spec_validates() {
        let lc = load_campaign("sdb-queue").expect("bundled campaign must load");
        assert_eq!(lc.spec.name, "sdb-queue");
        assert_eq!(lc.spec.task, "queue");
        assert_eq!(lc.spec.stages.len(), 9);
        assert_eq!(lc.spec.stages[0].id, 0);
        for stage in &lc.spec.stages {
            assert!(
                !stage.acceptance.queue_stages.is_empty(),
                "ticket {} has no queue acceptance",
                stage.id
            );
            assert!(
                stage
                    .acceptance
                    .queue_stages
                    .iter()
                    .all(|id| (1..=9).contains(id)),
                "ticket {} acceptance out of the queue suite",
                stage.id
            );
        }
        assert_eq!(lc.spec.stages[0].acceptance.queue_stages, vec![1]);
        assert_eq!(
            lc.spec.stages[8].acceptance.queue_stages,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
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

    #[test]
    fn r23_d2_pool_budget_splits_by_tier() {
        assert!((pool_budget(0.9, 2, 6) - 0.3).abs() < 1e-9);
        assert_eq!(pool_budget(0.0, 2, 6), 0.0);
        assert_eq!(pool_budget(0.9, 0, 6), 0.0);
        assert_eq!(pool_budget(0.9, 2, 0), 0.0);
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

    /// R23 D3: a paid launch refuses a model with no list price; eval-only
    /// runs are exempt because they do not spend.
    #[tokio::test]
    async fn r23_d3_a_model_without_a_list_price_is_refused() {
        let dead = "http://127.0.0.1:9/models";
        assert!(
            require_model_list_price_at("google/gemini-2.5-flash", dead, false)
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
    fn test_real_sdb_http_spec_validates() {
        let lc = load_campaign("sdb-http").expect("bundled campaign must load");
        assert_eq!(lc.spec.name, "sdb-http");
        assert_eq!(lc.spec.task, "http");
        assert_eq!(lc.spec.stages.len(), 8);
        assert_eq!(lc.spec.stages[0].id, 0);
        for stage in &lc.spec.stages {
            assert!(
                !stage.acceptance.http_stages.is_empty(),
                "ticket {} has no http acceptance",
                stage.id
            );
            assert!(
                stage
                    .acceptance
                    .http_stages
                    .iter()
                    .all(|id| (1..=10).contains(id)),
                "ticket {} acceptance out of the http suite",
                stage.id
            );
        }
        assert_eq!(lc.spec.stages[0].acceptance.http_stages, vec![1, 2]);
        assert_eq!(
            lc.spec.stages[7].acceptance.http_stages,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
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
        let (ids, sev) =
            detect_regressions(&best, &[stage_result(1, false), stage_result(3, false)]);
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
            over_budget: false,
            cap_reached: false,
            over_wall: false,
            rules_version: crate::report::rules::RULES_VERSION.to_string(),
            budget_usd: 0.99,
            wall_min: 150,
            points: 10.0,
            max_points: 10.0,
            phase: Some(phase),
            chain_from: None,
            career: Some(CareerRollup {
                points: 10.0,
                total_cost_usd: 0.5,
                cells: vec!["prev".into()],
            }),
            ladder_state: None,
            harness_fault: None,
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
        // Once 11 has failed, 14 is decided: skipped (not waiting).
        rollup
            .stages
            .push(failed_record(11, "Indexes", "own stage: boom", ""));
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
        let seen =
            std::cell::RefCell::new(Vec::<(u32, Option<StageVariant>, Option<String>)>::new());
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
            "[TOKENS] Turn usage - input: 1, output: 65536, cached: 0"
        ));
        assert_eq!(failure.kind, "agent_crash");
        assert!(failure.line.contains("65,536"), "{}", failure.line);
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
}
