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
use std::collections::{HashMap, HashSet};
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
    pub fn render_spec_md(&self, stage: &StageSpec, current_prompt: &Path) -> Result<String> {
        let mut out = format!(
            "# SPEC.md — {}\n\n\
             Harness-owned: rewritten before every ticket, so edits here are \
             discarded. It is every ticket released so far, oldest first; a \
             later ticket extends the earlier ones and wins where they \
             differ. The last section is the ticket you are working on now.\n",
            self.spec.title
        );
        let mut push = |title: &str, path: &Path| -> Result<()> {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("SPEC.md: read {}", path.display()))?;
            out.push_str(&format!(
                "\n---\n\n<!-- {title} -->\n\n{}\n",
                text.trim_end()
            ));
            Ok(())
        };
        for s in self.spec.stages.iter().filter(|s| s.id < stage.id) {
            push(
                &format!("ticket {}: {}", s.id, s.title),
                &self.prompt_path(s),
            )?;
        }
        push(
            &format!("ticket {} (current): {}", stage.id, stage.title),
            current_prompt,
        )?;
        Ok(out)
    }
}

/// Stage config handed to `BenchmarkPipeline`: prompt override + workspace
/// continuity + the acceptance subset that decides this stage's verdict.
#[derive(Debug, Clone)]
pub struct CampaignStageConfig {
    pub campaign: String,
    pub stage: u32,
    pub prompt_path: PathBuf,
    /// true when the workspace already exists and must be preserved
    /// (every stage after genesis).
    pub continuity: bool,
    pub accepted_stages: Vec<u32>,
    /// sdb-sql: the harness-owned `SPEC.md`, written into the workspace
    /// after the pipeline prepares it (a genesis workspace is wiped first,
    /// so the campaign runner cannot write it beforehand).
    pub spec_md: Option<String>,
}

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
    Ok(())
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
        .filter(|r| !r.passed && best.get(&r.stage).copied().unwrap_or(false))
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
    /// Score weight of a pass (1.0 full, the lite spec's weight otherwise).
    pub weight: f64,
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
    pub turns: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub agent_failed: bool,
    pub infra_error: bool,
    pub disqualified: bool,
    pub git_commit: Option<String>,
    pub git_stat: Option<String>,
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
    /// Budget cap reached before the ladder finished (never a model failure).
    pub over_budget: bool,
    /// Optional wall-clock cap reached before the ladder finished.
    #[serde(default)]
    pub over_wall: bool,
    /// Difficulty-weighted points actually scored (tier x weight per pass).
    #[serde(default)]
    pub points: f64,
    /// Ceiling: every stage full (sum of tiers).
    #[serde(default)]
    pub max_points: f64,
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
    /// Campaign-wide cap: the ladder stops (over_budget) once cumulative
    /// effective cost reaches it.
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
pub async fn run_campaign<F, Fut>(
    lc: &LoadedCampaign,
    opts: &CampaignRunOptions,
    mut exec: F,
    on_stage: Option<&dyn Fn(&CampaignRollup)>,
) -> Result<CampaignRollup>
where
    F: FnMut(&StageAttempt<'_>) -> Fut,
    Fut: std::future::Future<Output = Result<StageOutcome>>,
{
    let started_at = chrono::Utc::now().to_rfc3339();
    let wall_start = std::time::Instant::now();
    let mut best: HashMap<u32, bool> = HashMap::new();
    let mut reached: Option<u32> = None;
    let mut total_cost = 0.0f64;
    // Ladder bookkeeping (continue mode): per-stage outcome + whether the
    // last attempt left the workspace dirty (needs a rollback first).
    let mut outcomes: HashMap<u32, LadderOutcome> = HashMap::new();
    let mut dirty = false;
    let mut rollup = CampaignRollup {
        campaign_id: opts.campaign_id.clone(),
        name: lc.spec.name.clone(),
        model: opts.model.clone(),
        harness: opts.harness.clone(),
        effort: opts.effort.clone(),
        task: lc.spec.task.clone(),
        started_at: started_at.clone(),
        finished_at: started_at,
        stages: Vec::new(),
        reached: None,
        regression_free_tickets: 0,
        total_cost_usd: 0.0,
        over_budget: false,
        over_wall: false,
        points: 0.0,
        max_points: max_points(&lc.spec.stages),
    };

    for stage in &lc.spec.stages {
        if opts.budget_usd > 0.0 && total_cost >= opts.budget_usd {
            rollup.over_budget = true;
            if let Some(cb) = on_stage {
                cb(&rollup);
            }
            break;
        }
        if let Some(cap_min) = opts.wall_min {
            if cap_min > 0 && wall_start.elapsed() >= std::time::Duration::from_secs(cap_min * 60) {
                rollup.over_wall = true;
                if let Some(cb) = on_stage {
                    cb(&rollup);
                }
                break;
            }
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
            budget_usd: stage.budget_usd,
            max_turns: stage.max_turns,
            weight: 1.0,
            rollback_first: dirty,
        };
        let out = exec(&attempt_full)
            .await
            .with_context(|| format!("stage {} ({})", stage.id, stage.title))?;
        let (regressed, regression_severity) = detect_regressions(&best, &out.stage_results);
        for r in &out.stage_results {
            let e = best.entry(r.stage).or_insert(false);
            *e = *e || r.passed;
        }
        total_cost += out.effective_cost_usd;
        let fault = out.agent_failed || out.infra_error;
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
        if out.passed {
            outcomes.insert(stage.id, LadderOutcome::Full);
            reached = Some(stage.id);
            dirty = false;
        } else if fault {
            // A fault measured nothing: recorded, never a model failure,
            // but it cannot satisfy `needs`.
            outcomes.insert(stage.id, LadderOutcome::Failed);
            dirty = true;
        } else if let Some(lite) = &stage.lite {
            // Lite attempt: the workspace rolls back to the last green
            // commit (and the data dir) before the fresh session.
            let attempt_lite = StageAttempt {
                stage,
                variant: StageVariant::Lite,
                prompt_path: lc.root.join(&lite.prompt),
                budget_usd: lite.budget_usd,
                max_turns: lite.max_turns,
                weight: lite.weight,
                rollback_first: true,
            };
            let out2 = exec(&attempt_lite)
                .await
                .with_context(|| format!("stage {} lite", stage.id))?;
            let (regressed2, severity2) = detect_regressions(&best, &out2.stage_results);
            for r in &out2.stage_results {
                let e = best.entry(r.stage).or_insert(false);
                *e = *e || r.passed;
            }
            total_cost += out2.effective_cost_usd;
            let fault2 = out2.agent_failed || out2.infra_error;
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
            if out2.passed {
                outcomes.insert(stage.id, LadderOutcome::Lite);
                if reached.map(|r| stage.id > r).unwrap_or(true) {
                    reached = Some(stage.id);
                }
                dirty = false;
            } else {
                outcomes.insert(stage.id, LadderOutcome::Failed);
                dirty = true;
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

    rollup.finished_at = chrono::Utc::now().to_rfc3339();
    Ok(rollup)
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
        variant: match stage.lite {
            Some(_) => "full".to_string(),
            None => "full".to_string(),
        },
        tier: stage.tier,
        points: 0.0,
        run_id: format!("{}_skipped", stage.id),
        git_commit: None,
        git_stat: None,
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
    let points = match (status, variant) {
        ("passed", StageVariant::Full) => stage.tier as f64,
        ("passed", StageVariant::Lite) => {
            stage.tier as f64 * stage.lite.as_ref().map(|l| l.weight).unwrap_or(0.4)
        }
        _ => 0.0,
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
        variant: match variant {
            StageVariant::Full => "full".to_string(),
            StageVariant::Lite => "lite".to_string(),
        },
        tier: stage.tier,
        points,
        run_id: out.run_id.clone(),
        git_commit: out.git_commit.clone(),
        git_stat: out.git_stat.clone(),
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
        std::fs::write(
            workdir.join(".gitignore"),
            // Runtime state (AOF/dumps) is evidence on disk, never source:
            // committing it bloated the first replay's stage diffs (123k lines).
            "target/\nnode_modules/\n__pycache__/\n*.o\n*.tmp\nappendonly.log\n*.aof\n*.rdb\n",
        )?;
    }
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
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir_excluding_git(&from, &to)?;
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
            turns: Some(10),
            duration_seconds: Some(60.0),
            agent_failed: false,
            infra_error: false,
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
        };
        std::fs::write(
            dir.join("campaign.json"),
            serde_json::to_string_pretty(&spec).unwrap(),
        )
        .unwrap();
        LoadedCampaign { spec, root: dir }
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
        let md = lc.render_spec_md(s2, &lc.prompt_path(s2)).unwrap();
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
        let md0 = lc.render_spec_md(s0, &lc.prompt_path(s0)).unwrap();
        assert!(md0.contains("ticket 0 (current)") && !md0.contains("# Ticket 1"));
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
        };
        std::fs::write(
            dir.join("campaign.json"),
            serde_json::to_string_pretty(&spec).unwrap(),
        )
        .unwrap();
        LoadedCampaign { spec, root: dir }
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
        let rollup = run_campaign(
            &lc,
            &opts,
            |attempt| {
                let id = attempt.stage.id;
                let variant = attempt.variant;
                seen.borrow_mut()
                    .push((id, variant, attempt.rollback_first));
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
            budget_usd: 0.005, // one stage costs 0.01; the second never starts
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
        assert!(rollup.over_budget);
        assert_eq!(rollup.stages.len(), 1);
        assert_eq!(rollup.reached, Some(0));
    }

    #[tokio::test]
    async fn test_orchestrator_records_regression_and_fault() {
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
        assert_eq!(s1.status, "fault");
        assert_eq!(rollup.regression_free_tickets, 1);
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
