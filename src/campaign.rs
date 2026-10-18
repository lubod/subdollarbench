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
    /// Every verification round of this attempt (initial + repairs).
    pub rounds: Vec<RoundRecord>,
    /// The classified cause of the final round when the attempt failed.
    pub failure: Option<FailureSummary>,
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
    /// fault | no_start | regression | late_check | own_stage | agent
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

fn truncate_line(s: &str, n: usize) -> String {
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
    let first = failed.first().copied();
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
    // 3. regression: an earlier green ticket fails now. Results the
    // verifier skipped ("skipped due to failure in earlier stages") only
    // follow the real break; they are counted, not blamed (round-7
    // finding 3: ticket 6 broke stage 0, not stages 0,1,3,4,5).
    let is_skipped = |s: &crate::verifier::StageResult| {
        s.error
            .as_deref()
            .map(|e| e.starts_with("skipped due to failure"))
            .unwrap_or(false)
    };
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
    if !failed.is_empty() && failed.iter().all(|s| s.stage == stage_id) {
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
    /// pass | lite | fail | fault | skipped | not_reached
    pub status: String,
    /// full | lite: the variant of the record that decided the status.
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub points: f64,
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
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        budget_usd: live["campaign_budget"].as_f64().unwrap_or(0.0),
        wall_min: v["wall_cap_min"].as_u64().unwrap_or(0),
        points: 0.0,
        max_points: 0.0,
    }
}

/// The argv `POST /api/campaigns` spawns: one place, and the CLI parse
/// test pins that the real clap definition accepts it (round-8 finding 2).
pub fn launcher_args(model: &str, effort: &str, budget_usd: f64, wall_min: u64) -> Vec<String> {
    [
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
        "20",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
        let current = rollup.rules_version == crate::report::rules::RULES_VERSION;
        Self {
            live: false,
            current_rules: current,
            points: rollup.points,
            per_dollar: (rollup.total_cost_usd > 0.0)
                .then(|| rollup.points / rollup.total_cost_usd),
            cost: rollup.total_cost_usd,
        }
    }
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
    if rollup.over_budget {
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
    let passed: Vec<u32> = rollup
        .stages
        .iter()
        .filter(|r| r.passed)
        .map(|r| r.id)
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
                ("skipped", format!("skipped: needs {list}"))
            } else if rollup.over_budget {
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
                cause: Some(cause),
            }
        };
        out.push(entry);
    }
    out
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
    // Which stages passed on a lite variant (stage -> profile).
    let mut liters: std::collections::BTreeMap<u32, Vec<String>> =
        std::collections::BTreeMap::new();
    // Every stage passed so far (full or lite): the declared acceptance
    // list is intersected with it, so a failed stage sinks nothing later.
    let mut passed_all: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut dirty = false;
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
        reached: None,
        regression_free_tickets: 0,
        total_cost_usd: 0.0,
        over_budget: false,
        over_wall: false,
        rules_version: crate::report::rules::RULES_VERSION.to_string(),
        budget_usd: opts.budget_usd,
        wall_min: opts.wall_min.unwrap_or(0),
        points: 0.0,
        max_points: max_points(&lc.spec.stages),
    };

    for stage in &lc.spec.stages {
        // Caps are checked between tickets; both flags are set when both
        // were crossed at the same boundary (round-7 finding 5; the stop
        // reason stays budget-first).
        let over_budget_now = opts.budget_usd > 0.0 && total_cost >= opts.budget_usd;
        let over_wall_now = opts
            .wall_min
            .map(|m| m > 0 && wall_start.elapsed() >= std::time::Duration::from_secs(m * 60))
            .unwrap_or(false);
        if over_budget_now || over_wall_now {
            rollup.over_budget |= over_budget_now;
            rollup.over_wall |= over_wall_now;
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
            lite_profile: Vec::new(),
            passed_lite: liters.iter().map(|(k, v)| (*k, v.clone())).collect(),
            passed: passed_all.iter().copied().collect(),
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
            // commit (and the data dir) before the fresh session.
            let attempt_lite = StageAttempt {
                stage,
                variant: StageVariant::Lite,
                prompt_path: lc.root.join(&lite.prompt),
                budget_usd: lite.budget_usd,
                max_turns: lite.max_turns,
                lite_profile: lite.profile.clone(),
                passed_lite: liters.iter().map(|(k, v)| (*k, v.clone())).collect(),
                passed: passed_all.iter().copied().collect(),
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
            over_wall: false,
            rules_version: String::new(),
            budget_usd: 0.0,
            wall_min: 0,
            points: 0.0,
            max_points: 0.0,
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
            over_wall: false,
            rules_version: crate::report::rules::RULES_VERSION.to_string(),
            budget_usd: 0.25,
            wall_min: 30,
            points: 1.0,
            max_points: 4.0,
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
            turns: Some(10),
            duration_seconds: Some(60.0),
            agent_failed: false,
            infra_error: false,
            rounds: Vec::new(),
            failure: None,
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
