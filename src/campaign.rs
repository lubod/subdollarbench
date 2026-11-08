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

pub use crate::scoring::{
    cap_allows, cap_headroom, cap_reserve, default_feedback_rounds, default_lite_weight,
    default_tier, max_points, pool_budget, repair_round_spend, runnable_weight_sum, score_ladder,
    stage_lite_weight, stage_points, stage_weight, LadderScore, BUDGET_FLOOR_USD, CAP_RESERVE_USD,
    CATCH_UP_SLOTS, CATCH_UP_WEIGHT, CELL_BUDGET_USD, CELL_WALL_MIN, DEFAULT_LITE_WEIGHT,
    DEFAULT_TIER, TICKET_WALL_GUARD_MIN, TURN_GUARD,
};

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
#[allow(clippy::too_many_arguments)]
pub fn validate_chain(
    prev: &CampaignRollup,
    state: Option<&LadderState>,
    phase: u32,
    model: &str,
    harness: &str,
    effort: &str,
    rules_version: &str,
    sandbox_image: Option<&str>,
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
    if prev.sandbox_image.as_deref() != sandbox_image {
        bail!(
            "chain cell {} ran image {}, not {}",
            prev.campaign_id,
            prev.sandbox_image.as_deref().unwrap_or("—"),
            sandbox_image.unwrap_or("—")
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
        perf: prev.perf.clone(),
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
    /// R67 F2: the provider's own bill from the session's `usage.cost.total`,
    /// beside the list-price `api_cost_usd`.
    #[serde(default)]
    pub billed_usd: f64,
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
    /// R36/R37: the sandbox image this attempt's manifest recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_image: Option<String>,
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
    /// R48 F2: seconds this attempt waited for the host grading lock.
    #[serde(default)]
    pub grading_wait_s: f64,
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
    /// R25 R23-8: the provider API bill for this ticket. This is the unit
    /// the budget and cap use; `effective_cost_usd` adds the compute floor.
    #[serde(default)]
    pub api_cost_usd: f64,
    /// R67 F2: the provider's own bill for this ticket, beside the list.
    #[serde(default)]
    pub billed_usd: f64,
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
    /// R48 F2: seconds this ticket waited for the host grading lock.
    #[serde(default)]
    pub grading_wait_s: f64,
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
    /// R26 R23-7: why this session ended (dollars | turns | wall | ready |
    /// crash), recorded per session, not only for the attempt's last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
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
/// R22-1: one `[TOKENS]` line's output count. The old detector matched
/// loose text (`not valid JSON`, `output limit`) anywhere in the console,
/// so a recovered tool-call error could mislabel a later crash.
fn token_output(line: &str) -> Option<u64> {
    if !line.contains("[TOKENS]") {
        return None;
    }
    line.split("output:")
        .nth(1)?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

fn is_provider_400(line: &str) -> bool {
    line.contains("400 Provider returned error")
        || line.contains("[STDERR] 400")
        || line.contains("HTTP 400")
}

fn format_token_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// R22-1/R25-1: label a crash only when the **last** `[TOKENS]` turn before
/// the provider `400` is the session's largest output. A truncation breaks
/// the tool call in that turn, so the next provider response is the 400; if
/// another turn sits between the largest output and the 400, the 400 is a
/// different failure. The number in the line is the measured output, not a
/// hard-coded 65,536.
pub fn mark_agent_crash(failure: &mut FailureSummary, console: &str) -> bool {
    // A console can hold more than one agent session (the initial attempt
    // and a repair); a later recovered error must not pair with an earlier
    // session's truncation.
    let mut sessions: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in console.lines() {
        if line.contains("[LIVE] Agent initialized") && !current.is_empty() {
            sessions.push(std::mem::take(&mut current));
        }
        current.push(line);
    }
    if !current.is_empty() {
        sessions.push(current);
    }
    for lines in sessions {
        let max_output = lines
            .iter()
            .filter_map(|line| token_output(line))
            .max()
            .unwrap_or(0);
        if max_output == 0 {
            continue;
        }
        for (i, line) in lines.iter().enumerate() {
            if !is_provider_400(line) {
                continue;
            }
            // The 400 must directly follow the truncated turn: the last
            // [TOKENS] line before it has to be the session's largest.
            let last_tokens = lines[..i]
                .iter()
                .rposition(|line| token_output(line).is_some());
            if last_tokens
                .and_then(|j| token_output(lines[j]))
                .is_some_and(|output| output == max_output)
            {
                failure.kind = "agent_crash".to_string();
                failure.line = format!(
                    "agent hit the {}-token output limit and crashed",
                    format_token_count(max_output)
                );
                return true;
            }
        }
    }
    false
}

/// R53 F2: two cells that start the same model in the same second on
/// different workers get distinct ids. The timestamp stays readable by the
/// grid, the board and `--chain`.
pub fn campaign_id_for(campaign: &str, model_tag: &str, timestamp: &str, worker: u8) -> String {
    format!("campaign_{campaign}_{model_tag}_{timestamp}_w{worker}")
}

/// R53 F2: create the campaign directory exclusively, so two writers can
/// never share one even if their ids collide.
pub fn create_campaign_dir(runs: &std::path::Path, campaign_id: &str) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(runs)
        .map_err(|e| anyhow::anyhow!("create runs dir {}: {e}", runs.display()))?;
    let dir = runs.join(campaign_id);
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok(dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
            "campaign directory {} already exists (another cell started in the same second?)",
            dir.display()
        ),
        Err(e) => anyhow::bail!("create campaign dir {}: {e}", dir.display()),
    }
}

/// The campaign lock (round-7 finding 1): one campaign at a time, whether
/// started from the CLI (`scripts/sql-cell.sh`) or the console. A small
/// JSON file holding `{pid, campaign_id}`; a lock whose holder is gone is
/// stale and may be replaced. Every campaign path takes it, so the GUI,
/// the queue and the CLI all agree on who is running.
pub fn campaign_lock_path_for(worker: u8) -> std::path::PathBuf {
    crate::report::archive::RunArchiver::resolve_runs_dir()
        .join(format!(".campaign.lock.w{worker}"))
}

/// The pre-round-53 single lock; still read as a live holder until its
/// process exits.
pub fn legacy_campaign_lock_path() -> std::path::PathBuf {
    crate::report::archive::RunArchiver::resolve_runs_dir().join(".campaign.lock")
}

/// R53 F1: worker 0's lock path (the old name's closest equivalent).
pub fn campaign_lock_path() -> std::path::PathBuf {
    campaign_lock_path_for(0)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub campaign_id: String,
    /// R53 F1: the worker whose lock this is (legacy locks default 0).
    #[serde(default)]
    pub worker: u8,
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

/// Every live campaign-lock holder: each worker's lock plus a legacy
/// single lock. Dead holders are ignored.
pub fn live_lock_holders() -> Vec<LockInfo> {
    let runs = crate::report::archive::RunArchiver::resolve_runs_dir();
    live_lock_holders_in(&runs)
}

pub fn live_lock_holders_in(runs: &std::path::Path) -> Vec<LockInfo> {
    let mut out = Vec::new();
    // R56 F3/F4/F6: test lanes use 8 and 9; scan them too so a tool's
    // beside-cell check and the concurrent-lane count see a test lock.
    for worker in 0..=9u8 {
        if let Some(mut h) = live_lock_holder_at(&runs.join(format!(".campaign.lock.w{worker}"))) {
            // A test may write a lock file by hand without the field.
            if h.worker != worker && h.worker == 0 {
                h.worker = worker;
            }
            out.push(h);
        }
    }
    if let Some(h) = live_lock_holder_at(&runs.join(".campaign.lock")) {
        out.push(h);
    }
    out
}

/// R56 F4: the lanes live at this instant: this worker plus every other
/// live lock's worker. At least 1.
pub fn live_lane_count_in(runs: &std::path::Path, worker: u8) -> u32 {
    let mut workers = std::collections::BTreeSet::new();
    workers.insert(worker);
    for h in live_lock_holders_in(runs) {
        if h.worker != worker {
            workers.insert(h.worker);
        }
    }
    workers.len() as u32
}

/// The holder of one worker's lock, if live (legacy locks count as
/// worker 0's).
pub fn live_lock_for_worker(worker: u8) -> Option<LockInfo> {
    live_lock_holders().into_iter().find(|h| h.worker == worker)
}

/// The first live holder, for old clients.
pub fn live_lock_holder() -> Option<LockInfo> {
    live_lock_holders().into_iter().next()
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
    pub fn acquire(campaign_id: &str, worker: u8) -> Result<CampaignLock, String> {
        Self::try_acquire_at_worker(&campaign_lock_path_for(worker), campaign_id, worker)
    }

    /// The same acquisition against an explicit path (tests use a temp
    /// file so parallel tests never touch the real lock).
    pub fn try_acquire_at(
        path: &std::path::Path,
        campaign_id: &str,
    ) -> Result<CampaignLock, String> {
        Self::try_acquire_at_worker(path, campaign_id, 0)
    }

    pub fn try_acquire_at_worker(
        path: &std::path::Path,
        campaign_id: &str,
        worker: u8,
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
                        worker,
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
    let lock_live = live_lock_holders()
        .iter()
        .any(|h| h.campaign_id == campaign_id);
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

/// R22-5: apply F2's `fault` -> `failed` relabel to every loaded rollup,
/// independently of the rounds backfill. The rule is unchanged: the stage
/// manifest says `agent_failed`, and the cell has no `harness_fault`.
pub fn relabel_graded_crashes(rollup: &mut CampaignRollup, campaign_dir: &std::path::Path) {
    if rollup.harness_fault.is_some() {
        return;
    }
    for rec in &mut rollup.stages {
        if rec.status != "fault" {
            continue;
        }
        let manifest: Option<serde_json::Value> = std::fs::read_to_string(
            campaign_dir
                .join(format!("s{}_{}", rec.id, rec.variant))
                .join("manifest.json"),
        )
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
        if manifest.as_ref().and_then(|m| m["status"].as_str()) == Some("agent_failed") {
            rec.status = "failed".to_string();
        }
    }
}

/// Fill `rounds`/`failure` on records written before they were recorded
/// (gui-spec.md §6.1): read each attempt's manifest from the campaign dir
/// and mark the derived rounds. The crash relabel is applied first, because
/// it also applies to cells with recorded rounds.
pub fn backfill_rounds(rollup: &mut CampaignRollup, campaign_dir: &std::path::Path) {
    relabel_graded_crashes(rollup, campaign_dir);
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
            .unwrap_or(false);
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
                    stop_reason: r.stop_reason.clone(),
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
                        stop_reason: None,
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
                    stop_reason: None,
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
                stop_reason: rec.stop_reason.clone(),
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

/// R36-3: the Board used to run `docker image inspect` for every live cell
/// on every request. The cell's own status (written at launch and mirrored
/// from each rollup) is the primary source; a finished ticket's manifest is
/// the second. Only a cell with neither falls back to one process-wide
/// cached `sandbox_image_id()`, refreshed at most once a minute. So the
/// async handler never runs the subprocess per request, and it never shows
/// today's image when the run recorded its own.
fn live_cell_image(
    campaign_id: &str,
    v: &serde_json::Value,
    runs_dir: &Path,
    fallback: &dyn Fn() -> Option<String>,
) -> Option<String> {
    if let Some(img) = v
        .pointer("/live/sandbox_image")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            v.pointer("/rollup/sandbox_image")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| v.get("sandbox_image").and_then(serde_json::Value::as_str))
    {
        return Some(img.to_string());
    }
    // A cell that already archived a ticket carries the image in its
    // manifest (`s<id>_<variant>/manifest.json`). Read them in name order so
    // the first ticket's image wins, as the rollup does.
    let clean = Path::new(campaign_id)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(campaign_id);
    if let Ok(entries) = std::fs::read_dir(runs_dir.join(clean)) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) else {
                continue;
            };
            if let Ok(m) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(img) = m.get("sandbox_image").and_then(|i| i.as_str()) {
                    return Some(img.to_string());
                }
            }
        }
    }
    fallback()
}

/// One cache for the Board's fallback image lookup. `None` means no lookup
/// yet (or its 60 s freshness window passed). R51 F7: the mutex is released
/// before the lookup (a `docker` subprocess) and re-taken to store, so two
/// threads on an empty cache inspect concurrently instead of serializing.
struct ImageCache(std::sync::Mutex<Option<(std::time::Instant, Option<String>)>>);

impl ImageCache {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    fn resolve(&self, lookup: &dyn Fn() -> Option<String>) -> Option<String> {
        if let Some((_, id)) = {
            let cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .as_ref()
                .filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(60))
                .cloned()
        } {
            return id;
        }
        let id = lookup();
        let mut cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some((std::time::Instant::now(), id.clone()));
        id
    }
}

/// The process-wide cache production uses.
static SANDBOX_IMAGE_CACHE: ImageCache = ImageCache::new();

fn cached_sandbox_image_id() -> Option<String> {
    cached_sandbox_image_id_with(&crate::sandbox::omp_updater::sandbox_image_id)
}

fn cached_sandbox_image_id_with(lookup: &dyn Fn() -> Option<String>) -> Option<String> {
    SANDBOX_IMAGE_CACHE.resolve(lookup)
}

/// A rollup-shaped stub for a running cell that has no rollup yet (its
/// first ticket is still in flight; round-8 finding 8). The live block the
/// runner writes carries the model, harness, effort and budgets.
pub fn live_rollup_stub(campaign_id: &str, v: &serde_json::Value) -> CampaignRollup {
    live_rollup_stub_at(
        campaign_id,
        v,
        &crate::report::archive::RunArchiver::resolve_runs_dir(),
    )
}

/// R36-3: the testable form of the stub. `runs_dir` is where the cell's
/// per-ticket manifests live; a test points it at a fixture.
fn live_rollup_stub_at(
    campaign_id: &str,
    v: &serde_json::Value,
    runs_dir: &Path,
) -> CampaignRollup {
    live_rollup_stub_at_with(campaign_id, v, runs_dir, &cached_sandbox_image_id)
}

/// R51 F7: the injected form. `fallback` resolves the image only when the
/// cell's status and manifests have none.
fn live_rollup_stub_at_with(
    campaign_id: &str,
    v: &serde_json::Value,
    runs_dir: &Path,
    fallback: &dyn Fn() -> Option<String>,
) -> CampaignRollup {
    let live = &v["live"];
    let s = |k: &str, fb: &str| live[k].as_str().unwrap_or(fb).to_string();
    CampaignRollup {
        campaign_id: campaign_id.to_string(),
        name: s("campaign", "sdb-sql"),
        model: s("model", "?"),
        harness: s("harness", "omp"),
        // R51 F6 (R43-1): an unknown effort stays empty, never "high".
        effort: s("effort", ""),
        task: "sql".to_string(),
        started_at: v["started_at"].as_str().unwrap_or("").to_string(),
        finished_at: String::new(),
        stages: Vec::new(),
        reached: None,
        regression_free_tickets: 0,
        total_cost_usd: live["campaign_spent"].as_f64().unwrap_or(0.0),
        billed_usd: 0.0,
        over_budget: false,
        cap_reached: false,
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        sqlite_version: Some(crate::report::rules::sqlite_version().to_string()),
        cap_price_basis: Some(crate::report::rules::CAP_PRICE_BASIS.to_string()),
        sandbox_image: live_cell_image(campaign_id, v, runs_dir, fallback),
        budget_usd: live["campaign_budget"].as_f64().unwrap_or(0.0),
        wall_min: v["wall_cap_min"].as_u64().unwrap_or(0),
        worker: 0,
        concurrent_cells: 1,
        grading_wait_s: 0.0,
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
        perf: None,
        refork: if v["live"]["refork"].is_null() {
            None
        } else {
            serde_json::from_value(v["live"]["refork"].clone()).ok()
        },
        ladder_state: None,
        harness_fault: None,
        rehearsal: live["rehearsal"].as_bool().unwrap_or(false),
        smoke: live["smoke"].as_bool().unwrap_or(false),
    }
}

/// R23 D3/R24: a paid campaign launch must have a price in the committed
/// snapshot (`src/cost/model_prices.json`); eval-only runs do not spend and
/// are exempt. `_at` is the offline-testable form; the URL is ignored.
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
        .is_none()
    {
        return Err(format!(
            "model {model} has no entry in the price snapshot src/cost/model_prices.json; add the model's catalog id there, then retry the paid campaign launch"
        ));
    }
    if crate::cost::calculator::provider_for_model(model).is_none() {
        return Err(format!(
            "model {model} has no pinned provider in src/cost/model_prices.json; add the provider endpoint and its prices, then retry the paid campaign launch"
        ));
    }
    Ok(())
}

pub async fn require_model_list_price(model: &str, eval_only: bool) -> Result<(), String> {
    require_model_list_price_at(model, "https://openrouter.ai/api/v1/models", eval_only).await
}

/// The argv `POST /api/campaigns` spawns: one place, and the CLI parse
/// test pins that the real clap definition accepts it (round-8 finding 2).
pub fn launcher_args(model: &str, effort: &str, budget_usd: f64, wall_min: u64) -> Vec<String> {
    launcher_args_with_phase(model, effort, budget_usd, wall_min, None, None, None, 0)
}

/// Launcher argv with the optional career phase/chain (P1). Kept separate
/// so the existing launcher callers and their CLI-parse test stay stable.
#[allow(clippy::too_many_arguments)]
pub fn launcher_args_with_phase(
    model: &str,
    effort: &str,
    budget_usd: f64,
    wall_min: u64,
    phase: Option<u32>,
    chain: Option<&str>,
    refork: Option<&str>,
    worker: u8,
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
    args.push("--worker".into());
    args.push(worker.to_string());
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
    /// R47 F1: the career's latest measured perf score (`career.perf`), or
    /// the phase's own when the cell has no career block. Breaks points
    /// ties; a score sorts above no score.
    #[serde(default)]
    pub perf: Option<f64>,
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
        // R47 F1: the career's perf score, or the phase's own for a cell
        // with no career block.
        let perf = match &rollup.career {
            Some(c) => c.perf.as_ref().and_then(|p| p.score),
            None => rollup.perf.as_ref().and_then(|p| p.score),
        };
        Self {
            live: false,
            current_rules: current,
            points,
            perf,
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

/// The board order: current rules first, then points desc, then perf desc
/// (a score above none), then points per dollar desc, then cheaper.
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
            // Higher perf first; Some sorts above None (Option's order).
            b.perf
                .partial_cmp(&a.perf)
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
                // R22-2: a failed full does not decide a need that still
                // has its lite to run. The need is decided only once it has
                // failed and either has no lite or already has a lite
                // record (a passed lite would have removed it from `unmet`).
                let decided = unmet.iter().any(|n| {
                    let records: Vec<&StageRecord> =
                        rollup.stages.iter().filter(|r| r.id == *n).collect();
                    let failed = records.iter().any(|r| !r.passed);
                    let has_lite = spec.stages.iter().any(|s| s.id == *n && s.lite.is_some());
                    let has_lite_record = records.iter().any(|r| r.variant == "lite");
                    failed && (!has_lite || has_lite_record)
                });
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
    /// R45: the perf score for this career, once a phase provided one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf: Option<crate::verifier::sql::perfscore::PerfScore>,
}

fn one_cell() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignRollup {
    pub campaign_id: String,
    pub name: String,
    pub model: String,
    pub harness: String,
    /// R51 F3: an old/partial rollup may not record effort; it stays empty,
    /// never defaulted to "high".
    #[serde(default)]
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
    /// R67 F2: the phase's provider bill summed over its tickets.
    #[serde(default)]
    pub billed_usd: f64,
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
    /// R28-1: the oracle and cap policy recorded when this cell was graded.
    /// Absent on old cells; readers show `—`/no label rather than today's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_price_basis: Option<String>,
    /// R36 F1/D1: the sandbox image this cell ran on. A phase chains only
    /// onto the same image ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_image: Option<String>,
    /// Campaign-wide budget cap in USD (0 = uncapped), for the Run view's
    /// spend bar.
    #[serde(default)]
    pub budget_usd: f64,
    /// Wall-clock cap in minutes (0 = uncapped).
    #[serde(default)]
    pub wall_min: u64,
    /// R48 F1: the worker lane this cell ran on (0-7).
    #[serde(default)]
    pub worker: u8,
    /// R48 F5: how many cells were running at once during this phase.
    #[serde(default = "one_cell")]
    pub concurrent_cells: u32,
    /// R48 F2/F5: summed seconds spent waiting for the grading lock, not
    /// charged to the wall cap (D2 a).
    #[serde(default)]
    pub grading_wait_s: f64,
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
    /// R45: the perf score measured on this phase's final server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf: Option<crate::verifier::sql::perfscore::PerfScore>,
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
    /// R54 F1: a $0 launch of the real path with the reference candidate.
    #[serde(default)]
    pub rehearsal: bool,
    /// R54 F1: a sub-$0.99 smoke cell; never ranked, and its career is not.
    #[serde(default)]
    pub smoke: bool,
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
    /// R45: store the phase's perf block and carry it into the career
    /// block. The latest phase's score is the career's current perf (the
    /// board's tie-break); each phase keeps its own block in its own
    /// campaign.json.
    pub fn set_perf(&mut self, perf: crate::verifier::sql::perfscore::PerfScore) {
        // R49 F4 (R45-6): a harness-fault block (no score) stays on the
        // phase, but the career keeps its latest scored phase.
        let scored = perf.score.is_some();
        self.perf = Some(perf.clone());
        if scored {
            if let Some(career) = self.career.as_mut() {
                career.perf = Some(perf);
            }
        }
    }

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

/// R48 F2 (D2 a): the wall cap is charged only for time spent working,
/// not for time spent waiting for the grading lock.
pub fn wall_exceeded(
    elapsed: std::time::Duration,
    wall_min: Option<u64>,
    grading_wait_s: f64,
) -> bool {
    let wait = std::time::Duration::from_secs_f64(grading_wait_s.max(0.0));
    let worked = elapsed.saturating_sub(wait);
    wall_min
        .map(|m| m > 0 && worked >= std::time::Duration::from_secs(m * 60))
        .unwrap_or(false)
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
    run_campaign_ctx(lc, opts, None, None, None, None, None, exec, on_stage).await
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
    run_campaign_phase_on_image(
        lc, opts, phase, chain, chain_from, career, None, exec, on_stage,
    )
    .await
}

/// R37-5: the same phase run, seeded with the image the chain cell ran on.
/// A later ticket on a different image then turns the rollup `mixed` (see
/// `push_attempt_record`), and `validate_chain` refuses to continue it.
#[allow(clippy::too_many_arguments)]
pub async fn run_campaign_phase_on_image<F, Fut>(
    lc: &LoadedCampaign,
    opts: &CampaignRunOptions,
    phase: Option<&PhaseSpec>,
    chain: Option<&LadderState>,
    chain_from: Option<&str>,
    career: Option<&CareerRollup>,
    chain_image: Option<&str>,
    exec: F,
    on_stage: Option<&dyn Fn(&CampaignRollup)>,
) -> Result<CampaignRollup>
where
    F: FnMut(&StageAttempt<'_>) -> Fut,
    Fut: std::future::Future<Output = Result<StageOutcome>>,
{
    run_campaign_ctx(
        lc,
        opts,
        phase,
        chain,
        chain_from,
        career,
        chain_image,
        exec,
        on_stage,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_campaign_ctx<F, Fut>(
    lc: &LoadedCampaign,
    opts: &CampaignRunOptions,
    phase: Option<&PhaseSpec>,
    chain: Option<&LadderState>,
    chain_from: Option<&str>,
    career: Option<&CareerRollup>,
    chain_image: Option<&str>,
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
        billed_usd: 0.0,
        over_budget: false,
        cap_reached: false,
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        sqlite_version: Some(crate::report::rules::sqlite_version().to_string()),
        cap_price_basis: Some(crate::report::rules::CAP_PRICE_BASIS.to_string()),
        sandbox_image: chain_image.map(str::to_string),
        budget_usd: opts.budget_usd,
        wall_min: opts.wall_min.unwrap_or(0),
        worker: 0,
        concurrent_cells: 1,
        grading_wait_s: 0.0,
        points: 0.0,
        max_points: phase
            .map(|p| phase_max_points(p, &lc.spec.stages))
            .unwrap_or_else(|| max_points(&lc.spec.stages)),
        phase: phase.map(|p| p.id),
        chain_from: chain_from.map(str::to_string),
        career: None,
        perf: None,
        ladder_state: None,
        harness_fault: None,
        rehearsal: false,
        smoke: false,
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
            // R23 D2/R23-1: the catch-up draws from the phase pool; a lite
            // catch-up carries the lite's share, not the whole ticket.
            let target_weight = if variant == StageVariant::Lite {
                stage_lite_weight(stage)
            } else {
                stage.tier as f64
            };
            let catch_up_weight = runnable_weight_sum(&stage_list, &outcomes) + target_weight;
            let attempt = StageAttempt {
                stage,
                variant,
                prompt_path,
                budget_usd: pool_budget(headroom, target_weight, catch_up_weight),
                max_turns,
                lite_profile: lite_profile.clone(),
                passed_lite: liters.iter().map(|(k, v)| (*k, v.clone())).collect(),
                passed: passed_all.iter().copied().collect(),
                outcomes: outcomes_vec(&outcomes),
                weight: CATCH_UP_WEIGHT,
                cap_headroom_usd: (opts.budget_usd > 0.0).then_some(headroom),
                hints: chain_hints.clone(),
                catch_up: true,
                rollback_first: dirty,
            };
            // Q9: the shown phase ceiling includes the catch-up's possible
            // 0.4 x tier, so a full phase plus a catch-up cannot read >max.
            rollup.max_points += stage.tier as f64 * CATCH_UP_WEIGHT;
            let out = exec(&attempt)
                .await
                .with_context(|| format!("catch-up stage {} ({})", target, stage.title))?;
            rollup.grading_wait_s += out.grading_wait_s;
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
        let phase_weight = runnable_weight_sum(&stage_list, &outcomes);
        let outcomes_snapshot = |m: &HashMap<u32, LadderOutcome>| -> Vec<(u32, LadderOutcome)> {
            let mut v: Vec<(u32, LadderOutcome)> = m.iter().map(|(k, o)| (*k, *o)).collect();
            v.sort_by_key(|(id, _)| *id);
            v
        };
        let over_wall_now =
            wall_exceeded(wall_start.elapsed(), opts.wall_min, rollup.grading_wait_s);
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
            budget_usd: pool_budget(headroom, stage.tier as f64, phase_weight),
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
        rollup.grading_wait_s += out.grading_wait_s;
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
                        stage_lite_weight(stage),
                        runnable_weight_sum(&stage_list, &outcomes) - stage.tier as f64,
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
                rollup.grading_wait_s += out2.grading_wait_s;
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
        // R49 F4 (R45-3): this phase's perf is set after the ladder; the
        // career carries the previous phase's block unchanged. (The old
        // concatenation never ran and would have averaged duplicate
        // W1/W4 entries when it did.)
        let perf = prev.perf.clone();
        rollup.career = Some(CareerRollup {
            points: prev.points + rollup.points,
            total_cost_usd: prev.total_cost_usd + total_cost,
            cells,
            perf,
        });
    }
    rollup.finished_at = chrono::Utc::now().to_rfc3339();
    rollup.billed_usd = rollup.stages.iter().map(|r| r.billed_usd).sum();
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
    rollup.billed_usd = rollup.stages.iter().map(|r| r.billed_usd).sum();
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
        api_cost_usd: 0.0,
        billed_usd: 0.0,
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
        grading_wait_s: 0.0,
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
        stage.tier as f64 * CATCH_UP_WEIGHT
    } else {
        match (status, variant) {
            ("passed", StageVariant::Full) => stage.tier as f64,
            ("passed", StageVariant::Lite) => {
                stage.tier as f64
                    * stage
                        .lite
                        .as_ref()
                        .map(|l| l.weight)
                        .unwrap_or(DEFAULT_LITE_WEIGHT)
            }
            _ => 0.0,
        }
    };
    match (
        rollup.sandbox_image.as_deref(),
        out.sandbox_image.as_deref(),
    ) {
        (None, Some(image)) => rollup.sandbox_image = Some(image.to_string()),
        (Some(existing), Some(image)) if existing != image => {
            rollup.sandbox_image = Some("mixed".to_string());
        }
        _ => {}
    }
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
        api_cost_usd: out.api_cost_usd,
        billed_usd: out.billed_usd,
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
        grading_wait_s: out.grading_wait_s,
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

#[cfg(test)]
mod tests;
