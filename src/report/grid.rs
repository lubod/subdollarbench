//! Round 44: the published career grid.
//!
//! Phase cells are chained (P1); the public board ranks one row per model
//! from the model's *careers* and each career is the leaf phase cell of a
//! chain. The results page and the variance report read the same career
//! objects, so the board, the charts and the CLI never disagree.

use anyhow::{Context, Result};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::campaign::{self, BoardSortKey, CampaignRollup, CampaignSpec, LadderEntry, StageRecord};
use crate::report::portal::{is_hidden, is_poisoned, publishable, PortalSummary};

/// The published career cap: four phases at $0.99.
pub const CAREER_CAP_USD: f64 = 4.0 * 0.99;
pub const CAREER_MAX_POINTS: f64 = 76.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    pub prompt: u64,
    pub cached: u64,
    pub completion: u64,
    pub total: u64,
}

impl TokenTotals {
    /// R64 F5: `prompt_tokens` is 0 on some routes, so fresh input is the
    /// remainder of the total after the cached and completion shares.
    pub fn fresh_input(&self) -> u64 {
        self.total
            .saturating_sub(self.cached)
            .saturating_sub(self.completion)
    }
}

#[derive(Debug, Clone)]
pub struct CareerCell {
    pub campaign_id: String,
    pub cells: Vec<String>,
    pub model: String,
    pub harness: String,
    pub effort: String,
    pub phase: Option<u32>,
    pub phase_total: u32,
    pub finished: bool,
    /// R54 F1: a $0 rehearsal or a smoke cell; shown, never ranked.
    pub rehearsal: bool,
    pub smoke: bool,
    pub state: String,
    pub reached: Option<u32>,
    pub regression_free: u32,
    pub points: f64,
    pub max_points: f64,
    pub rules: String,
    pub sqlite_version: Option<String>,
    pub cap_price_basis: Option<String>,
    pub sandbox_image: Option<String>,
    /// Full rules fingerprints recorded by this career's ticket manifests.
    pub fingerprints: Vec<String>,
    /// R47 F2: the career's latest measured perf block, or the phase's own
    /// for a cell with no career. The board shows its score and hovers the
    /// per-workload ratios.
    pub perf: Option<crate::verifier::sql::perfscore::PerfScore>,
    pub cost: f64,
    pub over_budget: bool,
    pub cap_reached: bool,
    pub over_wall: bool,
    pub updated: String,
    pub started: String,
    pub finished_at: String,
    /// R64 F5: the phases' wall time summed, the agent's own share, the
    /// turns, and the manifests' token totals (display only).
    pub wall_seconds: f64,
    pub agent_seconds: f64,
    pub turns: u64,
    pub tokens: Option<TokenTotals>,
    pub stage_records: Vec<StageRecord>,
    pub ladder: Vec<LadderEntry>,
    pub key: BoardSortKey,
}

impl CareerCell {
    pub fn points_per_dollar(&self) -> Option<f64> {
        (self.cost > 0.0).then(|| self.points / self.cost)
    }

    pub fn reg_free_per_dollar(&self) -> Option<f64> {
        (self.cost > 0.0).then(|| self.regression_free as f64 / self.cost)
    }

    pub fn tickets_passed(&self) -> usize {
        unique_stage_records(&self.stage_records)
            .iter()
            .filter(|r| r.passed)
            .count()
    }

    /// P(reached >= i), where `reached` is the highest passed ticket as
    /// the rollup defines it. This is monotone by construction; a gap in
    /// the ladder (a failed ticket followed by a later pass) cannot make
    /// the curve rise again.
    pub fn survival(&self) -> Vec<f64> {
        let Some(reached) = self.reached else {
            return vec![0.0; 21];
        };
        (0..=20)
            .map(|i| if i <= reached { 1.0 } else { 0.0 })
            .collect()
    }
}

fn unique_stage_records(records: &[StageRecord]) -> Vec<&StageRecord> {
    let mut out: Vec<&StageRecord> = Vec::new();
    for r in records {
        let slot = out.iter().position(|x| x.id == r.id);
        match slot {
            None => out.push(r),
            Some(i) => {
                let old = out[i];
                let old_rank = (old.passed, old.variant != "lite");
                let new_rank = (r.passed, r.variant != "lite");
                if new_rank > old_rank {
                    out[i] = r;
                }
            }
        }
    }
    out.sort_by_key(|r| r.id);
    out
}

struct RawCell {
    name: String,
    dir: PathBuf,
    rollup: Value,
    parsed: Option<CampaignRollup>,
    state: String,
    updated: Option<String>,
    phase: Option<u32>,
    chain_from: Option<String>,
    career_cells: Vec<String>,
    /// R47 F4: the successor this cell superseded, when it is a refork.
    refork_of: Option<String>,
}

fn read_json(path: &Path) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

fn drop_lineage(dropped: &mut HashSet<String>, name: &str, rollup: &Value) {
    dropped.insert(name.to_string());
    if let Some(cells) = rollup["career"]["cells"].as_array() {
        for c in cells {
            if let Some(c) = c.as_str() {
                dropped.insert(c.to_string());
            }
        }
    }
    if let Some(chain) = rollup["chain_from"].as_str() {
        dropped.insert(chain.to_string());
    }
}

/// R47 F4: drop a superseded successor and every later phase chained from
/// it. The refork's own branch is not a child of the superseded cell, so it
/// survives; the superseded branch itself stays on disk for the Run view.
fn drop_branch(dropped: &mut HashSet<String>, root: &str, by_name: &HashMap<String, &RawCell>) {
    let mut stack = vec![root.to_string()];
    let mut seen = HashSet::new();
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        dropped.insert(name.clone());
        for c in by_name.values() {
            if c.chain_from.as_deref() == Some(name.as_str()) {
                stack.push(c.name.clone());
            }
        }
    }
}

fn load_raw(runs_dir: &str) -> (Vec<RawCell>, HashSet<String>) {
    let runs = Path::new(runs_dir);
    let mut out = Vec::new();
    let mut dropped = HashSet::new();
    let Ok(rd) = fs::read_dir(runs) else {
        return (out, dropped);
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("campaign_") || !e.path().is_dir() {
            continue;
        }
        if is_hidden(runs, &name) || is_poisoned(runs, &name) {
            if let Some(v) = read_json(&e.path().join("campaign.json")).or_else(|| {
                read_json(&e.path().join("status.json")).and_then(|s| s.get("rollup").cloned())
            }) {
                drop_lineage(&mut dropped, &name, &v);
            } else {
                dropped.insert(name.clone());
            }
            continue;
        }
        let status = read_json(&e.path().join("status.json"));
        let rollup = match read_json(&e.path().join("campaign.json")) {
            Some(v) => v,
            None => match status
                .as_ref()
                .and_then(|s| s.get("rollup"))
                .filter(|v| !v.is_null())
            {
                Some(v) => v.clone(),
                None => continue,
            },
        };
        // Only the active campaign publishes.
        if !campaign::ACTIVE_CAMPAIGNS
            .iter()
            .any(|c| rollup["name"].as_str() == Some(*c))
        {
            continue;
        }
        // A cell whose stage runs were hidden or poisoned is not a
        // measurement. The career check below also drops a chain that
        // contains one.
        let flagged = rollup["stages"]
            .as_array()
            .map(|ss| {
                ss.iter().any(|st| {
                    st["run_id"]
                        .as_str()
                        .map(|rid| is_hidden(runs, rid) || is_poisoned(runs, rid))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if flagged {
            drop_lineage(&mut dropped, &name, &rollup);
            continue;
        }
        let state = status
            .as_ref()
            .and_then(|t| t["state"].as_str())
            .unwrap_or("done")
            .to_string();
        let updated = status
            .as_ref()
            .and_then(|t| t["updated_at"].as_str().map(str::to_string));
        let state = campaign::effective_state(&name, &state, updated.as_deref());
        let stamp = rollup["started_at"]
            .as_str()
            .or_else(|| rollup["finished_at"].as_str())
            .unwrap_or("");
        let naive = stamp.get(..19).unwrap_or(stamp);
        if !publishable(naive) {
            drop_lineage(&mut dropped, &name, &rollup);
            continue;
        }
        let mut parsed: Option<CampaignRollup> = serde_json::from_value(rollup.clone()).ok();
        if let Some(r) = parsed.as_mut() {
            campaign::relabel_graded_crashes(r, &e.path());
            if r.harness_fault.is_some() {
                drop_lineage(&mut dropped, &name, &rollup);
                continue;
            }
        }
        let phase = rollup["phase"]
            .as_u64()
            .map(|p| p as u32)
            .or_else(|| parsed.as_ref().and_then(|r| r.phase));
        let chain_from = rollup["chain_from"]
            .as_str()
            .map(str::to_string)
            .or_else(|| parsed.as_ref().and_then(|r| r.chain_from.clone()));
        let career_cells = rollup["career"]["cells"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let refork_of = rollup["refork"]["of"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                status
                    .as_ref()
                    .and_then(|t| t.pointer("/refork/of"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .or_else(|| {
                status
                    .as_ref()
                    .and_then(|t| t.pointer("/live/refork/of"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
        out.push(RawCell {
            name,
            dir: e.path(),
            rollup,
            parsed,
            state,
            updated,
            phase,
            chain_from,
            career_cells,
            refork_of,
        });
    }
    // R47 F4: a refork supersedes the successor it names and that branch's
    // later phases. The public board keeps only the refork's branch; the
    // Run view still serves the superseded cells.
    let by_name: HashMap<String, &RawCell> = out.iter().map(|c| (c.name.clone(), c)).collect();
    let roots: Vec<String> = out.iter().filter_map(|c| c.refork_of.clone()).collect();
    for root in roots {
        drop_branch(&mut dropped, &root, &by_name);
    }
    (out, dropped)
}

fn chain_for<'a>(leaf: &'a RawCell, by_name: &HashMap<String, &'a RawCell>) -> Vec<&'a RawCell> {
    let mut ids: Vec<String> = Vec::new();
    let mut cur = leaf;
    loop {
        if ids.iter().any(|x| x == &cur.name) {
            break;
        }
        ids.push(cur.name.clone());
        match cur.chain_from.as_ref().and_then(|p| by_name.get(p)) {
            Some(prev) => cur = prev,
            None => break,
        }
    }
    // The leaf's cumulative career list may know phases older than the
    // immediately preceding cell. Keep it in phase order, without pulling
    // in a different chain.
    for name in &leaf.career_cells {
        if !ids.contains(name) {
            if let Some(c) = by_name.get(name) {
                ids.push(c.name.clone());
            }
        }
    }
    let mut cells: Vec<&RawCell> = ids
        .iter()
        .filter_map(|id| by_name.get(id).copied())
        .collect();
    cells.sort_by_key(|c| c.phase.unwrap_or(0));
    cells
}

fn total_phases(spec: Option<&CampaignSpec>) -> u32 {
    spec.map(|s| s.phases.len() as u32).unwrap_or(0)
}

fn total_points(spec: &CampaignSpec) -> f64 {
    spec.phases
        .iter()
        .map(|p| campaign::phase_max_points(p, &spec.stages))
        .sum()
}

fn merge_stage_records(cells: &[&RawCell], spec: Option<&CampaignSpec>) -> Vec<StageRecord> {
    let mut out = Vec::new();
    for cell in cells {
        let Some(parsed) = cell.parsed.as_ref() else {
            continue;
        };
        for rec in &parsed.stages {
            let owner = spec
                .and_then(|s| campaign::phase_of(s, rec.id))
                .map(|p| p.id);
            match (cell.phase, owner) {
                (Some(phase), Some(owner)) if phase != owner => continue,
                _ => out.push(rec.clone()),
            }
        }
    }
    out
}

fn recorded_fingerprints(cells: &[&RawCell]) -> Vec<String> {
    let mut out = Vec::new();
    for cell in cells {
        let Some(parsed) = cell.parsed.as_ref() else {
            continue;
        };
        for rec in &parsed.stages {
            let variants: Vec<&str> = if rec.variant.is_empty() {
                vec!["full", "lite"]
            } else {
                vec![rec.variant.as_str()]
            };
            for variant in variants {
                let path = cell
                    .dir
                    .join(format!("s{}_{}", rec.id, variant))
                    .join("manifest.json");
                let Some(v) = read_json(&path) else {
                    continue;
                };
                if let Some(fp) = v["rules_version"].as_str().filter(|f| !f.is_empty()) {
                    out.push(fp.to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// R64 F5: wall time summed over the career's phase cells, the agent's own
/// share from each stage's `duration_seconds`, and the turns.
fn career_effort(cells: &[&RawCell]) -> (f64, f64, u64) {
    let mut wall = 0.0;
    let mut agent = 0.0;
    let mut turns = 0u64;
    for cell in cells {
        let started = cell.rollup["started_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        let finished = cell.rollup["finished_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        if let (Some(a), Some(b)) = (started, finished) {
            let secs = (b - a).num_seconds() as f64;
            if secs > 0.0 {
                wall += secs;
            }
        }
        if let Some(parsed) = &cell.parsed {
            for rec in &parsed.stages {
                turns += rec.turns.unwrap_or(0) as u64;
                agent += rec.duration_seconds.unwrap_or(0.0);
            }
        }
    }
    (wall, agent, turns)
}

/// R64 F5: sum `tokens` from every `s*/manifest.json` in the career's
/// cells. A career with no manifest carrying a `tokens` object has None
/// and shows `—`.
fn token_totals(cells: &[&RawCell]) -> Option<TokenTotals> {
    let mut found = false;
    let mut t = TokenTotals::default();
    for cell in cells {
        let Ok(rd) = fs::read_dir(&cell.dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with('s') || !entry.path().is_dir() {
                continue;
            }
            let Some(v) = read_json(&entry.path().join("manifest.json")) else {
                continue;
            };
            let Some(tok) = v.get("tokens").filter(|x| x.is_object()) else {
                continue;
            };
            found = true;
            let get = |key: &str| tok.get(key).and_then(Value::as_u64).unwrap_or(0);
            t.prompt += get("prompt_tokens");
            t.cached += get("cached_tokens");
            t.completion += get("completion_tokens");
            t.total += get("total_tokens");
        }
    }
    found.then_some(t)
}

fn career_from_leaf(leaf: &RawCell, cells: &[&RawCell]) -> CareerCell {
    let spec_loaded = campaign::load_campaign(
        leaf.parsed
            .as_ref()
            .map(|r| r.name.as_str())
            .unwrap_or("sdb-sql"),
    )
    .ok();
    let spec = spec_loaded.as_ref().map(|l| &l.spec);
    let phase_total = if leaf.phase.is_some() {
        total_phases(spec)
    } else {
        0
    };
    let phases: HashSet<u32> = cells.iter().filter_map(|c| c.phase).collect();
    let rehearsal = cells
        .iter()
        .any(|c| c.rollup["rehearsal"].as_bool().unwrap_or(false));
    let smoke = cells
        .iter()
        .any(|c| c.rollup["smoke"].as_bool().unwrap_or(false));
    let finished = if rehearsal || smoke {
        false
    } else if leaf.phase.is_some() {
        leaf.state == "done"
            && phase_total > 0
            && phases.len() as u32 == phase_total
            && leaf.phase == Some(phase_total)
    } else {
        // A pre-career full cell is already a complete measurement; keep
        // the historical single-cell board behaviour for old rows.
        true
    };
    let stage_records = merge_stage_records(cells, spec);
    let fingerprints = recorded_fingerprints(cells);
    let (wall_seconds, agent_seconds, turns) = career_effort(cells);
    let tokens = token_totals(cells);
    let unique = unique_stage_records(&stage_records);
    let reached = unique.iter().filter(|r| r.passed).map(|r| r.id).max();
    let regression_free = unique
        .iter()
        .filter(|r| r.passed && r.regressed.is_empty())
        .count() as u32;
    let parsed = leaf.parsed.as_ref();
    let perf = parsed.and_then(|r| match &r.career {
        Some(c) => c.perf.clone(),
        None => r.perf.clone(),
    });
    let points = parsed
        .and_then(|r| r.career.as_ref().map(|c| c.points))
        .or_else(|| parsed.map(|r| r.points))
        .unwrap_or_else(|| {
            cells
                .iter()
                .filter_map(|c| c.parsed.as_ref())
                .map(|r| r.points)
                .sum()
        });
    let cost = parsed
        .and_then(|r| r.career.as_ref().map(|c| c.total_cost_usd))
        .or_else(|| parsed.map(|r| r.total_cost_usd))
        .unwrap_or_else(|| {
            cells
                .iter()
                .filter_map(|c| c.parsed.as_ref())
                .map(|r| r.total_cost_usd)
                .sum()
        });
    let max_points = if phase_total > 0 {
        spec.map(total_points).unwrap_or(CAREER_MAX_POINTS)
    } else {
        parsed.map(|r| r.max_points).unwrap_or(CAREER_MAX_POINTS)
    };
    let rules = leaf.rollup["rules_version"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let ladder = match (parsed, spec) {
        (Some(parsed), Some(spec)) => {
            let mut merged = parsed.clone();
            merged.stages = stage_records.clone();
            merged.phase = None;
            merged.chain_from = None;
            merged.reached = reached;
            merged.regression_free_tickets = regression_free;
            merged.points = points;
            merged.max_points = max_points;
            merged.total_cost_usd = cost;
            campaign::ladder(spec, &merged, None)
        }
        _ => Vec::new(),
    };
    let key = parsed
        .map(campaign::BoardSortKey::of)
        .unwrap_or(BoardSortKey {
            live: false,
            current_rules: campaign::is_current_rules(&rules),
            points,
            perf: leaf
                .rollup
                .pointer("/career/perf/score")
                .or_else(|| leaf.rollup.pointer("/perf/score"))
                .and_then(Value::as_f64),
            per_dollar: (cost > 0.0).then(|| points / cost),
            cost,
        });
    // R51 F4 (R44-14): the career spans its phase cells, not only the leaf.
    let started = cells
        .iter()
        .filter_map(|c| c.rollup["started_at"].as_str())
        .filter(|s| !s.is_empty())
        .min()
        .unwrap_or("")
        .to_string();
    let finished_at = cells
        .iter()
        .filter_map(|c| c.rollup["finished_at"].as_str())
        .filter(|s| !s.is_empty())
        .max()
        .unwrap_or("")
        .to_string();
    CareerCell {
        campaign_id: leaf.name.clone(),
        cells: cells.iter().map(|c| c.name.clone()).collect(),
        model: leaf.rollup["model"].as_str().unwrap_or("?").to_string(),
        harness: leaf.rollup["harness"].as_str().unwrap_or("?").to_string(),
        // R51 F3 (R44-10): an unknown effort stays empty; the board, the
        // results page and the Run view show nothing there, and grouping
        // keeps it apart from a real "high".
        effort: leaf.rollup["effort"].as_str().unwrap_or("").to_string(),
        phase: leaf.phase,
        phase_total,
        finished,
        rehearsal,
        smoke,
        state: leaf.state.clone(),
        reached,
        regression_free,
        points,
        max_points,
        rules,
        sqlite_version: leaf.rollup["sqlite_version"]
            .as_str()
            .map(str::to_string)
            .or_else(|| parsed.and_then(|r| r.sqlite_version.clone())),
        cap_price_basis: leaf.rollup["cap_price_basis"]
            .as_str()
            .map(str::to_string)
            .or_else(|| parsed.and_then(|r| r.cap_price_basis.clone())),
        sandbox_image: parsed
            .and_then(|r| r.sandbox_image.clone())
            .or_else(|| leaf.rollup["sandbox_image"].as_str().map(str::to_string)),
        fingerprints,
        perf,
        cost,
        over_budget: leaf.rollup["over_budget"].as_bool().unwrap_or(false),
        cap_reached: leaf.rollup["cap_reached"].as_bool().unwrap_or(false),
        over_wall: leaf.rollup["over_wall"].as_bool().unwrap_or(false),
        updated: leaf.updated.clone().unwrap_or_else(|| finished_at.clone()),
        started,
        finished_at,
        wall_seconds,
        agent_seconds,
        turns,
        tokens,
        stage_records,
        ladder,
        key,
    }
}

/// Load one row per career (leaf phase cell).
pub fn careers(runs_dir: &str) -> Vec<CareerCell> {
    let (raw, dropped) = load_raw(runs_dir);
    let by_name: HashMap<String, &RawCell> = raw.iter().map(|c| (c.name.clone(), c)).collect();
    let mut referenced = HashSet::new();
    for c in &raw {
        if let Some(p) = &c.chain_from {
            referenced.insert(p.clone());
        }
    }
    let mut out = Vec::new();
    for leaf in raw.iter().filter(|c| !referenced.contains(&c.name)) {
        // A career that contains a hidden, poisoned or harness-fault phase
        // is not a measurement. Merely dropping the bad cell would publish
        // a later phase with cumulative totals that still include it.
        let mut related: Vec<String> = leaf.career_cells.clone();
        related.push(leaf.name.clone());
        let mut cur = leaf;
        loop {
            let Some(prev_name) = cur.chain_from.as_ref() else {
                break;
            };
            related.push(prev_name.clone());
            match by_name.get(prev_name) {
                Some(prev) => cur = prev,
                None => break,
            }
        }
        if related.iter().any(|id| dropped.contains(id)) {
            continue;
        }
        let cells = chain_for(leaf, &by_name);
        out.push(career_from_leaf(leaf, &cells));
    }
    out
}

#[derive(Debug, Clone)]
pub struct ModelGroup {
    pub model: String,
    pub harness: String,
    pub effort: String,
    pub rules: String,
    pub current: bool,
    pub careers: Vec<usize>,
    pub best: usize,
    pub median: usize,
    pub worst: usize,
    pub group_start: usize,
    pub tie: bool,
}

impl ModelGroup {
    pub fn rank_label(&self) -> String {
        if !self.current {
            "—".to_string()
        } else if self.tie {
            format!("{}=", self.group_start + 1)
        } else {
            (self.group_start + 1).to_string()
        }
    }
}

pub fn model_groups(cells: &[CareerCell]) -> Vec<ModelGroup> {
    let mut groups: Vec<ModelGroup> = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        if !c.finished {
            continue;
        }
        if let Some(g) = groups.iter_mut().find(|g| {
            g.model == c.model
                && g.harness == c.harness
                && g.effort == c.effort
                && g.rules == c.rules
        }) {
            g.careers.push(i);
        } else {
            groups.push(ModelGroup {
                model: c.model.clone(),
                harness: c.harness.clone(),
                effort: c.effort.clone(),
                rules: c.rules.clone(),
                current: c.key.current_rules,
                careers: vec![i],
                best: i,
                median: i,
                worst: i,
                group_start: 0,
                tie: false,
            });
        }
    }
    for g in &mut groups {
        g.careers
            .sort_by(|a, b| campaign::board_order(&cells[*a].key, &cells[*b].key));
        g.best = g.careers[0];
        // R44-8/R47 F3: with an even count, take the worse middle (the
        // list is best-first), so the model row is conservative.
        g.median = g.careers[g.careers.len() / 2];
        g.worst = *g.careers.last().unwrap();
    }
    groups.sort_by(|a, b| campaign::board_order(&cells[a.median].key, &cells[b.median].key));
    let mut start = 0;
    let mut previous_current = false;
    for i in 0..groups.len() {
        if i == 0 {
            start = 0;
        } else {
            let prev = &groups[i - 1];
            let cur = &groups[i];
            let overlap = cur.current
                && previous_current
                && campaign::board_order(&cells[cur.best].key, &cells[prev.worst].key)
                    != Ordering::Greater;
            if !overlap {
                start = i;
            }
        }
        groups[i].group_start = start;
        previous_current = groups[i].current;
    }
    for i in 0..groups.len() {
        let s = groups[i].group_start;
        groups[i].tie = groups.iter().filter(|g| g.group_start == s).count() > 1;
    }
    groups
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn strip_html(ladder: &[LadderEntry]) -> String {
    if ladder.is_empty() {
        return String::new();
    }
    let squares: String = ladder
        .iter()
        .map(|e| {
            let cls = match e.status.as_str() {
                "pass" => "pass",
                "lite" => "lite",
                "fail" => "fail",
                "fault" => "fault",
                "skipped" => "skip",
                "running" => "run",
                _ => "na",
            };
            format!(
                "<span class=\"sq {cls}\" data-ticket=\"{}\" title=\"ticket {} · {} · {}{}\">{}</span>",
                e.id,
                e.id,
                esc(&e.title),
                e.status,
                e.cause
                    .as_deref()
                    .map(|c| format!(" · {c}"))
                    .unwrap_or_default(),
                e.id
            )
        })
        .collect();
    format!("<div class=\"strip\">{squares}</div>")
}

fn legend_html() -> &'static str {
    "<div class=\"legend\"><span class=\"sq pass\">·</span>pass<span class=\"sq lite\">·</span>lite<span class=\"sq fail\">·</span>fail<span class=\"sq fault\">·</span>fault<span class=\"sq skip\">·</span>skipped<span class=\"sq run\">·</span>running<span class=\"sq na\">·</span>not reached</div>"
}

/// R64 F1: points print with at most one decimal (`57.6`, never
/// `57.599999999999994`). A whole number loses the trailing `.0`.
fn fmt_pts(v: f64) -> String {
    let rounded = (v * 10.0).round() / 10.0;
    if (rounded.fract()).abs() < f64::EPSILON {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

/// A median keeps one decimal (`65.0`), the form the model row uses.
fn fmt_pts1(v: f64) -> String {
    format!("{v:.1}")
}

/// R64 F5: a duration in hours (`5.4 h`); under an hour reads in minutes.
fn fmt_hours(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "—".to_string();
    }
    let hours = seconds / 3600.0;
    if hours >= 1.0 {
        format!("{hours:.1} h")
    } else {
        format!("{:.0} m", (seconds / 60.0).round())
    }
}

fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn fmt_tokens(n: u64) -> String {
    let scaled = |v: f64, suffix: &str| {
        let r = (v * 10.0).round() / 10.0;
        if (r.fract()).abs() < f64::EPSILON {
            format!("{r:.0}{suffix}")
        } else {
            format!("{r:.1}{suffix}")
        }
    };
    if n >= 1_000_000 {
        scaled(n as f64 / 1_000_000.0, "M")
    } else if n >= 1_000 {
        scaled(n as f64 / 1_000.0, "K")
    } else {
        n.to_string()
    }
}

fn duration_cell(c: &CareerCell) -> String {
    if c.wall_seconds <= 0.0 {
        return "—".to_string();
    }
    let title = format!(
        "agent {} of {}",
        fmt_hours(c.agent_seconds),
        fmt_hours(c.wall_seconds)
    );
    format!(
        "<span title=\"{}\">{}</span>",
        esc(&title),
        fmt_hours(c.wall_seconds)
    )
}

fn turns_cell(c: &CareerCell) -> String {
    if c.turns == 0 {
        "—".to_string()
    } else {
        fmt_count(c.turns)
    }
}

fn token_cell(c: &CareerCell) -> String {
    let Some(t) = &c.tokens else {
        return "—".to_string();
    };
    let share = if t.total > 0 {
        100.0 * t.cached as f64 / t.total as f64
    } else {
        0.0
    };
    let title = format!(
        "prompt {} · cached {} · completion {} · total {} · cache share {:.1}% · fresh input {}",
        fmt_count(t.prompt),
        fmt_count(t.cached),
        fmt_count(t.completion),
        fmt_count(t.total),
        share,
        fmt_count(t.fresh_input())
    );
    format!(
        "<span title=\"{}\">out {} · in {} (+{} cached)</span>",
        esc(&title),
        fmt_tokens(t.completion),
        fmt_tokens(t.fresh_input()),
        fmt_tokens(t.cached)
    )
}

fn reached_of(c: &CareerCell) -> String {
    c.reached
        .map(|r| format!("ticket {r}"))
        .unwrap_or_else(|| "—".to_string())
}

fn ppd_of(c: &CareerCell) -> String {
    c.points_per_dollar()
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "—".to_string())
}

fn state_of(c: &CareerCell) -> String {
    match c.state.as_str() {
        "running" => "<span class=\"b run\">RUNNING</span>".to_string(),
        "done" => "<span class=\"b ok\">done</span>".to_string(),
        other => format!("<span class=\"b\">{}</span>", esc(other)),
    }
}

fn flags_of(c: &CareerCell) -> String {
    let mut f = String::new();
    if c.over_budget {
        f.push_str(" · <span class=\"b warn\">over budget</span>");
    }
    if c.cap_reached {
        f.push_str(" · <span class=\"b\">budget cap</span>");
    }
    if c.over_wall {
        f.push_str(" · <span class=\"b warn\">over wall</span>");
    }
    f
}

/// R47 F2: the board's Perf cell. A score reads `1.32×`; a career with no
/// perf block reads `—`. The title lists every workload and flags a floored
/// one (a wrong answer or a non-durable write, not a speed result).
fn perf_cell(c: &CareerCell) -> String {
    let Some(perf) = &c.perf else {
        return "—".to_string();
    };
    let title = esc(&perf_tooltip(perf));
    match perf.score {
        Some(score) => format!("<span title=\"{title}\">{score:.2}×</span>"),
        None => format!("<span title=\"{title}\">—</span>"),
    }
}

fn perf_tooltip(perf: &crate::verifier::sql::perfscore::PerfScore) -> String {
    let mut parts: Vec<String> = perf
        .workloads
        .iter()
        .map(|w| {
            // R52 F6 (R47-1): a floored block shows the floor, not the raw
            // ratio; the note says why, and a measured ratio may follow.
            let ratio = if w.correct {
                w.ratio
            } else {
                crate::verifier::sql::perfscore::WRONG_FLOOR
            };
            let mut line = format!(
                "{}: ref {:.3} · candidate {:.3} · {:.2}×",
                w.id, w.reference, w.candidate, ratio
            );
            if !w.correct {
                line.push_str(" · floored");
                if let Some(note) = &w.note {
                    line.push_str(&format!(" · {note}"));
                }
                if let Some(raw) = w.numbers.get("raw_ratio") {
                    line.push_str(&format!(" · measured {raw:.2}×"));
                }
            } else if let Some(note) = &w.note {
                line.push_str(&format!(" · {note}"));
            }
            line
        })
        .collect();
    if let Some(err) = &perf.error {
        parts.push(format!("harness: {err}"));
    }
    if parts.is_empty() {
        "no perf workloads recorded".to_string()
    } else {
        parts.join("\n")
    }
}

/// R51 F3: the effort fragment, omitted entirely when the cell did not
/// record one (never defaulted to "high").
fn effort_note(effort: &str) -> String {
    if effort.is_empty() {
        String::new()
    } else {
        format!(" · effort {}", esc(effort))
    }
}

fn mark_badge(c: &CareerCell) -> String {
    if c.rehearsal {
        " <span class=\"b warn\">REHEARSAL</span>".to_string()
    } else if c.smoke {
        " <span class=\"b warn\">SMOKE</span>".to_string()
    } else {
        String::new()
    }
}

/// R64 F4 (H1): a career's visible name is `<model> · career <i> · <date>`;
/// the campaign id lives in the tooltip and the results-page link. The
/// index is the career's best-first position within its model/harness/
/// effort/rules group, so it matches the expanded rows.
fn career_labels(cells: &[CareerCell]) -> HashMap<String, String> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        match groups.iter_mut().find(|g| {
            let o = &cells[g[0]];
            o.model == c.model
                && o.harness == c.harness
                && o.effort == c.effort
                && o.rules == c.rules
        }) {
            Some(g) => g.push(i),
            None => groups.push(vec![i]),
        }
    }
    let mut out = HashMap::new();
    for g in &mut groups {
        g.sort_by(|a, b| {
            let ca = &cells[*a];
            let cb = &cells[*b];
            let ranked_a = ca.finished && ca.key.current_rules && !ca.rehearsal && !ca.smoke;
            let ranked_b = cb.finished && cb.key.current_rules && !cb.rehearsal && !cb.smoke;
            // R64 F4: the ranked careers number first, so a rehearsal or an
            // unfinished cell never shifts the paid careers' indices.
            ranked_b
                .cmp(&ranked_a)
                .then_with(|| campaign::board_order(&ca.key, &cb.key))
                .then_with(|| ca.started.cmp(&cb.started))
        });
        for (i, idx) in g.iter().enumerate() {
            let c = &cells[*idx];
            let date = c.started.get(..10).filter(|d| !d.is_empty()).unwrap_or("—");
            out.insert(
                c.campaign_id.clone(),
                format!("{} · career {} · {}", c.model, i + 1, date),
            );
        }
    }
    out
}

fn career_row(c: &CareerCell, name: &str, phase_label: Option<&str>, extra_class: &str) -> String {
    let label = phase_label
        .map(|p| format!("<div class=\"mut\">{p}</div>"))
        .unwrap_or_default();
    let class = if extra_class.is_empty() {
        "career-row".to_string()
    } else {
        format!("career-row {extra_class}")
    };
    let id = esc(&c.campaign_id);
    format!(
        "<tr class=\"{class}\"><td class=\"num\"></td><td>{label}<a href=\"results.html#{id}\" title=\"{id}\"><b>{}</b></a>{}<div class=\"mut\">{}{}</div>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{} / {}</td><td class=\"num\">{}</td><td class=\"num\">${:.4}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
        esc(name),
        mark_badge(c),
        effort_note(&c.effort),
        flags_of(c),
        strip_html(&c.ladder),
        reached_of(c),
        c.regression_free,
        fmt_pts(c.points),
        c.max_points as u32,
        perf_cell(c),
        c.cost,
        ppd_of(c),
        duration_cell(c),
        turns_cell(c),
        token_cell(c),
        state_of(c),
    )
}

/// R64 F6: the page-wide maxima the duration, turns and token strips share.
struct DotScales {
    wall: f64,
    turns: f64,
    tokens: f64,
}

fn strip_cell(
    values: &[Option<f64>],
    median: Option<f64>,
    lo: f64,
    hi: f64,
    log: bool,
    label: &str,
    fmt: fn(f64) -> String,
) -> String {
    format!(
        "{} {}",
        dot_strip(values, median, lo, hi, log, label, fmt),
        median.map(fmt).unwrap_or_else(|| "—".to_string())
    )
}

fn model_row(
    cells: &[CareerCell],
    g: &ModelGroup,
    labels: &HashMap<String, String>,
    stability: Option<&RankStability>,
    scales: &DotScales,
    html: &mut String,
) {
    let med = &cells[g.median];
    let best = &cells[g.best];
    let worst = &cells[g.worst];
    // R64 F4: the model row no longer repeats the median career's line, so
    // every career (even a single one) lives in the expanded list with its
    // ladder strip. R64 F6 adds the ticket-agreement strip there.
    let mut inner = String::new();
    for i in &g.careers {
        let c = &cells[*i];
        let name = labels
            .get(&c.campaign_id)
            .map(String::as_str)
            .unwrap_or(&c.campaign_id);
        inner.push_str(&career_row(c, name, None, ""));
    }
    let word = if g.careers.len() == 1 {
        "career"
    } else {
        "careers"
    };
    let agreement = if g.careers.len() > 1 {
        ticket_agreement(cells, g)
    } else {
        String::new()
    };
    let more = format!(
        "<details class=\"more\"><summary>×{} {word} ▸</summary>{agreement}<table class=\"career-table\"><tbody>{inner}</tbody></table></details>",
        g.careers.len()
    );
    let points: Vec<Option<f64>> = g.careers.iter().map(|i| Some(cells[*i].points)).collect();
    let perfs: Vec<Option<f64>> = g
        .careers
        .iter()
        .map(|i| cells[*i].perf.as_ref().and_then(|p| p.score))
        .collect();
    let perf_med = med.perf.as_ref().and_then(|p| p.score);
    let costs: Vec<Option<f64>> = g.careers.iter().map(|i| Some(cells[*i].cost)).collect();
    let walls: Vec<Option<f64>> = g
        .careers
        .iter()
        .map(|i| (cells[*i].wall_seconds > 0.0).then_some(cells[*i].wall_seconds))
        .collect();
    let wall_med = (med.wall_seconds > 0.0).then_some(med.wall_seconds);
    let turn_vals: Vec<Option<f64>> = g
        .careers
        .iter()
        .map(|i| (cells[*i].turns > 0).then_some(cells[*i].turns as f64))
        .collect();
    let turn_med = (med.turns > 0).then_some(med.turns as f64);
    let token_vals: Vec<Option<f64>> = g
        .careers
        .iter()
        .map(|i| {
            cells[*i]
                .tokens
                .map(|t| t.completion as f64)
                .filter(|v| *v > 0.0)
        })
        .collect();
    let token_med = med.tokens.map(|t| t.completion as f64).filter(|v| *v > 0.0);
    let points_cell = strip_cell(
        &points,
        Some(med.points),
        0.0,
        CAREER_MAX_POINTS,
        false,
        "points",
        fmt_pts1,
    );
    let perf_cell = strip_cell(&perfs, perf_med, 0.1, 10.0, true, "perf", fmt_perf);
    let spend_cell = strip_cell(
        &costs,
        Some(med.cost),
        0.0,
        CAREER_CAP_USD,
        false,
        "$",
        fmt_dollars,
    );
    let duration = strip_cell(
        &walls,
        wall_med,
        0.0,
        scales.wall,
        false,
        "duration",
        fmt_duration_value,
    );
    let turns = strip_cell(
        &turn_vals,
        turn_med,
        0.0,
        scales.turns,
        false,
        "turns",
        fmt_turns_value,
    );
    let tokens = strip_cell(
        &token_vals,
        token_med,
        0.0,
        scales.tokens,
        false,
        "tokens out",
        fmt_token_value,
    );
    html.push_str(&format!(
        "<tr class=\"model-row\"><td class=\"num\">{}</td><td><b>{}</b>{}{}<div class=\"mut\">{}{} · {} ({}-{}) · {} {}</div></td><td>—</td><td class=\"num\">—</td><td class=\"num\">{points_cell}</td><td class=\"num\">{perf_cell}</td><td class=\"num\">{spend_cell}</td><td class=\"num\">{}</td><td class=\"num\">{duration}</td><td class=\"num\">{turns}</td><td>{tokens}</td><td>{}</td></tr>",
        rank_label(g, stability),
        esc(&g.model),
        mark_badge(med),
        more,
        esc(&g.harness),
        effort_note(&g.effort),
        fmt_pts1(med.points),
        fmt_pts(worst.points),
        fmt_pts(best.points),
        g.careers.len(),
        word,
        ppd_of(med),
        state_of(med),
    ));
}

fn render_board(cells: &[CareerCell]) -> String {
    let groups = model_groups(cells);
    // R64 F1: the header counts only careers that rank: finished, current
    // rules, never a $0 rehearsal or a smoke cell. An older rule set is
    // incomparable and counts nowhere.
    let ranked: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.finished && c.key.current_rules && !c.rehearsal && !c.smoke)
        .collect();
    let ranked_models = groups.iter().filter(|g| g.current).count();
    let best_points = ranked.iter().map(|c| c.points).fold(0.0f64, f64::max);
    let spend: f64 = ranked.iter().map(|c| c.cost).sum();
    let current_groups: Vec<&ModelGroup> = groups.iter().filter(|g| g.current).collect();
    let old_groups: Vec<&ModelGroup> = groups.iter().filter(|g| !g.current).collect();
    let unfinished_current: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| !c.finished && c.key.current_rules)
        .collect();
    let unfinished_old: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| !c.finished && !c.key.current_rules)
        .collect();
    let labels = career_labels(cells);
    let stability = rank_stability(cells, &groups);
    let ranked_cells = || cells.iter().filter(|c| c.finished && c.key.current_rules);
    let scales = DotScales {
        wall: ranked_cells()
            .map(|c| c.wall_seconds)
            .fold(0.0, f64::max)
            .max(1.0),
        turns: ranked_cells()
            .map(|c| c.turns as f64)
            .fold(0.0, f64::max)
            .max(1.0),
        tokens: ranked_cells()
            .filter_map(|c| c.tokens.map(|t| t.completion as f64))
            .fold(0.0, f64::max)
            .max(1.0),
    };
    let mut rows = String::new();
    for (i, g) in current_groups.iter().enumerate() {
        let stab = stability.as_ref().map(|s| &s[i]);
        model_row(cells, g, &labels, stab, &scales, &mut rows);
    }
    if !unfinished_current.is_empty() {
        rows.push_str(
            "<tr class=\"divider\"><td colspan=\"12\">unfinished careers (not ranked)</td></tr>",
        );
        for c in unfinished_current {
            let name = labels
                .get(&c.campaign_id)
                .map(String::as_str)
                .unwrap_or(&c.campaign_id);
            rows.push_str(&career_row(
                c,
                name,
                Some(&unfinished_label(c)),
                "unfinished",
            ));
        }
    }
    // R64 F1/B3: an older rule set is incomparable whether it finished or
    // not; it never reads as "unfinished".
    if !old_groups.is_empty() || !unfinished_old.is_empty() {
        rows.push_str(
            "<tr class=\"divider\"><td colspan=\"12\">older rule sets (incomparable)</td></tr>",
        );
        for g in &old_groups {
            model_row(cells, g, &labels, None, &scales, &mut rows);
        }
        for c in unfinished_old {
            let name = labels
                .get(&c.campaign_id)
                .map(String::as_str)
                .unwrap_or(&c.campaign_id);
            rows.push_str(&career_row(
                c,
                name,
                Some(&unfinished_label(c)),
                "unfinished",
            ));
        }
    }
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let mut h = String::new();
    h.push_str("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>SubDollarBench — sdb-sql campaign careers</title><style>");
    h.push_str(include_str!("portal_board.css"));
    h.push_str("</style></head><body><main>");
    h.push_str("<header><div><h1>💰 SubDollarBench — sdb-sql campaign careers</h1><div class=\"sub\">One ticket ladder, one task: a differential SQL database checked statement by statement against SQLite. A model ranks by its median career; careers rank by difficulty-weighted points, then the perf score on a tie, then points per dollar.</div></div><div class=\"right\"><a href=\"results.html\">📈 Results</a>");
    h.push_str(&format!(
        "<div class=\"mut\">Updated {}</div></div></header>",
        esc(&now)
    ));
    h.push_str(&format!(
        "<div class=\"sub\">Rules {} · SQLite {} · cap policy: at list price (model_prices.json) for runs from .39; each career records its own.</div><div class=\"sub\">A career is 4 phases, 21 tickets, at most $0.99 per phase.</div>",
        esc(crate::report::rules::RULES_VERSION),
        esc(crate::report::rules::sqlite_version())
    ));
    // R64 F3 (M3): a grid with a career still running says so.
    let grid_total = ranked.len()
        + cells
            .iter()
            .filter(|c| !c.finished && c.key.current_rules && !c.rehearsal && !c.smoke)
            .count();
    if ranked.len() < grid_total {
        h.push_str(&format!(
            "<div class=\"banner\">Grid in progress: {} of {} careers finished; ranks are provisional.</div>",
            ranked.len(),
            grid_total
        ));
    }
    h.push_str(&format!(
        "<div class=\"cards\"><div class=\"card\"><div class=\"k\">Careers</div><div class=\"v\">{}</div></div><div class=\"card\"><div class=\"k\">Models</div><div class=\"v\">{ranked_models}</div></div><div class=\"card\"><div class=\"k\">Best points</div><div class=\"v\">{}</div></div><div class=\"card\"><div class=\"k\">Spend</div><div class=\"v\">${spend:.4}</div></div></div>",
        ranked.len(),
        fmt_pts1(best_points)
    ));
    if !current_groups.is_empty() && stability.is_none() {
        h.push_str("<div class=\"mut\">Rank stability: too many combinations to enumerate; ranks follow the board order.</div>");
    }
    h.push_str("<div class=\"wrap\"><table><thead><tr><th>#</th><th>Model / career</th><th title=\"the highest ladder ticket whose checks passed\">Highest ticket</th><th title=\"tickets that passed with no regression in their acceptance suite\">Tickets kept green</th><th>Points</th><th>Perf</th><th>Spend</th><th>Pts / $</th><th title=\"wall time summed over the career's phases; hover for the agent's share\">Duration</th><th title=\"turns summed over the career's tickets\">Turns</th><th title=\"completion tokens from every ticket manifest; hover for prompt, cached, total and the cache share\">Tokens out</th><th>State</th></tr></thead><tbody>");
    if rows.is_empty() {
        h.push_str("<tr><td colspan=\"12\" class=\"mut\">No sdb-sql careers yet (run one with <code>scripts/sql-cell.sh &lt;model&gt;</code>).</td></tr>");
    } else {
        h.push_str(&rows);
    }
    h.push_str("</tbody></table>");
    h.push_str(legend_html());
    h.push_str("</div>");
    h.push_str("<section class=\"methodology\"><h2>How this board is ranked</h2><ul><li><b>Careers</b>: one row per leaf phase cell of a chain. A model's row is its <b>median career</b> by the one shared board order; the best and worst careers are named next to it and the full set sits in <b>×N careers ▸</b>.</li><li><b>Points</b>: difficulty-weighted score. On equal points the <b>Perf</b> score decides (higher first; a career with no score ranks below one with a score), then <b>Pts / $</b> divides by effective spend. The perf contract is ticket 0's <a href=\"https://github.com/lubod/subdollarbench/blob/master/campaigns/sdb-sql/stages/0_genesis/prompt.md\">Performance section</a>; hover a Perf cell for each workload's reference, candidate, ratio and any floor.</li><li><b>Ties</b>: adjacent models are a rank group (<code>2=</code>) when the worse model's best career is at least as good as the better model's worst, by the shared board order (points, then perf, then points per dollar). Ties never block publishing.</li><li>Unfinished careers (fewer than four phases, or a phase still running) are listed under the divider and are not ranked. Older rule sets are incomparable and sit below the board.</li><li>Hidden, poisoned and harness-fault cells are never published. The ladder squares merge each phase cell's own tickets; the legend sits under the table.</li></ul></section>");
    h.push_str("</main></body></html>");
    h
}

fn unfinished_label(c: &CareerCell) -> String {
    if c.rehearsal {
        "rehearsal (not ranked)".to_string()
    } else if c.smoke {
        "smoke (not ranked)".to_string()
    } else {
        match (c.phase, c.phase_total) {
            (Some(p), t) if t > 0 => format!("phase {p} of {t}"),
            (Some(p), _) => format!("phase {p}"),
            _ => "in progress".to_string(),
        }
    }
}

/// R51 F4: a grid date is printed to the minute.
fn fmt_grid_time(s: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|_| s.to_string())
}

fn sorted_unique(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = values.filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    v
}

fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        None
    } else if v.len() % 2 == 1 {
        Some(v[v.len() / 2])
    } else {
        Some((v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0)
    }
}

/// R64 F6 (D1): exact rank stability. Every combination takes one finished
/// career per current-rules model; each is ranked by the board order. The
/// counts are exact, never sampled, and a grid too large to enumerate
/// falls back to the board order.
const RANK_ENUM_LIMIT: u64 = 100_000;

#[derive(Debug, Clone)]
pub struct RankStability {
    pub total: u64,
    /// counts[r] = combinations in which this model holds 0-based rank r.
    pub counts: Vec<u64>,
    pub most_likely: usize,
    pub share: f64,
    pub tie_with_above: bool,
}

fn rank_stability(cells: &[CareerCell], groups: &[ModelGroup]) -> Option<Vec<RankStability>> {
    let current: Vec<&ModelGroup> = groups.iter().filter(|g| g.current).collect();
    let n = current.len();
    if n == 0 {
        return None;
    }
    let mut total: u64 = 1;
    for g in &current {
        total = total.checked_mul(g.careers.len() as u64)?;
        if total > RANK_ENUM_LIMIT {
            return None;
        }
    }
    let mut counts = vec![vec![0u64; n]; n];
    let mut idx = vec![0usize; n];
    loop {
        let picks: Vec<&CareerCell> = current
            .iter()
            .zip(idx.iter())
            .map(|(g, i)| &cells[g.careers[*i]])
            .collect();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| campaign::board_order(&picks[a].key, &picks[b].key));
        let mut rank = 0usize;
        for (pos, &gi) in order.iter().enumerate() {
            if pos > 0
                && campaign::board_order(&picks[order[pos - 1]].key, &picks[gi].key)
                    != Ordering::Equal
            {
                rank = pos;
            }
            counts[gi][rank] += 1;
        }
        let mut done = true;
        for k in (0..n).rev() {
            idx[k] += 1;
            if idx[k] < current[k].careers.len() {
                done = false;
                break;
            }
            idx[k] = 0;
        }
        if done {
            break;
        }
    }
    let mut out: Vec<RankStability> = counts
        .iter()
        .map(|c| {
            let mut most = 0usize;
            for (r, v) in c.iter().enumerate() {
                if *v > c[most] {
                    most = r;
                }
            }
            RankStability {
                total,
                counts: c.clone(),
                most_likely: most,
                share: c[most] as f64 / total as f64,
                tie_with_above: false,
            }
        })
        .collect();
    for i in 1..n {
        // D1: tied with the row above only when both most likely hold the
        // same rank and neither does so in more than half the combinations.
        let same = out[i].most_likely == out[i - 1].most_likely;
        let both = out[i].share <= 0.5 && out[i - 1].share <= 0.5;
        out[i].tie_with_above = same && both;
    }
    Some(out)
}

/// R64 F6: the rank cell. With exact stability it reads `#1 (61%)`, with
/// `=` when D1 shows a tie with the row above; a grid too large to
/// enumerate keeps the board order's label.
fn rank_label(g: &ModelGroup, stability: Option<&RankStability>) -> String {
    if !g.current {
        return "—".to_string();
    }
    match stability {
        Some(s) => format!(
            "#{}{} ({:.0}%)",
            s.most_likely + 1,
            if s.tie_with_above { "=" } else { "" },
            s.share * 100.0
        ),
        None => g.rank_label(),
    }
}

/// R64 F6: one dot per career and a diamond at the median, on a fixed
/// scale for the column (points 0-76, perf log 0.1-10x, dollars 0-3.96,
/// and the page's maximum for duration, turns and tokens).
#[allow(clippy::too_many_arguments)]
fn dot_strip(
    values: &[Option<f64>],
    median: Option<f64>,
    lo: f64,
    hi: f64,
    log: bool,
    label: &str,
    fmt: fn(f64) -> String,
) -> String {
    let w = 72.0;
    let h = 14.0;
    let pad = 4.0;
    let x = |v: f64| -> f64 {
        let t = if log {
            if v <= 0.0 || lo <= 0.0 || hi <= lo {
                0.0
            } else {
                ((v.ln() - lo.ln()) / (hi.ln() - lo.ln())).clamp(0.0, 1.0)
            }
        } else if hi <= lo {
            0.0
        } else {
            ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
        };
        pad + t * (w - 2.0 * pad)
    };
    let vals: Vec<String> = values
        .iter()
        .map(|v| v.map(fmt).unwrap_or_else(|| "—".to_string()))
        .collect();
    let med_text = median.map(fmt).unwrap_or_else(|| "—".to_string());
    let mut s = format!(
        "<svg class=\"dots\" data-metric=\"{}\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" role=\"img\">",
        esc(label)
    );
    s.push_str(&format!(
        "<title>{}: {} · median {}</title>",
        esc(label),
        esc(&vals.join(", ")),
        esc(&med_text)
    ));
    for v in values {
        let vv = v.unwrap_or(lo);
        s.push_str(&format!(
            "<circle class=\"dot{}\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"2.2\"/>",
            if v.is_none() { " missing" } else { "" },
            x(vv),
            h / 2.0
        ));
    }
    if let Some(m) = median {
        let cx = x(m);
        s.push_str(&format!(
            "<polygon class=\"med\" points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\"/>",
            cx,
            h / 2.0 - 3.5,
            cx + 3.5,
            h / 2.0,
            cx,
            h / 2.0 + 3.5,
            cx - 3.5,
            h / 2.0
        ));
    }
    s.push_str("</svg>");
    s
}

fn fmt_perf(v: f64) -> String {
    format!("{v:.2}×")
}

fn fmt_dollars(v: f64) -> String {
    format!("${v:.4}")
}

fn fmt_duration_value(v: f64) -> String {
    fmt_hours(v)
}

fn fmt_turns_value(v: f64) -> String {
    fmt_count(v.round() as u64)
}

fn fmt_token_value(v: f64) -> String {
    fmt_tokens(v.round() as u64)
}

fn fmt_variance(name: &str, v: f64) -> String {
    match name {
        "points" => format!("{v:.1}"),
        "perf" => format!("{v:.2}×"),
        "$" => format!("${v:.4}"),
        "duration" => fmt_hours(v),
        "turns" => fmt_count(v.round() as u64),
        "tokens out" => fmt_tokens(v.round() as u64),
        _ => format!("{v:.1}"),
    }
}

/// R64 F6: the expanded model view's ticket agreement. The strip shades
/// each ticket by the share of the model's careers that passed it, and
/// the line counts agreement over the 20 non-genesis tickets.
fn ticket_agreement(cells: &[CareerCell], g: &ModelGroup) -> String {
    let n = g.careers.len();
    let mut passed = [0usize; 21];
    for i in &g.careers {
        for rec in unique_stage_records(&cells[*i].stage_records) {
            if rec.passed && rec.id <= 20 {
                passed[rec.id as usize] += 1;
            }
        }
    }
    let mut squares = String::new();
    for (id, p) in passed.iter().enumerate() {
        let p = *p;
        let cls = if p == 0 {
            "agree-none"
        } else if p == n {
            "agree-full"
        } else if p * 2 > n {
            "agree-most"
        } else {
            "agree-some"
        };
        squares.push_str(&format!(
            "<span class=\"sq {cls}\" title=\"ticket {id} · passed by {p}/{n}\">{id}</span>"
        ));
    }
    let mut agree = 0;
    let mut differs: Vec<String> = Vec::new();
    for (id, p) in passed.iter().enumerate().skip(1) {
        if *p == 0 || *p == n {
            agree += 1;
        } else {
            differs.push(id.to_string());
        }
    }
    let diff = if differs.is_empty() {
        String::new()
    } else {
        format!(" · differs: {}", differs.join(", "))
    };
    format!(
        "<div class=\"agree\"><div class=\"strip\">{squares}</div><div class=\"mut\">agree {agree}/20{diff}</div></div>"
    )
}

/// R64 F6: the share of career pairs where A outscores B on points.
fn head_to_head(a: &[&CareerCell], b: &[&CareerCell]) -> (f64, f64, f64) {
    let mut aw = 0usize;
    let mut bw = 0usize;
    let mut ties = 0usize;
    for x in a {
        for y in b {
            if x.points > y.points {
                aw += 1;
            } else if x.points < y.points {
                bw += 1;
            } else {
                ties += 1;
            }
        }
    }
    let total = (aw + bw + ties).max(1) as f64;
    (aw as f64 / total, bw as f64 / total, ties as f64 / total)
}

/// R64 F6: per metric, the median within-model spread against the span of
/// the models' median careers, and their ratio (the publishing bar's
/// variance check).
fn variance_rows(
    cells: &[CareerCell],
    groups: &[&ModelGroup],
) -> Vec<(&'static str, f64, f64, Option<f64>)> {
    type Metric = (&'static str, fn(&CareerCell) -> Option<f64>);
    let metrics: [Metric; 6] = [
        ("points", |c| Some(c.points)),
        ("perf", |c| c.perf.as_ref().and_then(|p| p.score)),
        ("$", |c| Some(c.cost)),
        ("duration", |c| {
            (c.wall_seconds > 0.0).then_some(c.wall_seconds)
        }),
        ("turns", |c| (c.turns > 0).then_some(c.turns as f64)),
        ("tokens out", |c| {
            c.tokens.map(|t| t.completion as f64).filter(|v| *v > 0.0)
        }),
    ];
    let mut out = Vec::new();
    for (name, value) in metrics {
        let mut spreads: Vec<f64> = Vec::new();
        let mut medians: Vec<f64> = Vec::new();
        for g in groups {
            let vals: Vec<f64> = g.careers.iter().filter_map(|i| value(&cells[*i])).collect();
            if vals.len() >= 2 {
                let lo = vals.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                spreads.push(hi - lo);
            } else if vals.len() == 1 {
                spreads.push(0.0);
            }
            if let Some(m) = value(&cells[g.median]) {
                medians.push(m);
            }
        }
        let within = median(&sorted_unique(spreads.into_iter()));
        let between = if medians.is_empty() {
            None
        } else {
            let lo = medians.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = medians.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            Some(hi - lo)
        };
        if let (Some(w), Some(b)) = (within, between) {
            let ratio = (w > 0.0).then(|| b / w);
            out.push((name, w, b, ratio));
        }
    }
    out
}

fn palette(i: usize) -> &'static str {
    const COLORS: [&str; 10] = [
        "#58a6ff", "#3fb950", "#d29922", "#f85149", "#bc8cff", "#39c5cf", "#ff7b72", "#a5d6ff",
        "#7ee787", "#ffa657",
    ];
    COLORS[i % COLORS.len()]
}

/// The non-dominated career ids, for tests and for the results page.
pub fn pareto_ids(cells: &[CareerCell]) -> Vec<String> {
    let cars: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.finished && c.key.current_rules)
        .collect();
    pareto(&cars)
        .into_iter()
        .map(|c| c.campaign_id.clone())
        .collect()
}

fn pareto<'a>(cars: &'a [&'a CareerCell]) -> Vec<&'a CareerCell> {
    let mut out = Vec::new();
    for c in cars {
        let dominated = cars.iter().any(|o| {
            !std::ptr::eq(*c, *o)
                && o.points >= c.points
                && o.cost <= c.cost
                && (o.points > c.points || o.cost < c.cost)
        });
        if !dominated {
            out.push(*c);
        }
    }
    out.sort_by(|a, b| {
        a.cost
            .partial_cmp(&b.cost)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.points.partial_cmp(&a.points).unwrap_or(Ordering::Equal))
    });
    out
}

fn svg_scatter(cars: &[&CareerCell], models: &[String]) -> String {
    let w = 760.0;
    let h = 340.0;
    let l = 48.0;
    let r = 16.0;
    let t = 16.0;
    let b = 40.0;
    let pw = w - l - r;
    let ph = h - t - b;
    let x = |cost: f64| l + (cost / CAREER_CAP_USD).clamp(0.0, 1.0) * pw;
    let y = |points: f64| t + (1.0 - (points / CAREER_MAX_POINTS).clamp(0.0, 1.0)) * ph;
    let mut s = format!(
        "<svg class=\"chart\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"points against dollars\">"
    );
    s.push_str("<rect x=\"0\" y=\"0\" width=\"760\" height=\"340\" fill=\"#0d1117\"/>");
    s.push_str(&format!(
        "<line x1=\"{l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#30363d\"/><line x1=\"{l}\" y1=\"{t}\" x2=\"{l}\" y2=\"{}\" stroke=\"#30363d\"/>",
        t + ph,
        l + pw,
        t + ph,
        t + ph
    ));
    for (frac, label) in [(0.0, "0"), (0.5, "1.98"), (1.0, "3.96")] {
        let xx = l + frac * pw;
        s.push_str(&format!(
            "<text x=\"{xx:.1}\" y=\"{:.1}\" fill=\"#8b949e\" font-size=\"10\">${label}</text>",
            t + ph + 15.0
        ));
    }
    for (frac, label) in [(0.0, "0"), (0.5, "38"), (1.0, "76")] {
        let yy = t + (1.0 - frac) * ph;
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{yy:.1}\" fill=\"#8b949e\" font-size=\"10\">{label}</text>",
            l - 28.0
        ));
    }
    let frontier = pareto(cars);
    let line: String = frontier
        .iter()
        .map(|c| format!("{:.1},{:.1}", x(c.cost), y(c.points)))
        .collect::<Vec<_>>()
        .join(" ");
    if !line.is_empty() {
        s.push_str(&format!(
            "<polyline class=\"pareto-frontier\" points=\"{line}\" fill=\"none\" stroke=\"#58a6ff\" stroke-width=\"2\" opacity=\"0.8\"/>"
        ));
    }
    for c in cars {
        let color = palette(models.iter().position(|m| m == &c.model).unwrap_or(0));
        s.push_str(&format!(
            "<circle class=\"career-dot\" data-model=\"{}\" data-points=\"{:.1}\" data-cost=\"{:.4}\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"5\" fill=\"{color}\"><title>{}: {:.1} pts · ${:.4}</title></circle>",
            esc(&c.model),
            c.points,
            c.cost,
            x(c.cost),
            y(c.points),
            esc(&c.model),
            c.points,
            c.cost
        ));
    }
    s.push_str("</svg>");
    s
}

fn svg_survival(series: &[(String, Vec<f64>)]) -> String {
    let w = 760.0;
    let h = 260.0;
    let l = 42.0;
    let t = 16.0;
    let pw = w - l - 16.0;
    let ph = h - t - 34.0;
    let mut s = format!(
        "<svg class=\"chart\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"ticket survival\">"
    );
    s.push_str("<rect x=\"0\" y=\"0\" width=\"760\" height=\"260\" fill=\"#0d1117\"/>");
    s.push_str(&format!(
        "<line x1=\"{l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#30363d\"/><line x1=\"{l}\" y1=\"{t}\" x2=\"{l}\" y2=\"{}\" stroke=\"#30363d\"/>",
        t + ph,
        l + pw,
        t + ph,
        t + ph
    ));
    for (frac, label) in [(0.0, "0"), (0.5, "50%"), (1.0, "100%")] {
        let yy = t + (1.0 - frac) * ph;
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{yy:.1}\" fill=\"#8b949e\" font-size=\"10\">{label}</text>",
            l - 34.0
        ));
    }
    for (mi, (model, vals)) in series.iter().enumerate() {
        let pts: String = vals
            .iter()
            .enumerate()
            .map(|(i, v)| {
                format!(
                    "{:.1},{:.1}",
                    l + (i as f64 / 20.0) * pw,
                    t + (1.0 - v.clamp(0.0, 1.0)) * ph
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        s.push_str(&format!(
            "<polyline class=\"survival-line\" data-model=\"{}\" points=\"{pts}\" fill=\"none\" stroke=\"{}\" stroke-width=\"2\"/>",
            esc(model),
            palette(mi)
        ));
    }
    s.push_str("</svg>");
    s
}

fn render_results(cells: &[CareerCell]) -> String {
    let labels = career_labels(cells);
    let current: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.finished && c.key.current_rules)
        .collect();
    let mut models: Vec<String> = current.iter().map(|c| c.model.clone()).collect();
    models.sort();
    models.dedup();
    let caps = current.len();
    let binary_fingerprint = crate::report::rules::fingerprint();
    let mut recorded: Vec<String> = current
        .iter()
        .flat_map(|c| c.fingerprints.iter().cloned())
        .collect();
    recorded.sort();
    recorded.dedup();
    let recorded_text = if recorded.is_empty() {
        "—".to_string()
    } else {
        recorded.join(", ")
    };
    let fingerprint_note = if recorded.is_empty() {
        " <span class=\"b warn\">no recorded fingerprint</span>".to_string()
    } else if recorded.iter().any(|f| f != &binary_fingerprint) {
        format!(
            " <span class=\"b warn\">recorded fingerprint differs from binary {}</span>",
            esc(&binary_fingerprint)
        )
    } else {
        String::new()
    };
    let rules = current
        .iter()
        .map(|c| c.rules.as_str())
        .find(|r| !r.is_empty())
        .unwrap_or(crate::report::rules::RULES_VERSION);
    let image = current
        .iter()
        .filter_map(|c| c.sandbox_image.as_deref())
        .next()
        .unwrap_or("—");
    let harness = {
        let mut v: Vec<&str> = current.iter().map(|c| c.harness.as_str()).collect();
        v.sort();
        v.dedup();
        v.join(", ")
    };
    let effort = {
        let mut v: Vec<&str> = current.iter().map(|c| c.effort.as_str()).collect();
        v.sort();
        v.dedup();
        v.join(", ")
    };
    let harness_effort = if effort.is_empty() {
        harness.clone()
    } else {
        format!("{harness} / {effort}")
    };
    // R51 F5 (Q8 a): the snapshot date and the ranked models' overrides.
    let mut ranked_models: Vec<&str> = current.iter().map(|c| c.model.as_str()).collect();
    ranked_models.sort();
    ranked_models.dedup();
    let overrides = ranked_models
        .iter()
        .map(|m| {
            let detail = crate::cost::calculator::price_override_for(m).unwrap_or("");
            if detail.is_empty() {
                format!("{} — none", esc(m))
            } else {
                format!("{} — {}", esc(m), esc(detail))
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    let prices = format!(
        "base rates per call (snapshot {}); overrides: {}",
        esc(crate::cost::calculator::price_snapshot_as_of()),
        if overrides.is_empty() {
            "—"
        } else {
            &overrides
        }
    );
    let mut dates: Vec<&str> = current
        .iter()
        .flat_map(|c| [c.started.as_str(), c.finished_at.as_str()])
        .filter(|s| !s.is_empty())
        .collect();
    dates.sort();
    let first = dates.first().copied().unwrap_or("—");
    let last = dates.last().copied().unwrap_or("—");

    let mut by_model: HashMap<&str, Vec<&CareerCell>> = HashMap::new();
    for c in &current {
        by_model.entry(c.model.as_str()).or_default().push(c);
    }
    let mut series: Vec<(String, Vec<f64>)> = Vec::new();
    for model in &models {
        let cs = by_model.get(model.as_str()).cloned().unwrap_or_default();
        if cs.is_empty() {
            continue;
        }
        let mut v = vec![0.0; 21];
        for c in &cs {
            let s = c.survival();
            for i in 0..=20 {
                v[i] += s[i];
            }
        }
        for x in &mut v {
            *x /= cs.len() as f64;
        }
        series.push((model.clone(), v));
    }

    let mut metrics = String::new();
    for model in &models {
        let cs = by_model.get(model.as_str()).cloned().unwrap_or_default();
        let costs = sorted_unique(cs.iter().filter_map(|c| {
            let n = c.tickets_passed();
            (n > 0).then(|| c.cost / n as f64)
        }));
        let passed: usize = cs.iter().map(|c| c.tickets_passed()).sum();
        let regfree: u32 = cs.iter().map(|c| c.regression_free).sum();
        let rate = (passed > 0).then(|| regfree as f64 / passed as f64);
        metrics.push_str(&format!(
            "<tr><td><b>{}</b></td><td class=\"num\">{:.4} / {:.4} / {:.4}</td><td class=\"num\">{}</td></tr>",
            esc(model),
            costs.first().copied().unwrap_or(0.0),
            median(&costs).unwrap_or(0.0),
            costs.last().copied().unwrap_or(0.0),
            rate.map(|r| format!("{:.1}%", r * 100.0))
                .unwrap_or_else(|| "—".to_string())
        ));
    }

    let mut legend = String::new();
    for (i, m) in models.iter().enumerate() {
        legend.push_str(&format!(
            "<span class=\"key\"><span class=\"dot\" style=\"background:{}\"></span>{}</span>",
            palette(i),
            esc(m)
        ));
    }
    let scatter = svg_scatter(&current, &models);
    let survival = svg_survival(&series);
    let mut scatter_rows = String::new();
    for c in &current {
        let id = esc(&c.campaign_id);
        let name = labels
            .get(&c.campaign_id)
            .map(String::as_str)
            .unwrap_or(&c.campaign_id);
        scatter_rows.push_str(&format!(
            "<tr id=\"{id}\" title=\"{id}\"><td>{}</td><td>{}</td><td class=\"num\">{:.1}</td><td class=\"num\">${:.4}</td></tr>",
            esc(&c.model),
            esc(name),
            c.points,
            c.cost
        ));
    }
    let mut survival_rows = String::new();
    for (model, vals) in &series {
        let cells = (0..=20)
            .map(|i| format!("<td class=\"num\">{:.0}%</td>", vals[i] * 100.0))
            .collect::<String>();
        survival_rows.push_str(&format!("<tr><td>{}</td>{cells}</tr>", esc(model)));
    }
    let mut h = String::new();
    h.push_str("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>SubDollarBench — results</title><style>");
    h.push_str(include_str!("portal_board.css"));
    h.push_str("</style></head><body><main>");
    h.push_str("<header><div><h1>📈 SubDollarBench — results</h1><div class=\"sub\">The career grid: what was measured, how the models spread, and how far each got.</div></div><div class=\"right\"><a href=\"index.html\">← Board</a>");
    h.push_str(&format!(
        "<div class=\"mut\">Updated {}</div></div></header>",
        esc(&chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string())
    ));
    h.push_str(&format!(
        "<section class=\"methodology\"><h2>What it measured</h2><div class=\"wrap\"><table><tbody><tr><th>Rules</th><td>{}</td></tr><tr><th>Fingerprint (recorded)</th><td><code>{}</code>{}</td></tr><tr><th>SQLite</th><td>{}</td></tr><tr><th>Sandbox image</th><td><code>{}</code></td></tr><tr><th>Harness / effort</th><td>{}</td></tr><tr><th>Cap</th><td>$0.99 per phase × 4 phases = ${:.2}</td></tr><tr><th>Prices</th><td>{}</td></tr><tr><th>Grid dates</th><td>{} → {}</td></tr><tr><th>Careers</th><td>{}</td></tr><tr><th>Methodology</th><td><a href=\"https://github.com/lubod/subdollarbench/blob/master/docs/PROPOSAL-sdb-sql.md\">docs/PROPOSAL-sdb-sql.md</a></td></tr></tbody></table></div></section>",
        esc(rules),
        esc(&recorded_text),
        fingerprint_note,
        esc(crate::report::rules::sqlite_version()),
        esc(image),
        esc(&harness_effort),
        CAREER_CAP_USD,
        prices,
        esc(&fmt_grid_time(first)),
        esc(&fmt_grid_time(last)),
        caps
    ));
    let marked: Vec<&CareerCell> = cells.iter().filter(|c| c.rehearsal || c.smoke).collect();
    if !marked.is_empty() {
        h.push_str("<section><h2>Rehearsals and smoke cells (not ranked)</h2><div class=\"wrap\"><table><tbody>");
        for c in marked {
            let id = esc(&c.campaign_id);
            let name = labels
                .get(&c.campaign_id)
                .map(String::as_str)
                .unwrap_or(&c.campaign_id);
            h.push_str(&format!(
                "<tr id=\"{id}\" title=\"{id}\"><td>{}</td><td>{}</td><td class=\"num\">{:.1}</td><td class=\"num\">${:.4}</td></tr>",
                esc(name),
                mark_badge(c),
                c.points,
                c.cost
            ));
        }
        h.push_str("</tbody></table></div></section>");
    }
    h.push_str(&format!(
        "<section><h2>Points against dollars</h2><div class=\"mut\">One dot per career; the line is the non-dominated frontier. Career cap ${CAREER_CAP_USD:.2}.</div>"
    ));
    h.push_str(&format!("<div class=\"wrap\">{scatter}</div>"));
    h.push_str(&format!("<div class=\"wrap\"><table><thead><tr><th>Model</th><th>Career</th><th>Points</th><th>Spend</th></tr></thead><tbody>{scatter_rows}</tbody></table></div></section>"));
    h.push_str("<section><h2>Survival: P(ticket ≥ i)</h2><div class=\"wrap\">");
    h.push_str(&survival);
    h.push_str(&format!("</div><div class=\"wrap\"><table><thead><tr><th>Model</th><th>0</th><th>1</th><th>2</th><th>3</th><th>4</th><th>5</th><th>6</th><th>7</th><th>8</th><th>9</th><th>10</th><th>11</th><th>12</th><th>13</th><th>14</th><th>15</th><th>16</th><th>17</th><th>18</th><th>19</th><th>20</th></tr></thead><tbody>{survival_rows}</tbody></table></div></section>"));
    h.push_str(&format!("<section><h2>Cost and robustness</h2><div class=\"wrap\"><table><thead><tr><th>Model</th><th>$ / ticket (min / median / max)</th><th>Regression-free rate</th></tr></thead><tbody>{metrics}</tbody></table></div></section>"));
    let groups = model_groups(cells);
    let stability = rank_stability(cells, &groups);
    let current_groups: Vec<&ModelGroup> = groups.iter().filter(|g| g.current).collect();
    let mut h2h_rows = String::new();
    for i in 0..models.len() {
        for j in (i + 1)..models.len() {
            let a = by_model
                .get(models[i].as_str())
                .cloned()
                .unwrap_or_default();
            let b = by_model
                .get(models[j].as_str())
                .cloned()
                .unwrap_or_default();
            if a.is_empty() || b.is_empty() {
                continue;
            }
            let (aw, bw, ties) = head_to_head(&a, &b);
            h2h_rows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td class=\"num\">{:.0}%</td><td class=\"num\">{:.0}%</td><td class=\"num\">{:.0}%</td></tr>",
                esc(&models[i]),
                esc(&models[j]),
                aw * 100.0,
                bw * 100.0,
                ties * 100.0
            ));
        }
    }
    h.push_str("<section><h2>Head-to-head</h2><div class=\"mut\">The share of career pairs where model A's career outscores model B's, by points.</div><div class=\"wrap\"><table><thead><tr><th>Model A</th><th>Model B</th><th>A wins</th><th>B wins</th><th>Ties</th></tr></thead><tbody>");
    h.push_str(&h2h_rows);
    h.push_str("</tbody></table></div></section>");
    let vars = variance_rows(cells, &current_groups);
    let mut var_rows = String::new();
    for (name, within, between, ratio) in &vars {
        var_rows.push_str(&format!(
            "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            esc(name),
            fmt_variance(name, *within),
            fmt_variance(name, *between),
            ratio
                .map(|r| format!("{r:.1}×"))
                .unwrap_or_else(|| "—".to_string())
        ));
    }
    h.push_str("<section><h2>Variance check</h2><div class=\"mut\">The publishing bar: the spread between models must exceed the spread within one model's careers.</div><div class=\"wrap\"><table><thead><tr><th>Metric</th><th>Median within-model spread</th><th>Span of model medians</th><th>Between / within</th></tr></thead><tbody>");
    h.push_str(&var_rows);
    h.push_str("</tbody></table></div></section>");
    h.push_str("<section><h2>Model ranks</h2><div class=\"wrap\"><table><thead><tr><th>#</th><th>Model</th><th>Best / median / worst points</th></tr></thead><tbody>");
    for (i, g) in current_groups.iter().enumerate() {
        let stab = stability.as_ref().map(|s| &s[i]);
        h.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"num\">{:.1} / {:.1} / {:.1}</td></tr>",
            rank_label(g, stab),
            esc(&g.model),
            cells[g.best].points,
            cells[g.median].points,
            cells[g.worst].points
        ));
    }
    h.push_str("</tbody></table></div></section>");
    h.push_str("<div class=\"mut\">Dots: <span class=\"key\">");
    h.push_str(&legend);
    h.push_str("</span></div></main></body></html>");
    h
}

pub fn export(runs_dir: &str, out: &str) -> Result<PortalSummary> {
    let cells = careers(runs_dir);
    let ranked: Vec<&CareerCell> = cells
        .iter()
        .filter(|c| c.finished && c.key.current_rules)
        .collect();
    // R47 F5 (R44-13): every ranked career must carry exactly one recorded
    // fingerprint. A career with none used to add nothing to the set and
    // pass silently when the others agreed; one with two faces the same
    // "mixed" problem as two careers with different stamps. Refuse and name
    // the career before writing any output.
    for c in &ranked {
        match c.fingerprints.len() {
            1 => {}
            0 => anyhow::bail!(
                "ranked career {} has no recorded rules fingerprint; refusing to export",
                c.campaign_id
            ),
            n => anyhow::bail!(
                "ranked career {} has {n} recorded rules fingerprints ({}); refusing to export",
                c.campaign_id,
                c.fingerprints.join(", ")
            ),
        }
    }
    let out_dir = Path::new(out);
    fs::create_dir_all(out_dir)
        .with_context(|| format!("create portal dir {}", out_dir.display()))?;
    fs::write(out_dir.join("index.html"), render_board(&cells))?;
    fs::write(out_dir.join("results.html"), render_results(&cells))?;
    let mut ranked_rules: Vec<&str> = ranked.iter().map(|c| c.rules.as_str()).collect();
    ranked_rules.sort();
    ranked_rules.dedup();
    let fingerprint = crate::report::rules::fingerprint();
    let mut ranked_fingerprints: Vec<String> = ranked
        .iter()
        .flat_map(|c| c.fingerprints.iter().cloned())
        .collect();
    ranked_fingerprints.sort();
    ranked_fingerprints.dedup();
    let meta = serde_json::json!({
        "rules_version": cells.iter().map(|c| c.rules.as_str()).find(|r| !r.is_empty()).unwrap_or(crate::report::rules::RULES_VERSION),
        "current_rules": crate::report::rules::RULES_VERSION,
        "ranked_rules": ranked_rules,
        "fingerprint": fingerprint,
        "ranked_fingerprints": ranked_fingerprints,
        "careers": cells.len(),
        "ranked": ranked.len(),
        "ranked_rehearsal": ranked.iter().filter(|c| c.rehearsal || c.smoke).count(),
        "models": model_groups(&cells).iter().map(|g| g.model.clone()).collect::<Vec<_>>(),
    });
    fs::write(
        out_dir.join("grid.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;
    Ok(PortalSummary {
        run_count: cells.len(),
        out_dir: out_dir.to_path_buf(),
    })
}

pub fn variance_report(runs_dir: &str) -> Result<String> {
    let cells = careers(runs_dir);
    let groups = model_groups(&cells);
    let current: Vec<&ModelGroup> = groups.iter().filter(|g| g.current).collect();
    let mut out = String::new();
    for g in &current {
        let points = sorted_unique(g.careers.iter().map(|i| cells[*i].points));
        let costs = sorted_unique(g.careers.iter().map(|i| cells[*i].cost));
        out.push_str(&format!(
            "{} [{} / {} / {}]\n  points: min {:.1} · median {:.1} · max {:.1}\n  $: min {:.4} · median {:.4} · max {:.4}\n",
            g.model,
            g.harness,
            g.effort,
            g.rank_label(),
            points.first().copied().unwrap_or(0.0),
            median(&points).unwrap_or(0.0),
            points.last().copied().unwrap_or(0.0),
            costs.first().copied().unwrap_or(0.0),
            median(&costs).unwrap_or(0.0),
            costs.last().copied().unwrap_or(0.0)
        ));
    }
    for pair in current.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let overlap =
            campaign::board_order(&cells[b.best].key, &cells[a.worst].key) != Ordering::Greater;
        if overlap {
            out.push_str(&format!(
                "{} vs {}: tied ({} best {:.1} >= {} worst {:.1})\n",
                a.model, b.model, b.model, cells[b.best].points, a.model, cells[a.worst].points
            ));
        } else {
            out.push_str(&format!(
                "{} vs {}: ranked ({} worst {:.1} ≥ {} best {:.1})\n",
                a.model, b.model, a.model, cells[a.worst].points, b.model, cells[b.best].points
            ));
        }
    }
    if current.is_empty() {
        out.push_str("no finished current-rules careers\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn stage(id: u32) -> Value {
        serde_json::json!({
            "id": id,
            "title": format!("t{id}"),
            "type": "feature",
            "passed": true,
            "pass_rate": 100.0,
            "passed_stages": 1,
            "total_stages": 1,
            "regressed": [],
            "regression_severity": 0,
            "api_cost_usd": 0.01,
            "effective_cost_usd": 0.01,
            "turns": null,
            "duration_seconds": null,
            "status": "passed",
            "run_id": format!("run{id}"),
            "git_commit": null,
            "git_stat": null,
            "rounds": [],
            "failure": null,
            "variant": "full",
            "tier": 1,
            "points": 1.0
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn write_phase(
        runs: &Path,
        prefix: &str,
        model: &str,
        phase: u32,
        chain: Option<&str>,
        cells: &[String],
        ids: &[u32],
        points: f64,
        cost: f64,
        state: &str,
        career_points: f64,
        career_cost: f64,
    ) {
        let id = format!("campaign_{prefix}_p{phase}");
        let dir = runs.join(&id);
        fs::create_dir_all(&dir).unwrap();
        let stages: Vec<Value> = ids.iter().map(|i| stage(*i)).collect();
        let rollup = serde_json::json!({
            "campaign_id": id,
            "name": "sdb-sql",
            "model": model,
            "harness": "omp",
            "effort": "high",
            "task": "sql",
            "started_at": "2026-10-27T00:00:00Z",
            "finished_at": if state == "done" { "2026-10-27T01:00:00Z" } else { "" },
            "stages": stages,
            "reached": ids.iter().copied().max(),
            "regression_free_tickets": ids.len(),
            "total_cost_usd": cost,
            "over_budget": false,
            "cap_reached": false,
            "over_wall": false,
            "rules_version": crate::report::rules::RULES_VERSION,
            "sqlite_version": "3.0.0",
            "cap_price_basis": "list",
            "sandbox_image": "sha256:test-image",
            "budget_usd": 0.99,
            "wall_min": 150,
            "points": points,
            "max_points": 10.0,
            "phase": phase,
            "chain_from": chain,
            "career": {"points": career_points, "total_cost_usd": career_cost, "cells": cells},
            "ladder_state": null,
            "refork": null,
            "harness_fault": null
        });
        fs::write(dir.join("campaign.json"), rollup.to_string()).unwrap();
        fs::write(
            dir.join("status.json"),
            serde_json::json!({
                "state": state,
                "updated_at": "2026-10-27T01:00:00Z",
                "rollup": rollup,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn career(runs: &Path, prefix: &str, model: &str, points: [f64; 4], cost: [f64; 4]) {
        career_with_stamp(
            runs,
            prefix,
            model,
            points,
            cost,
            &crate::report::rules::fingerprint(),
        );
    }

    fn career_with_stamp(
        runs: &Path,
        prefix: &str,
        model: &str,
        points: [f64; 4],
        cost: [f64; 4],
        stamp: &str,
    ) {
        let ids = [
            (1u32, vec![0, 1, 2, 3, 4, 5, 6]),
            (2u32, vec![7, 8, 9, 10]),
            (3u32, vec![11, 12, 13, 14, 15]),
            (4u32, vec![16, 17, 18, 19, 20]),
        ];
        let cells: Vec<String> = (1..=4).map(|p| format!("campaign_{prefix}_p{p}")).collect();
        let mut prior = None;
        let career_points: f64 = points.iter().sum();
        let career_cost: f64 = cost.iter().sum();
        for (i, (phase, stage_ids)) in ids.iter().enumerate() {
            write_phase(
                runs,
                prefix,
                model,
                *phase,
                prior.as_deref(),
                &cells,
                stage_ids,
                points[i],
                cost[i],
                "done",
                career_points,
                career_cost,
            );
            for id in stage_ids {
                let dir = runs.join(format!("campaign_{prefix}_p{phase}/s{id}_full"));
                fs::create_dir_all(&dir).unwrap();
                fs::write(
                    dir.join("manifest.json"),
                    serde_json::json!({
                        "run_id": format!("run{id}"),
                        "rules_version": stamp,
                    })
                    .to_string(),
                )
                .unwrap();
            }
            prior = Some(format!("campaign_{prefix}_p{phase}"));
        }
    }

    /// R47 F2: attach a perf block to a career's leaf (the career block
    /// carries the latest scored phase, as the runner writes it).
    fn set_career_perf(runs: &Path, prefix: &str, score: f64, workloads: Value) {
        let path = runs.join(format!("campaign_{prefix}_p4/campaign.json"));
        let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
        let perf = serde_json::json!({"workloads": workloads, "score": score});
        v["perf"] = perf.clone();
        v["career"]["perf"] = perf;
        fs::write(&path, v.to_string()).unwrap();
        let spath = runs.join(format!("campaign_{prefix}_p4/status.json"));
        let mut sv: Value = serde_json::from_str(&read(&spath)).unwrap();
        sv["rollup"] = v;
        fs::write(&spath, sv.to_string()).unwrap();
    }

    /// R47 F4: mark a cell as the refork that superseded `of`.
    fn set_refork(runs: &Path, prefix: &str, phase: u32, of: &str) {
        let path = runs.join(format!("campaign_{prefix}_p{phase}/campaign.json"));
        let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
        v["refork"] = serde_json::json!({"of": of, "reason": "retry"});
        fs::write(&path, v.to_string()).unwrap();
        let spath = runs.join(format!("campaign_{prefix}_p{phase}/status.json"));
        let mut sv: Value = serde_json::from_str(&read(&spath)).unwrap();
        sv["rollup"] = v;
        fs::write(&spath, sv.to_string()).unwrap();
    }

    /// R54 F1: set `rehearsal`/`smoke` on a phase rollup in both files.
    fn set_mark(runs: &Path, prefix: &str, phase: u32, key: &str) {
        let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
        for file in ["campaign.json", "status.json"] {
            let path = dir.join(file);
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            let rollup = if file == "status.json" {
                v.get_mut("rollup").unwrap()
            } else {
                &mut v
            };
            rollup[key] = serde_json::json!(true);
            fs::write(path, v.to_string()).unwrap();
        }
    }

    /// R51 F4: rewrite a phase's start/finish in both files.
    fn set_phase_times(runs: &Path, prefix: &str, phase: u32, started: &str, finished: &str) {
        let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
        for file in ["campaign.json", "status.json"] {
            let path = dir.join(file);
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            let rollup = if file == "status.json" {
                v.get_mut("rollup").unwrap()
            } else {
                &mut v
            };
            rollup["started_at"] = serde_json::json!(started);
            rollup["finished_at"] = serde_json::json!(finished);
            fs::write(path, v.to_string()).unwrap();
        }
    }

    /// R64 F1: rewrite a phase's recorded rules version in both files.
    fn set_phase_rules(runs: &Path, prefix: &str, phase: u32, rules: &str) {
        let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
        for file in ["campaign.json", "status.json"] {
            let path = dir.join(file);
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            let rollup = if file == "status.json" {
                v.get_mut("rollup").unwrap()
            } else {
                &mut v
            };
            rollup["rules_version"] = serde_json::json!(rules);
            fs::write(path, v.to_string()).unwrap();
        }
    }

    /// R64 F5: rewrite every stage's turns and give it a 1000 s agent
    /// duration, in both files.
    fn set_turns(runs: &Path, prefix: &str, phase: u32, turns: &[u32]) {
        let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
        for file in ["campaign.json", "status.json"] {
            let path = dir.join(file);
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            let rollup = if file == "status.json" {
                v.get_mut("rollup").unwrap()
            } else {
                &mut v
            };
            for (i, st) in rollup["stages"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                st["turns"] = serde_json::json!(turns.get(i).copied().unwrap_or(0));
                st["duration_seconds"] = serde_json::json!(1000.0);
            }
            fs::write(path, v.to_string()).unwrap();
        }
    }

    /// R64 F5: write one ticket manifest's token block.
    #[allow(clippy::too_many_arguments)]
    fn set_tokens(
        runs: &Path,
        prefix: &str,
        phase: u32,
        id: u32,
        prompt: u64,
        cached: u64,
        completion: u64,
        total: u64,
    ) {
        let path = runs.join(format!(
            "campaign_{prefix}_p{phase}/s{id}_full/manifest.json"
        ));
        let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
        v["tokens"] = serde_json::json!({
            "prompt_tokens": prompt,
            "cached_tokens": cached,
            "completion_tokens": completion,
            "total_tokens": total,
        });
        fs::write(path, v.to_string()).unwrap();
    }

    /// R64 F6: mark one ticket failed in a career's phase cell.
    fn fail_ticket(runs: &Path, prefix: &str, id: u32) {
        for phase in 1..=4 {
            let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
            if !dir.exists() {
                continue;
            }
            for file in ["campaign.json", "status.json"] {
                let path = dir.join(file);
                let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
                let rollup = if file == "status.json" {
                    v.get_mut("rollup").unwrap()
                } else {
                    &mut v
                };
                if let Some(stages) = rollup["stages"].as_array_mut() {
                    for st in stages {
                        if st["id"].as_u64() == Some(id as u64) {
                            st["passed"] = serde_json::json!(false);
                            st["status"] = serde_json::json!("failed");
                        }
                    }
                }
                fs::write(path, v.to_string()).unwrap();
            }
        }
    }

    /// R51 F3: rewrite (or remove) a phase's effort in both files.
    fn set_effort(runs: &Path, prefix: &str, phase: u32, effort: Option<&str>) {
        let dir = runs.join(format!("campaign_{prefix}_p{phase}"));
        for file in ["campaign.json", "status.json"] {
            let path = dir.join(file);
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            let rollup = if file == "status.json" {
                v.get_mut("rollup").unwrap()
            } else {
                &mut v
            };
            match effort {
                Some(e) => rollup["effort"] = serde_json::json!(e),
                None => {
                    rollup.as_object_mut().unwrap().remove("effort");
                }
            }
            fs::write(path, v.to_string()).unwrap();
        }
    }

    fn fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let runs = base.join("runs");
        let out = base.join("portal");
        fs::create_dir_all(&runs).unwrap();
        (base, runs, out)
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn pos(hay: &str, needle: &str) -> usize {
        hay.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}: {hay}"))
    }

    /// R64 F4: the text a reader sees, with every tag and attribute removed.
    fn visible_text(html: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for ch in html.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out
    }

    #[test]
    fn r44_f1_careers_on_the_board() {
        let (base, runs, out) = fixture("r44f1");
        career(
            &runs,
            "a1",
            "alpha",
            [1.0, 2.0, 3.0, 4.0],
            [0.01, 0.01, 0.01, 0.01],
        );
        career(
            &runs,
            "a2",
            "alpha",
            [2.0, 3.0, 4.0, 5.0],
            [0.01, 0.01, 0.01, 0.01],
        );
        career(
            &runs,
            "a3",
            "alpha",
            [3.0, 4.0, 5.0, 6.0],
            [0.01, 0.01, 0.01, 0.01],
        );
        career(
            &runs,
            "b1",
            "beta",
            [2.0, 2.0, 2.0, 2.0],
            [0.02, 0.02, 0.02, 0.02],
        );
        career(
            &runs,
            "b2",
            "beta",
            [3.0, 3.0, 3.0, 3.0],
            [0.02, 0.02, 0.02, 0.02],
        );
        career(
            &runs,
            "b3",
            "beta",
            [4.0, 4.0, 4.0, 4.0],
            [0.02, 0.02, 0.02, 0.02],
        );
        // One unfinished career: phase 2 of 4, still running.
        write_phase(
            &runs,
            "u1",
            "unfinished",
            1,
            None,
            &["campaign_u1_p1".into()],
            &[0, 1, 2, 3, 4, 5, 6],
            1.0,
            0.01,
            "done",
            1.0,
            0.01,
        );
        write_phase(
            &runs,
            "u1",
            "unfinished",
            2,
            Some("campaign_u1_p1"),
            &["campaign_u1_p1".into(), "campaign_u1_p2".into()],
            &[7, 8, 9, 10],
            2.0,
            0.01,
            "running",
            3.0,
            0.02,
        );
        let summary = export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        assert_eq!(summary.run_count, 7);
        let html = read(&out.join("index.html"));
        assert_eq!(html.matches("class=\"model-row\"").count(), 2, "{html}");
        assert_eq!(html.matches("class=\"career-row\"").count(), 6, "{html}");
        assert_eq!(
            html.matches("class=\"career-row unfinished\"").count(),
            1,
            "{html}"
        );
        assert!(
            pos(&html, "unfinished careers") < pos(&html, "campaign_u1_p2"),
            "{html}"
        );
        assert!(
            pos(&html, "phase 2 of 4") < pos(&html, "campaign_u1_p2"),
            "{html}"
        );
        assert!(
            pos(&html, "alpha") < pos(&html, "beta"),
            "median order: {html}"
        );
        assert!(
            !html.contains("Benchmark Runner"),
            "public board links to the console: {html}"
        );
        assert!(
            !html.contains("href=\"/\""),
            "public board links to the console: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f1_career_strip_merges_phases() {
        let (base, runs, out) = fixture("r44strip");
        career(&runs, "s", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        for id in 0..=20 {
            assert!(
                html.contains(&format!("data-ticket=\"{id}\"")),
                "ticket {id}: {html}"
            );
        }
        let t = |id: u32| {
            let marker = format!("data-ticket=\"{id}\"");
            let at = pos(&html, &marker);
            let end = html[at..].find("</span>").unwrap() + at;
            &html[at..end]
        };
        assert!(t(0).contains("pass"));
        assert!(t(7).contains("pass"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f1_model_row_is_the_median_career() {
        let (base, runs, out) = fixture("r44median");
        career(&runs, "m1", "m", [1.0; 4], [0.01; 4]);
        career(&runs, "m2", "m", [2.0; 4], [0.01; 4]);
        career(&runs, "m3", "m", [3.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        // R64 F3: the row is median (worst-best) · careers, not a repeated
        // best/median/worst line.
        assert!(
            html.contains("8.0 (4-12) · 3 careers"),
            "the model row is the median with its range: {html}"
        );
        assert!(
            !html.contains("best 12.0 · median 8.0 · worst 4.0"),
            "the old repeated median line is gone: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_r1_recorded_fingerprint_is_not_the_binary() {
        let (base, runs, out) = fixture("r44fp");
        career_with_stamp(&runs, "f1", "m1", [1.0; 4], [0.01; 4], "fp-a");
        career_with_stamp(&runs, "f2", "m2", [2.0; 4], [0.01; 4], "fp-b");
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let meta: Value = serde_json::from_str(&read(&out.join("grid.json"))).unwrap();
        let fps: Vec<String> = meta["ranked_fingerprints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(fps, vec!["fp-a".to_string(), "fp-b".to_string()]);
        assert_ne!(meta["fingerprint"].as_str().unwrap(), "fp-a");
        let html = read(&out.join("results.html"));
        assert!(html.contains("fp-a") && html.contains("fp-b"), "{html}");
        assert!(html.contains("recorded fingerprint differs"), "{html}");

        let bar = base.join("bar.txt");
        fs::write(&bar, "RESULT: PASS\n").unwrap();
        let status = std::process::Command::new("bash")
            .arg(env!("CARGO_MANIFEST_DIR").to_string() + "/scripts/publish.sh")
            .args(["--check", out.to_str().unwrap(), bar.to_str().unwrap()])
            .status()
            .expect("run publish check");
        assert!(
            !status.success(),
            "mixed recorded fingerprints were accepted"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f2_pareto_frontier() {
        let (base, runs, out) = fixture("r44pareto");
        career(&runs, "c1", "m1", [2.5; 4], [0.075; 4]); // 10 pts / 0.3
        career(&runs, "c2", "m1", [5.0; 4], [0.1; 4]); // 20 pts / 0.4
        career(&runs, "c3", "m2", [1.25; 4], [0.125; 4]); // 5 / 0.5
        career(&runs, "c4", "m2", [3.75; 4], [0.05; 4]); // 15 / 0.2
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        // The non-dominated careers are c2 (20/0.4) and c4 (15/0.2).
        assert!(html.contains("data-cost=\"0.4000\""), "{html}");
        assert!(html.contains("data-cost=\"0.2000\""), "{html}");
        assert!(html.contains("pareto-frontier"), "{html}");
        let mut ids = pareto_ids(&careers(runs.to_str().unwrap()));
        ids.sort();
        assert_eq!(ids, vec!["campaign_c2_p4", "campaign_c4_p4"], "{html}");
        assert!(
            !html.contains("data-cost=\"0.1000\" data-points=\"10.0\""),
            "{html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f2_metrics() {
        let (base, runs, out) = fixture("r44metrics");
        career(&runs, "c1", "m", [4.0; 4], [0.1; 4]); // 16 pts, $0.40, 21 tickets
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(html.contains("$ / ticket"), "{html}");
        assert!(html.contains("Regression-free rate"), "{html}");
        assert!(
            html.contains("100.0%"),
            "all 21 tickets regression-free: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f2_survival_is_monotone() {
        let (base, runs, out) = fixture("r44survival");
        career(&runs, "s", "m", [1.0; 4], [0.01; 4]);
        // Fail ticket 4 and the last ticket 20; later tickets still pass.
        for (file, id) in [("campaign_s_p1", 4u32), ("campaign_s_p4", 20u32)] {
            let path = runs.join(file).join("campaign.json");
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            for st in v["stages"].as_array_mut().unwrap() {
                if st["id"].as_u64() == Some(id as u64) {
                    st["passed"] = serde_json::json!(false);
                }
            }
            fs::write(&path, v.to_string()).unwrap();
        }
        let cells = careers(runs.to_str().unwrap());
        assert_eq!(cells.len(), 1);
        let s = cells
            .iter()
            .find(|c| c.model == "m")
            .expect("survival career")
            .survival();
        assert_eq!(s[4], 1.0, "a later pass keeps reached above 4");
        assert_eq!(s[19], 1.0);
        assert_eq!(s[20], 0.0);
        for i in 0..20 {
            assert!(s[i] >= s[i + 1], "survival rose at {i}: {s:?}");
        }

        // A career that passed nothing has no reached ticket: all zeros,
        // never a free 1.0 at ticket 0.
        career(&runs, "none", "none", [1.0; 4], [0.01; 4]);
        for phase in 1..=4 {
            let path = runs.join(format!("campaign_none_p{phase}/campaign.json"));
            let mut v: Value = serde_json::from_str(&read(&path)).unwrap();
            for st in v["stages"].as_array_mut().unwrap() {
                st["passed"] = serde_json::json!(false);
            }
            fs::write(&path, v.to_string()).unwrap();
        }
        let none = careers(runs.to_str().unwrap())
            .into_iter()
            .find(|c| c.model == "none")
            .expect("none career");
        assert!(none.reached.is_none());
        assert!(none.survival().iter().all(|v| *v == 0.0));
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f1_dropped_phase_career_is_not_published() {
        let (base, runs, out) = fixture("r44dropped");
        career(&runs, "drop", "dropped-model", [1.0; 4], [0.01; 4]);
        fs::write(
            runs.join("campaign_drop_p2").join(".poisoned"),
            "bad phase\n",
        )
        .unwrap();
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(
            !html.contains("<b>dropped-model</b>"),
            "dropped phase model row leaked: {html}"
        );
        assert!(
            !html.contains("campaign_drop_p"),
            "dropped phase id leaked: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f2_results_page_names_what_it_measured() {
        let (base, runs, out) = fixture("r44names");
        career(&runs, "c1", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(html.contains(crate::report::rules::RULES_VERSION), "{html}");
        assert!(
            html.contains(&crate::report::rules::fingerprint()),
            "{html}"
        );
        assert!(html.contains("sha256:test-image"), "{html}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R47 F5 (R44-13): a ranked career must carry exactly one recorded
    /// fingerprint. None, or more than one, refuses the export by name.
    #[test]
    fn r47_f5_career_without_fingerprint_refused() {
        let (base, runs, out) = fixture("r47nofp");
        career_with_stamp(&runs, "ok1", "m1", [1.0; 4], [0.01; 4], "fp-one");
        career_with_stamp(&runs, "ok2", "m2", [2.0; 4], [0.01; 4], "fp-one");
        career_with_stamp(&runs, "nofp", "m3", [3.0; 4], [0.01; 4], "");
        let err = export(runs.to_str().unwrap(), out.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("campaign_nofp_p4"),
            "the refusal names the career: {err}"
        );
        assert!(err.contains("no recorded rules fingerprint"), "{err}");
        assert!(
            !out.join("index.html").exists(),
            "a refused export writes nothing"
        );

        // A career with two distinct stamps is refused for the same reason.
        let (base2, runs2, out2) = fixture("r47twofp");
        career_with_stamp(&runs2, "mixed", "m", [1.0; 4], [0.01; 4], "fp-one");
        career_with_stamp(&runs2, "ok", "m2", [2.0; 4], [0.01; 4], "fp-one");
        let manifest = runs2.join("campaign_mixed_p2/s7_full/manifest.json");
        let mut v: Value = serde_json::from_str(&read(&manifest)).unwrap();
        v["rules_version"] = serde_json::json!("fp-two");
        fs::write(&manifest, v.to_string()).unwrap();
        let err = export(runs2.to_str().unwrap(), out2.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("campaign_mixed_p4"), "{err}");
        assert!(err.contains("2 recorded rules fingerprints"), "{err}");
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&base2);
    }

    /// R47 F4 (R44-7): a refork supersedes the named successor and its
    /// later phases, so the board gives one career: the refork's.
    #[test]
    fn r47_f4_refork_supersedes() {
        let (base, runs, _out) = fixture("r47refork");
        // Predecessor.
        write_phase(
            &runs,
            "rel",
            "forked",
            1,
            None,
            &["campaign_rel_p1".into()],
            &[0, 1, 2, 3, 4, 5, 6],
            1.0,
            0.01,
            "done",
            1.0,
            0.01,
        );
        // The superseded branch.
        let mut old_cells = vec!["campaign_rel_p1".to_string()];
        let mut old_prior = "campaign_rel_p1".to_string();
        for (phase, ids, pts) in [
            (2u32, vec![7, 8, 9, 10], 2.0),
            (3, vec![11, 12, 13, 14, 15], 3.0),
            (4, vec![16, 17, 18, 19, 20], 4.0),
        ] {
            let prefix = "relold";
            old_cells.push(format!("campaign_{prefix}_p{phase}"));
            write_phase(
                &runs,
                prefix,
                "forked",
                phase,
                Some(&old_prior),
                &old_cells,
                &ids,
                pts,
                0.01,
                "done",
                1.0 + pts,
                0.02,
            );
            old_prior = format!("campaign_{prefix}_p{phase}");
        }
        // The refork branch, naming the old phase-2 cell as superseded.
        let mut new_cells = vec!["campaign_rel_p1".to_string()];
        let mut new_prior = "campaign_rel_p1".to_string();
        for (phase, ids, pts) in [
            (2u32, vec![7, 8, 9, 10], 2.0),
            (3, vec![11, 12, 13, 14, 15], 3.0),
            (4, vec![16, 17, 18, 19, 20], 4.0),
        ] {
            let prefix = "relnew";
            new_cells.push(format!("campaign_{prefix}_p{phase}"));
            write_phase(
                &runs,
                prefix,
                "forked",
                phase,
                Some(&new_prior),
                &new_cells,
                &ids,
                pts,
                0.01,
                "done",
                1.0 + pts,
                0.02,
            );
            new_prior = format!("campaign_{prefix}_p{phase}");
        }
        set_refork(&runs, "relnew", 2, "campaign_relold_p2");
        assert!(runs.join("campaign_relold_p4").is_dir(), "kept on disk");
        let cells = careers(runs.to_str().unwrap());
        assert_eq!(cells.len(), 1, "one career survives a refork: {cells:?}");
        assert_eq!(cells[0].campaign_id, "campaign_relnew_p4");
        assert!(
            !cells[0].cells.iter().any(|c| c.contains("relold")),
            "the superseded branch leaked into the career: {:?}",
            cells[0].cells
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R47 F3 (R44-6): a ranked pair reads in the ranking's direction:
    /// the better model's worst is at least the worse model's best.
    #[test]
    fn r47_f3_variance_line_direction() {
        let (base, runs, _out) = fixture("r47varline");
        career(&runs, "a1", "a", [5.0; 4], [0.01; 4]);
        career(&runs, "b1", "b", [2.5; 4], [0.01; 4]);
        let report = variance_report(runs.to_str().unwrap()).unwrap();
        assert!(
            report.contains("a vs b: ranked (a worst 20.0 ≥ b best 10.0)"),
            "{report}"
        );
        assert!(
            !report.contains("a worst 20.0 < b best 10.0"),
            "the line reads backwards: {report}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R47 F3 (R44-8): with an even count the median is the worse middle;
    /// with three careers it is the middle. The list is best-first.
    #[test]
    fn r47_f3_even_median_is_the_worse() {
        let (base, runs, _out) = fixture("r47median");
        career(&runs, "two-hi", "two", [5.0; 4], [0.01; 4]);
        career(&runs, "two-lo", "two", [2.5; 4], [0.01; 4]);
        career(&runs, "three-hi", "three", [7.5; 4], [0.01; 4]);
        career(&runs, "three-mid", "three", [5.0; 4], [0.01; 4]);
        career(&runs, "three-lo", "three", [2.5; 4], [0.01; 4]);
        let cells = careers(runs.to_str().unwrap());
        let groups = model_groups(&cells);
        let two = groups.iter().find(|g| g.model == "two").unwrap();
        assert_eq!(
            cells[two.median].points, 10.0,
            "2 careers: the worse middle is the median"
        );
        let three = groups.iter().find(|g| g.model == "three").unwrap();
        assert_eq!(
            cells[three.median].points, 20.0,
            "3 careers: the middle is the median"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r44_f3_variance_ties() {
        let (base, runs, out) = fixture("r44variance");
        career(&runs, "a1", "a", [5.0; 4], [0.01; 4]); // 20
        career(&runs, "a2", "a", [5.5; 4], [0.01; 4]); // 22
        career(&runs, "a3", "a", [6.0; 4], [0.01; 4]); // 24
        career(&runs, "b1", "b", [3.0; 4], [0.01; 4]); // 12
        career(&runs, "b2", "b", [3.25; 4], [0.01; 4]); // 13
        career(&runs, "b3", "b", [3.5; 4], [0.01; 4]); // 14
        career(&runs, "c1", "c", [2.0; 4], [0.01; 4]); // 8
        career(&runs, "c2", "c", [2.25; 4], [0.01; 4]); // 9
        career(&runs, "c3", "c", [3.25; 4], [0.01; 4]); // 13 (overlaps b)
        let report = variance_report(runs.to_str().unwrap()).unwrap();
        assert!(report.contains("ranked"), "{report}");
        assert!(report.contains("tied"), "{report}");
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        // R64 F6/D1: exact rank stability replaces the N= groups. b holds
        // #2 in 8 of 9 combinations (c=13 with b=12 is the exception).
        assert!(
            html.contains("<td class=\"num\">#2 (89%)</td>"),
            "the model row has no exact rank cell: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }
    /// R54 F1: a rehearsal is badged and never ranked, on the board and
    /// the results page.
    #[test]
    fn r54_f1_rehearsal_never_ranked() {
        let (base, runs, out) = fixture("r54rehearsal");
        career(&runs, "reh", "m", [1.0; 4], [0.01; 4]);
        set_mark(&runs, "reh", 4, "rehearsal");
        let cells = careers(runs.to_str().unwrap());
        assert!(cells[0].rehearsal && !cells[0].finished);
        assert!(model_groups(&cells).is_empty(), "a rehearsal must not rank");
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let board = read(&out.join("index.html"));
        assert!(board.contains("REHEARSAL"), "{board}");
        assert!(board.contains("rehearsal (not ranked)"), "{board}");
        let results = read(&out.join("results.html"));
        assert!(results.contains("REHEARSAL"), "{results}");
        assert!(results.contains("not ranked"), "{results}");
        let meta: Value = serde_json::from_str(&read(&out.join("grid.json"))).unwrap();
        assert_eq!(meta["ranked"], 0);
        let _ = fs::remove_dir_all(&base);
    }

    /// R54 F1: a career whose phase 1 is a smoke cell is badged and
    /// unranked even though phase 4 has no mark of its own.
    #[test]
    fn r54_f1_smoke_career_never_ranked() {
        let (base, runs, out) = fixture("r54smoke");
        career(&runs, "smk", "m", [1.0; 4], [0.01; 4]);
        set_mark(&runs, "smk", 1, "smoke");
        let cells = careers(runs.to_str().unwrap());
        assert!(cells[0].smoke && !cells[0].finished);
        assert!(
            model_groups(&cells).is_empty(),
            "a smoke career must not rank"
        );
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let board = read(&out.join("index.html"));
        assert!(board.contains("SMOKE"), "{board}");
        assert!(board.contains("smoke (not ranked)"), "{board}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R64 F1 (B1, B2, B3): the header counts ranked careers only, points
    /// print with at most one decimal, and an older rule set sits under
    /// "older rule sets", never under "unfinished".
    #[test]
    fn r64_f1_header_counts_ranked_only() {
        let (base, runs, out) = fixture("r64f1");
        career(&runs, "paid", "paid-model", [17.5; 4], [0.1; 4]); // 70
        career(&runs, "haiku", "haiku-model", [14.4; 4], [0.1; 4]); // 57.599999999999994
        career(&runs, "reh", "reh-model", [17.75; 4], [0.0; 4]); // 71, $0
        set_mark(&runs, "reh", 4, "rehearsal");
        // B3: an unfinished cell from an older rule set is incomparable,
        // not "phase 1 of 4" among the unfinished careers.
        write_phase(
            &runs,
            "old",
            "old-model",
            1,
            None,
            &["campaign_old_p1".into()],
            &[0, 1, 2, 3, 4, 5, 6],
            9.0,
            0.05,
            "done",
            9.0,
            0.05,
        );
        set_phase_rules(&runs, "old", 1, "2026-10-19.44");
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(
            html.contains("<div class=\"k\">Careers</div><div class=\"v\">2</div>"),
            "only the two ranked careers count: {html}"
        );
        assert!(
            html.contains("<div class=\"k\">Models</div><div class=\"v\">2</div>"),
            "the old and rehearsal models count nowhere: {html}"
        );
        assert!(
            html.contains("<div class=\"k\">Best points</div><div class=\"v\">70.0</div>"),
            "the $0 rehearsal's 71 must not be the header best: {html}"
        );
        assert!(
            html.contains("$0.8000"),
            "spend is the ranked careers' $0.80: {html}"
        );
        assert!(
            html.contains("57.6") && !html.contains("57.599999999999994"),
            "points print with one decimal: {html}"
        );
        assert!(
            pos(&html, "older rule sets") < pos(&html, "old-model"),
            "the .44 cell is under older rule sets: {html}"
        );
        assert!(
            pos(&html, "unfinished careers") < pos(&html, "older rule sets"),
            "the .44 cell is not in the unfinished section: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R64 F3 (M3, H5): an unfinished career raises the in-progress banner,
    /// a finished grid doesn't, and the model row is the median with its
    /// range and career count.
    #[test]
    fn r64_f3_banner_and_model_row() {
        let (base, runs, out) = fixture("r64f3");
        career(&runs, "lo", "m", [15.75; 4], [0.01; 4]); // 63
        career(&runs, "mid", "m", [16.25; 4], [0.01; 4]); // 65
        career(&runs, "hi", "m", [17.5; 4], [0.01; 4]); // 70
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(
            html.contains("65.0 (63-70) · 3 careers"),
            "the model row is median · worst-best · careers: {html}"
        );
        assert!(
            !html.contains("best 70.0 · median 65.0 · worst 63.0"),
            "the old repeated median line is gone: {html}"
        );
        assert!(
            !html.contains("Grid in progress"),
            "a finished grid has no banner: {html}"
        );
        let _ = fs::remove_dir_all(&base);

        let (base2, runs2, out2) = fixture("r64f3prog");
        career(&runs2, "done", "m", [17.5; 4], [0.01; 4]); // 70, finished
        write_phase(
            &runs2,
            "run",
            "m",
            1,
            None,
            &["campaign_run_p1".into()],
            &[0, 1, 2, 3, 4, 5, 6],
            9.0,
            0.02,
            "done",
            9.0,
            0.02,
        );
        export(runs2.to_str().unwrap(), out2.to_str().unwrap()).unwrap();
        let html2 = read(&out2.join("index.html"));
        assert!(
            html2.contains("Grid in progress: 1 of 2 careers finished; ranks are provisional."),
            "one unfinished career raises the banner: {html2}"
        );
        let _ = fs::remove_dir_all(&base2);
    }

    /// R64 F4 (H1-H4): plain career names, one header for the rules, the
    /// SQLite version and the cap, renamed columns, and no per-row rules
    /// or Updated.
    #[test]
    fn r64_f4_plain_names() {
        let (base, runs, out) = fixture("r64f4");
        career(&runs, "c", "m", [17.5; 4], [0.01; 4]); // 70
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let text = visible_text(&html);
        assert!(
            !text.contains("campaign_"),
            "no campaign id is visible text: {text}"
        );
        assert!(
            text.contains("m · career 1 · 2026-10-27"),
            "the career name is model · career i · date: {text}"
        );
        assert!(
            html.contains("title=\"campaign_c_p4\"") && html.contains("results.html#campaign_c_p4"),
            "the campaign id lives in the tooltip and the link: {html}"
        );
        assert_eq!(
            text.matches(crate::report::rules::RULES_VERSION).count(),
            1,
            "the rules version shows once, in the header: {text}"
        );
        assert_eq!(
            text.matches("Updated").count(),
            1,
            "only the page's Updated line survives: {text}"
        );
        assert!(
            html.contains("70 / 76"),
            "the Points cell is `70 / 76`: {html}"
        );
        assert!(
            html.contains(">Highest ticket</th>") && html.contains(">Tickets kept green</th>"),
            "the columns are renamed: {html}"
        );
        assert!(
            !html.contains(">Reached</th>")
                && !html.contains(">Reg-free</th>")
                && !html.contains("Reg-free / $")
                && !html.contains(">Updated</th>"),
            "the retired columns are gone: {html}"
        );
        assert!(
            text.contains("A career is 4 phases, 21 tickets, at most $0.99 per phase."),
            "the one-line definition is under the title: {text}"
        );
        let results = read(&out.join("results.html"));
        assert!(
            results.contains("id=\"campaign_c_p4\""),
            "the results page anchors the career the board links to: {results}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R64 F5: duration is the phases' summed wall time, turns the stages'
    /// sum, and tokens the manifests' sums with fresh input derived as
    /// total - cached - completion.
    #[test]
    fn r64_f5_duration_turns_tokens() {
        let (base, runs, out) = fixture("r64f5");
        career(&runs, "t", "m", [1.0; 4], [0.01; 4]);
        let times = [
            ("00:00", "01:00"),
            ("01:00", "02:00"),
            ("02:00", "03:30"),
            ("03:30", "05:24"),
        ];
        for (i, (start, end)) in times.iter().enumerate() {
            set_phase_times(
                &runs,
                "t",
                i as u32 + 1,
                &format!("2026-10-27T{start}:00Z"),
                &format!("2026-10-27T{end}:00Z"),
            );
        }
        set_turns(&runs, "t", 1, &[100; 7]);
        set_turns(&runs, "t", 2, &[100; 4]);
        set_turns(&runs, "t", 3, &[100; 5]);
        set_turns(&runs, "t", 4, &[78, 77, 77, 77, 77]);
        // One route left prompt_tokens at 0; fresh input comes from the
        // total after cached and completion.
        set_tokens(&runs, "t", 1, 0, 0, 100_000_000, 1_000_000, 101_000_000);
        set_tokens(&runs, "t", 2, 7, 45_511, 54_500_000, 740_000, 61_100_000);

        let cells = careers(runs.to_str().unwrap());
        let c = &cells[0];
        assert!(
            (c.wall_seconds - 5.4 * 3600.0).abs() < 1.0,
            "phase wall time sums to 5.4 h: {}",
            c.wall_seconds
        );
        assert_eq!(c.turns, 1986, "stage turns sum");
        let tokens = c.tokens.expect("two manifests carry tokens");
        assert_eq!(tokens.completion, 1_740_000);
        assert_eq!(tokens.cached, 154_500_000);
        assert_eq!(tokens.fresh_input(), 5_860_000);

        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(html.contains("5.4 h"), "the career duration: {html}");
        assert!(html.contains("1,986"), "the career turns: {html}");
        assert!(
            html.contains("out 1.7M · in 5.9M (+154.5M cached)"),
            "the token cell: {html}"
        );
        assert!(
            html.contains("cache share 95.3%"),
            "the tooltip gives the cache share: {html}"
        );
        assert!(
            html.contains("agent 5.8 h"),
            "the tooltip gives the agent's share: {html}"
        );
        let _ = fs::remove_dir_all(&base);

        // A career whose manifests carry no tokens shows an em dash.
        let (base2, runs2, _out2) = fixture("r64f5notok");
        career(&runs2, "n", "n", [1.0; 4], [0.01; 4]);
        let cells2 = careers(runs2.to_str().unwrap());
        assert!(cells2[0].tokens.is_none());
        assert_eq!(token_cell(&cells2[0]), "—");
        let _ = fs::remove_dir_all(&base2);
    }

    /// R64 F6/D1: rank stability is exact over every combination of one
    /// career per model; 3x3 is 27 hand-counted combinations.
    #[test]
    fn r64_f6_rank_stability_exact() {
        let (base, runs, out) = fixture("r64f6rank");
        for (prefix, model, points) in [
            ("a1", "A", 70.0),
            ("a2", "A", 50.0),
            ("a3", "A", 40.0),
            ("b1", "B", 60.0),
            ("b2", "B", 55.0),
            ("b3", "B", 35.0),
            ("c1", "C", 30.0),
            ("c2", "C", 20.0),
            ("c3", "C", 10.0),
        ] {
            career(&runs, prefix, model, [points / 4.0; 4], [0.01; 4]);
        }
        let cells = careers(runs.to_str().unwrap());
        let groups = model_groups(&cells);
        let stab = rank_stability(&cells, &groups).expect("27 combinations enumerate");
        assert_eq!(stab[0].total, 27);
        let of = |m: &str| {
            let i = groups.iter().position(|g| g.model == m).unwrap();
            &stab[i]
        };
        // A: #1 in 15 of 27 (9 with 70; 3 with 50 over B=35; 3 with 40
        // over B=35). B: #1 in the other 12 (B=60/55 over A=50/40).
        assert_eq!(of("A").counts[0], 15);
        assert_eq!(of("A").counts[1], 12);
        assert!((of("A").share - 15.0 / 27.0).abs() < 1e-9);
        assert_eq!(of("B").counts[0], 12);
        assert_eq!(of("B").counts[1], 15);
        // C: last in every combination.
        assert_eq!(of("C").counts[2], 27);
        assert!((of("C").share - 1.0).abs() < 1e-9);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(html.contains("#1 (56%)"), "A's most likely rank: {html}");
        assert!(html.contains("#3 (100%)"), "C's rank: {html}");
        let _ = fs::remove_dir_all(&base);

        // Identical careers hold their rank in every combination.
        let (base2, runs2, _out2) = fixture("r64f6same");
        for (prefix, model, points) in [
            ("x1", "X", 50.0),
            ("x2", "X", 50.0),
            ("x3", "X", 50.0),
            ("y1", "Y", 40.0),
            ("y2", "Y", 40.0),
            ("y3", "Y", 40.0),
        ] {
            career(&runs2, prefix, model, [points / 4.0; 4], [0.01; 4]);
        }
        let cells2 = careers(runs2.to_str().unwrap());
        let groups2 = model_groups(&cells2);
        let stab2 = rank_stability(&cells2, &groups2).unwrap();
        let of2 = |m: &str| {
            let i = groups2.iter().position(|g| g.model == m).unwrap();
            &stab2[i]
        };
        assert_eq!(of2("X").counts[0], 9);
        assert!((of2("X").share - 1.0).abs() < 1e-9);
        assert_eq!(of2("Y").counts[1], 9);
        let _ = fs::remove_dir_all(&base2);

        // More than 100,000 combinations is not enumerated.
        let (base3, runs3, _out3) = fixture("r64f6many");
        for m in 0..6 {
            for c in 0..7 {
                career(
                    &runs3,
                    &format!("m{m}c{c}"),
                    &format!("model{m}"),
                    [1.0; 4],
                    [0.01; 4],
                );
            }
        }
        let cells3 = careers(runs3.to_str().unwrap());
        let groups3 = model_groups(&cells3);
        assert!(
            rank_stability(&cells3, &groups3).is_none(),
            "6 models x 7 careers is 117,649 combinations"
        );
        let _ = fs::remove_dir_all(&base3);
    }

    /// R64 F6: ticket agreement is over the 20 non-genesis tickets.
    #[test]
    fn r64_f6_ticket_agreement() {
        let (base, runs, out) = fixture("r64f6agree");
        career(&runs, "a", "m", [1.0; 4], [0.01; 4]);
        career(&runs, "b", "m", [1.0; 4], [0.01; 4]);
        career(&runs, "c", "m", [1.0; 4], [0.01; 4]);
        fail_ticket(&runs, "b", 5);
        fail_ticket(&runs, "c", 11);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(
            html.contains("agree 18/20"),
            "two of the 20 non-genesis tickets differ: {html}"
        );
        assert!(
            html.contains("differs: 5, 11"),
            "the differing tickets are listed: {html}"
        );
        assert!(
            html.contains("passed by 2/3"),
            "the strip shades by the share that passed: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R64 F6: the results page's head-to-head and within/between variance
    /// on the reviewer's 2026-11-06 12-career table.
    #[test]
    fn r64_f6_head_to_head_and_spread() {
        let (base, runs, out) = fixture("r64f6h2h");
        let table: [(&str, &str, &[f64]); 5] = [
            (
                "haiku",
                "openrouter/anthropic/claude-haiku-5.5",
                &[55.6, 57.6, 64.6],
            ),
            (
                "deepseek",
                "openrouter/deepseek/deepseek-v4.1-flash-1017",
                &[70.0, 63.0, 65.0],
            ),
            ("luna", "openrouter/openai/gpt-6-luna", &[27.4, 47.0]),
            ("qwen", "openrouter/qwen/qwen3.8-flash", &[37.0, 37.0]),
            ("mimo", "openrouter/xiaomi/mimo-v2.6-flash", &[67.0, 62.8]),
        ];
        for (prefix, model, points) in table {
            for (i, p) in points.iter().enumerate() {
                career(
                    &runs,
                    &format!("{prefix}{i}"),
                    model,
                    [p / 4.0; 4],
                    [0.01; 4],
                );
            }
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        let h2h = &html[pos(&html, "Head-to-head")..];
        let at = pos(
            h2h,
            "<td>openrouter/deepseek/deepseek-v4.1-flash-1017</td><td>openrouter/xiaomi/mimo-v2.6-flash</td>",
        );
        let row = &h2h[at..at + h2h[at..].find("</tr>").unwrap()];
        assert!(row.contains("mimo"), "deepseek vs mimo row: {row}");
        assert!(
            row.contains("67%"),
            "deepseek beats mimo in 4 of 6 pairs: {row}"
        );
        let var = &html[pos(&html, "Variance check")..];
        let at = pos(var, "<td>points</td>");
        let row = &var[at..at + var[at..].find("</tr>").unwrap()];
        assert!(row.contains("7.0"), "points within-model spread: {row}");
        assert!(row.contains("37.6"), "points between models: {row}");
        assert!(row.contains("5.4×"), "the bar ratio: {row}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R64 F6: each dot strip is inline SVG, one dot per career, with the
    /// values in its title.
    #[test]
    fn r64_f6_dot_strip_svg() {
        let (base, runs, out) = fixture("r64f6dots");
        career(&runs, "d1", "m", [2.5; 4], [0.01; 4]); // 10
        career(&runs, "d2", "m", [5.0; 4], [0.01; 4]); // 20
        career(&runs, "d3", "m", [7.5; 4], [0.01; 4]); // 30
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let at = pos(&html, "data-metric=\"points\"");
        let svg = &html[at..at + html[at..].find("</svg>").unwrap()];
        assert_eq!(
            svg.matches("<circle").count(),
            3,
            "one dot per career: {svg}"
        );
        assert!(
            svg.contains("<title>points: 30.0, 20.0, 10.0 · median 20.0</title>"),
            "the values are in the title: {svg}"
        );
        assert!(
            svg.contains("<polygon class=\"med\""),
            "median diamond: {svg}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R52 F6 (R47-1): the board hover shows a floored workload's 0.10x
    /// and reason, never its raw ratio or a guessed "not durable".
    #[test]
    fn r52_f6_floored_workload_says_why() {
        let (base, runs, out) = fixture("r52floor");
        career(&runs, "floor", "m", [1.0; 4], [0.01; 4]);
        set_career_perf(
            &runs,
            "floor",
            0.1,
            serde_json::json!([
                {"id": "W3", "metric": "latency", "reference": 1.0, "candidate": 0.04, "ratio": 64.06, "correct": false},
                {"id": "W4", "metric": "throughput", "reference": 100.0, "candidate": 40.0, "ratio": 0.4, "correct": false, "note": "not durable: 3 flushes for 10 commits"}
            ]),
        );
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        let w3_start = pos(&html, "W3:");
        let w4_start = pos(&html, "W4:");
        let w3 = &html[w3_start..w4_start];
        assert!(w3.contains("0.10×"), "W3 floor: {w3}");
        assert!(w3.contains("floored"), "W3 reason: {w3}");
        assert!(!w3.contains("durable"), "W3 guessed a cause: {w3}");
        assert!(!w3.contains("64.06×"), "W3 raw ratio leaked: {w3}");
        assert!(
            html.contains("not durable: 3 flushes for 10 commits"),
            "W4's recorded note must stay: {html}"
        );
        assert!(
            !html.contains("floored (wrong answer, not durable)"),
            "the old blanket wording is gone: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// R51 F5 (Q8 a): the results page names the snapshot date, says costs
    /// are base rates, and lists each ranked model's override.
    #[test]
    fn r51_f5_prices_row() {
        let (base, runs, out) = fixture("r51prices");
        for (prefix, model) in [
            ("haiku", "anthropic/claude-haiku-5.5"),
            ("luna", "openai/gpt-6-luna"),
            ("deepseek", "deepseek/deepseek-v4.1-flash-1017"),
            ("mimo", "xiaomi/mimo-v2.6-flash"),
            ("qwen", "qwen/qwen3.8-flash"),
        ] {
            career(&runs, prefix, model, [1.0; 4], [0.01; 4]);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(html.contains("<th>Prices</th>"), "{html}");
        assert!(html.contains("2026-10-28"), "{html}");
        assert!(html.contains("base rates per call"), "{html}");
        for needle in [
            "5× the prompt rate above 100k prompt tokens",
            "2× above 272k prompt tokens",
            "2× in weekday peak hours",
            "xiaomi/mimo-v2.6-flash — none",
            "qwen/qwen3.8-flash — none",
        ] {
            assert!(html.contains(needle), "missing {needle:?}: {html}");
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// R51 F4 (R44-14): the grid dates run from the earliest phase start
    /// of any ranked career to the latest finish, to the minute.
    #[test]
    fn r51_f4_grid_dates() {
        let (base, runs, out) = fixture("r51dates");
        career(&runs, "a", "m", [1.0; 4], [0.01; 4]);
        career(&runs, "b", "m", [1.0; 4], [0.01; 4]);
        let a = [
            (
                "2026-10-20T01:09:00.900460150+00:00",
                "2026-10-20T01:20:00.5+00:00",
            ),
            ("2026-10-20T01:30:00.2+00:00", "2026-10-20T01:50:00.2+00:00"),
            ("2026-10-20T02:00:00.2+00:00", "2026-10-20T02:30:00.2+00:00"),
            ("2026-10-20T03:00:00.2+00:00", "2026-10-20T04:00:00.1+00:00"),
        ];
        let b = [
            (
                "2026-10-20T00:30:00.123+00:00",
                "2026-10-20T00:45:00.9+00:00",
            ),
            ("2026-10-20T10:00:00.9+00:00", "2026-10-20T11:00:00.9+00:00"),
            ("2026-10-20T20:00:00.9+00:00", "2026-10-20T21:00:00.9+00:00"),
            (
                "2026-10-21T01:00:00.9+00:00",
                "2026-10-21T02:00:00.999+00:00",
            ),
        ];
        for (i, (start, end)) in a.iter().enumerate() {
            set_phase_times(&runs, "a", i as u32 + 1, start, end);
        }
        for (i, (start, end)) in b.iter().enumerate() {
            set_phase_times(&runs, "b", i as u32 + 1, start, end);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("results.html"));
        assert!(
            html.contains("2026-10-20 00:30 UTC → 2026-10-21 02:00 UTC"),
            "the grid dates must span the careers: {html}"
        );
        assert!(!html.contains("00:30:00"), "seconds leaked: {html}");
        assert!(!html.contains(".999"), "nanoseconds leaked: {html}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R51 F3 (R44-10): a rollup without effort renders no "high" and
    /// groups apart from a real high-effort career.
    #[test]
    fn r51_f3_unknown_effort_blank() {
        let (base, runs, out) = fixture("r51effort");
        career(&runs, "none", "effort-model", [1.0; 4], [0.01; 4]);
        for phase in 1..=4 {
            set_effort(&runs, "none", phase, None);
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(
            !html.contains("effort high") && !html.contains("high ·"),
            "an unknown effort must render nothing: {html}"
        );
        career(&runs, "high", "effort-model", [1.0; 4], [0.01; 4]);
        let cells = careers(runs.to_str().unwrap());
        let groups = model_groups(&cells);
        let efforts: Vec<&str> = groups.iter().map(|g| g.effort.as_str()).collect();
        assert!(efforts.contains(&""), "unknown effort group: {efforts:?}");
        assert!(efforts.contains(&"high"), "high effort group: {efforts:?}");
        assert_eq!(efforts.len(), 2, "same model, two efforts: {efforts:?}");
        let _ = fs::remove_dir_all(&base);
    }

    /// R47 F2: a career with a perf block shows a Perf column and its
    /// score; one without shows an em dash. The hover names each workload
    /// and flags a floored one; the methodology names the tie-break.
    #[test]
    fn r47_f2_perf_column() {
        let (base, runs, out) = fixture("r47perf");
        career(&runs, "perf", "perf-model", [1.0; 4], [0.01; 4]);
        set_career_perf(
            &runs,
            "perf",
            1.32,
            serde_json::json!([
                {"id": "W1", "metric": "latency", "reference": 1.0, "candidate": 0.76, "ratio": 1.32, "correct": true},
                {"id": "W4", "metric": "throughput", "reference": 100.0, "candidate": 40.0, "ratio": 0.4, "correct": false, "note": "not durable"}
            ]),
        );
        career(&runs, "plain", "plain-model", [1.0; 4], [0.01; 4]);
        let cells = careers(runs.to_str().unwrap());
        let perf = cells.iter().find(|c| c.model == "perf-model").unwrap();
        assert_eq!(perf.perf.as_ref().and_then(|p| p.score), Some(1.32));
        assert_eq!(perf.key.perf, Some(1.32));
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = read(&out.join("index.html"));
        assert!(html.contains("<th>Perf</th>"), "{html}");
        assert!(html.contains("1.32×"), "{html}");
        assert!(html.contains("W1: ref 1.000"), "{html}");
        assert!(html.contains("candidate 0.760"), "{html}");
        assert!(
            html.contains("0.10× · floored · not durable"),
            "the W4 floor and its recorded note: {html}"
        );
        let plain = &html[pos(&html, "plain-model")..];
        let row = &plain[..plain.find("</tr>").unwrap()];
        assert!(row.contains(">—<"), "a career with no perf shows —: {row}");
        assert!(
            html.contains("Performance section") && html.contains("score decides"),
            "methodology must name the perf tie-break: {html}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r41_f3_portal_is_the_sql_board() {
        let (base, runs, out) = fixture("r41portal");
        career(&runs, "one", "m", [1.0; 4], [0.01; 4]);
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        assert!(out.join("index.html").exists());
        assert!(out.join("results.html").exists());
        assert!(out.join("grid.json").exists());
        assert!(!out.join("runs").exists());
        assert!(!out.join("results.json").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn r41_f3_old_sql_row_renders() {
        let (base, runs, out) = fixture("r41old");
        career(&runs, "old", "old-model", [1.0; 4], [0.01; 4]);
        for phase in 1..=4 {
            let path = runs.join(format!("campaign_old_p{phase}/campaign.json"));
            let mut v: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            v["rules_version"] = serde_json::json!("2026-10-19.41");
            std::fs::write(&path, v.to_string()).unwrap();
        }
        export(runs.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        let html = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(html.contains("old-model"), "{html}");
        assert!(html.contains("older rule sets"), "{html}");
        let _ = fs::remove_dir_all(&base);
    }
}
