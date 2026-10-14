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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Acceptance {
    pub redis_stages: Vec<u32>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignSpec {
    pub name: String,
    pub version: u32,
    pub task: String,
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
    if spec.task != "redis" {
        bail!(
            "campaign '{}': task '{}' is not supported yet (redis only until stage adapters land)",
            spec.name,
            spec.task
        );
    }
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
        let a = &s.acceptance.redis_stages;
        if a.is_empty() {
            bail!(
                "campaign '{}': stage {} has empty acceptance",
                spec.name,
                s.id
            );
        }
        let mut seen = HashSet::new();
        for id in a {
            if !(1..=13).contains(id) {
                bail!(
                    "campaign '{}': stage {} acceptance id {} out of range 1..=13",
                    spec.name,
                    s.id,
                    id
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
    pub status: String,
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
    /// Highest stage id whose ticket passed; None when ticket 0 failed.
    pub reached: Option<u32>,
    /// Tickets passed without any regression in their acceptance suite.
    pub regression_free_tickets: u32,
    pub total_cost_usd: f64,
    /// Budget cap reached before the ladder finished (never a model failure).
    pub over_budget: bool,
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
    F: FnMut(&StageSpec) -> Fut,
    Fut: std::future::Future<Output = Result<StageOutcome>>,
{
    let started_at = chrono::Utc::now().to_rfc3339();
    let mut best: HashMap<u32, bool> = HashMap::new();
    let mut reached: Option<u32> = None;
    let mut total_cost = 0.0f64;
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
    };

    for stage in &lc.spec.stages {
        if opts.budget_usd > 0.0 && total_cost >= opts.budget_usd {
            rollup.over_budget = true;
            if let Some(cb) = on_stage {
                cb(&rollup);
            }
            break;
        }
        let out = exec(stage)
            .await
            .with_context(|| format!("stage {} ({})", stage.id, stage.title))?;
        let (regressed, regression_severity) = detect_regressions(&best, &out.stage_results);
        for r in &out.stage_results {
            let e = best.entry(r.stage).or_insert(false);
            *e = *e || r.passed;
        }
        total_cost += out.effective_cost_usd;
        let status = if out.passed {
            "passed"
        } else if out.agent_failed || out.infra_error {
            // A fault measured nothing: recorded, never scored as a model
            // failure (same doctrine as the single-shot boards).
            "fault"
        } else {
            "failed"
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
            run_id: out.run_id,
            git_commit: out.git_commit,
            git_stat: out.git_stat,
        });
        if out.passed {
            reached = Some(stage.id);
        }
        // Progressive view for the GUI: the rollup-so-far is published
        // after every ticket (status.json writer in main_campaign).
        rollup.reached = reached;
        rollup.total_cost_usd = total_cost;
        rollup.regression_free_tickets = rollup
            .stages
            .iter()
            .filter(|r| r.passed && r.regressed.is_empty())
            .count() as u32;
        if let Some(cb) = on_stage {
            cb(&rollup);
        }
        if !out.passed {
            break;
        }
    }

    rollup.finished_at = chrono::Utc::now().to_rfc3339();
    Ok(rollup)
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
                    },
                    budget_usd: 0.1,
                    max_turns: 10,
                    feedback_rounds: 1,
                },
                StageSpec {
                    id: 1,
                    kind: "feature".into(),
                    title: "expiry".into(),
                    prompt: "stages/1.md".into(),
                    acceptance: Acceptance {
                        redis_stages: vec![1, 2, 3],
                    },
                    budget_usd: 0.1,
                    max_turns: 10,
                    feedback_rounds: 1,
                },
                StageSpec {
                    id: 2,
                    kind: "feature".into(),
                    title: "pubsub".into(),
                    prompt: "stages/2.md".into(),
                    acceptance: Acceptance {
                        redis_stages: vec![1, 2, 3, 5],
                    },
                    budget_usd: 0.1,
                    max_turns: 10,
                    feedback_rounds: 1,
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

        // Out-of-range acceptance id (10-13 are valid stages now).
        let mut v = good.clone();
        v["stages"][0]["acceptance"]["redis_stages"] = serde_json::json!([14]);
        let bad: CampaignSpec = serde_json::from_value(v).unwrap();
        assert!(validate_spec(&bad, &lc.root).is_err());

        // Stages 10-13 (dashboard, security, perf, HA) are valid ids.
        let mut v = good.clone();
        v["stages"][0]["acceptance"]["redis_stages"] = serde_json::json!([1, 10, 11, 12, 13]);
        let ok: CampaignSpec = serde_json::from_value(v).unwrap();
        assert!(validate_spec(&ok, &lc.root).is_ok());

        // Unsupported task.
        let mut v = good;
        v["task"] = serde_json::json!("http");
        let bad: CampaignSpec = serde_json::from_value(v).unwrap();
        assert!(validate_spec(&bad, &lc.root).is_err());
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
        };
        let mut calls = 0;
        let snaps = std::cell::RefCell::new(Vec::new());
        let cb = |r: &CampaignRollup| snaps.borrow_mut().push(r.stages.len());
        let rollup = run_campaign(
            &lc,
            &opts,
            |stage| {
                calls += 1;
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

    #[tokio::test]
    async fn test_orchestrator_budget_stop_is_not_a_failure() {
        let lc = fixture_campaign();
        let opts = CampaignRunOptions {
            campaign_id: "c2".into(),
            model: "m".into(),
            harness: "omp".into(),
            effort: "high".into(),
            budget_usd: 0.005, // one stage costs 0.01; the second never starts
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
        };
        // Stage 0 green; stage 1 regresses stage 1 (was green, now red).
        let rollup = run_campaign(
            &lc,
            &opts,
            |stage| {
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
